//! The Windows calls behind the fallback grant on the object directory of the
//! owner's session: which session this process runs in, the directory's
//! security descriptor as a measurement line, and adding and taking off the
//! one access-allowed entry for the sandbox group. The rights themselves, the
//! directory's path and the words kGraf says about the grant are in
//! [`super::super::named_object_grant`], which every host tests.
//!
//! **The grant does not survive a logon.** Windows makes
//! `\Sessions\<n>\BaseNamedObjects` again every time the owner signs in, with
//! the access list the system gives it, so the entry [`grant`] writes is gone
//! at the owner's next sign-out whether or not [`revoke`] ran. That is not a
//! reason to leave it: `kgraf sandbox remove` still takes it off, because a
//! removal must leave nothing of kGraf's behind on a machine that stays
//! signed in, and because the undo record may be read on a machine that has
//! not signed out since setup.
//!
//! **Who may write this list.** The directory's access list is the system's,
//! and the account that runs `kgraf sandbox setup` may not be admitted to
//! change it. The elevated setup step runs in the owner's own session, which
//! is why it can open this directory at all; its administrator rights are not
//! by themselves a right on the object. A refused open therefore says so
//! rather than reporting a missing directory.
//!
//! Nothing here decides anything: the mask comes from the policy file and the
//! choice between this fallback and kGraf's launcher service is made in
//! [`super::super::session0`].

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::NtOpenDirectoryObject;
use windows_sys::Win32::Foundation::{
    ERROR_SUCCESS, HLOCAL, LocalFree, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
    STATUS_ACCESS_DENIED, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSidToSidW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, SDDL_REVISION_1, SetEntriesInAclW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, AddAce, DACL_SECURITY_INFORMATION, EqualSid,
    GROUP_SECURITY_INFORMATION, GetAce, GetKernelObjectSecurity, GetSecurityDescriptorDacl,
    InitializeAcl, InitializeSecurityDescriptor, LABEL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_DESCRIPTOR,
    SetKernelObjectSecurity, SetSecurityDescriptorDacl,
};
use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION,
};

use super::super::named_object_grant::{GRANT_MASK, directory_path};
use crate::error::SandboxError;

/// `STATUS_SUCCESS` and every other status that is not an error has the sign
/// bit clear.
fn ok(status: NTSTATUS) -> bool {
    status >= 0
}

/// A failure of an NT call: the call by name, the `NTSTATUS`, the Win32 error
/// it maps to, and a plain reason for the codes whose cause is known here.
///
/// The reason is part of the message and not a separate field because
/// [`SandboxError::PlatformSetup`] carries one `io::Error`, and the mapped
/// Win32 code alone ("Access is denied") does not say whose access list
/// refused what.
fn nt_fail(step: &'static str, call: &str, status: NTSTATUS, path: &str) -> SandboxError {
    // SAFETY: a plain mapping of a status code to a Win32 code; no pointers.
    let win32 = unsafe { RtlNtStatusToDosError(status) };
    let reason = match status {
        STATUS_ACCESS_DENIED => format!(
            ". The account that runs the setup may not change the access list of {path}: that \
             directory's list is the system's, and administrator rights are not by themselves a \
             right on the object. The owner's own elevated setup process runs in the owner's \
             session, which is why it can open this directory at all"
        ),
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND => format!(
            ". There is no object directory {path}: a session that has signed out has none, and \
             session 0 uses the machine-wide \\BaseNamedObjects instead"
        ),
        _ => String::new(),
    };
    SandboxError::PlatformSetup {
        step,
        source: io::Error::other(format!(
            "{call} on {path} returned NTSTATUS {:#010x} (Windows error {win32}){reason}",
            status as u32
        )),
    }
}

/// A failure of a Win32 call: the call by name and the thread's last error.
fn win32_fail(step: &'static str, call: &str) -> SandboxError {
    let last = io::Error::last_os_error();
    SandboxError::PlatformSetup {
        step,
        source: io::Error::other(format!("{call} failed: {last}")),
    }
}

/// A failure this code found itself, with the call that produced what it
/// judged.
fn fail(step: &'static str, why: impl std::fmt::Display) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: io::Error::other(why.to_string()),
    }
}

/// Memory a Windows call allocated with `LocalAlloc`, freed on drop.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a call documented to allocate
            // with LocalAlloc, and is freed once.
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }
}

// ---- which session this process runs in ---------------------------------------

/// The Windows session this process runs in (`ProcessIdToSessionId`).
///
/// **This is the first thing a Windows run must report.** The two launch paths
/// are told apart by the object directory they reach: kGraf's launcher service
/// runs a command in session 0, below the machine-wide `\BaseNamedObjects`,
/// and the fallback grant admits the sandbox group to
/// `\Sessions\<n>\BaseNamedObjects`, the directory of the owner's session. If
/// the process that runs kGraf is itself in session 0, as it can be on a
/// hosted runner, then both paths name the same machine-wide directory, and
/// every later measurement must be read that way: a command that makes a named
/// object there proves nothing about a command in an interactive session, and
/// a grant written there is a grant on the machine-wide directory, not on a
/// session's own.
pub fn session_of_current_process() -> Result<u32, SandboxError> {
    let mut session = 0u32;
    // SAFETY: this process's own ID and an out pointer to a local.
    let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
    if ok == 0 {
        return Err(win32_fail(
            "read this process's Windows session",
            "ProcessIdToSessionId",
        ));
    }
    Ok(session)
}

// ---- opening the directory ----------------------------------------------------

/// Open the object directory of `session` with `access`
/// (`NtOpenDirectoryObject`, ntdll). `step` names the step of setup or removal
/// the open belongs to, so a refusal says which action it stopped.
///
/// `OBJ_CASE_INSENSITIVE` is set, as `kernelbase.dll` sets it for its own open
/// of this directory; the object manager's root is not case sensitive by
/// default, and a name that differed only by case would otherwise not be
/// found.
fn open_directory(
    step: &'static str,
    session: u32,
    access: u32,
) -> Result<(OwnedHandle, String), SandboxError> {
    let path = directory_path(session);
    let mut wide: Vec<u16> = OsStr::new(&path).encode_wide().collect();
    // A UNICODE_STRING carries a byte length, not a terminator; the path is
    // far below the limit, so this cannot fail in practice and is still
    // checked rather than truncated.
    let bytes = u16::try_from(wide.len() * 2).map_err(|_| {
        fail(
            step,
            format!("the object directory name {path} is too long"),
        )
    })?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..Default::default()
    };
    let mut handle: windows_sys::Win32::Foundation::HANDLE = ptr::null_mut();
    // SAFETY: `name` borrows `wide`, and both outlive the call; `handle` is an
    // out pointer to a local, owned below on success.
    let status = unsafe { NtOpenDirectoryObject(&mut handle, access, &attributes) };
    if !ok(status) {
        return Err(nt_fail(step, "NtOpenDirectoryObject", status, &path));
    }
    // SAFETY: a fresh handle this code now owns; an object directory handle is
    // closed by CloseHandle, which `OwnedHandle` calls on drop.
    let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
    Ok((owned, path))
}

/// The security descriptor of the object behind `h`, in its own buffer
/// (`GetKernelObjectSecurity`, twice: once for the size, once for the bytes).
///
/// `GetSecurityInfo` with `SE_KERNEL_OBJECT` would do the same, and this call
/// is used instead because it writes into a buffer this frame owns, with no
/// `LocalFree` to get right, and because the size query is the documented way
/// to learn how large the descriptor is.
///
/// The buffer is a `Vec<u32>`, not a `Vec<u8>`: a security descriptor holds
/// aligned fields, and the pointer handed back to Windows must be aligned for
/// them. A byte vector is aligned to one byte only.
fn descriptor_of(
    step: &'static str,
    h: &OwnedHandle,
    what: u32,
    path: &str,
) -> Result<Vec<u32>, SandboxError> {
    let mut needed = 0u32;
    // SAFETY: a size query: no buffer, length 0, and `needed` an out pointer
    // to a local. The call is documented to fail with
    // ERROR_INSUFFICIENT_BUFFER here and to set `needed`.
    unsafe { GetKernelObjectSecurity(h.as_raw_handle(), what, ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(win32_fail(step, "GetKernelObjectSecurity (size query)"));
    }
    // A descriptor is read as bytes and handed to Windows calls that expect
    // its own alignment, so the buffer is allocated as u32 and cast.
    let mut words = vec![0u32; (needed as usize).div_ceil(4)];
    let sd: PSECURITY_DESCRIPTOR = words.as_mut_ptr().cast();
    // SAFETY: `words` holds at least `needed` bytes, aligned for a descriptor.
    let got = unsafe { GetKernelObjectSecurity(h.as_raw_handle(), what, sd, needed, &mut needed) };
    if got == 0 {
        return Err(fail(
            step,
            format!(
                "GetKernelObjectSecurity on {path} failed: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(words)
}

/// The access list of the descriptor in `sd`, or `None` for a list that is
/// not present, which grants every principal every right. The pointer borrows
/// `sd`.
///
/// The two cases are kept apart, rather than both reported as an empty list,
/// because they mean opposite things and each caller must decide for itself.
/// A read that finds no list has nothing to take off ([`revoke`]) and holds
/// no grant of kGraf's ([`is_granted`]), but a **write** against no list
/// would replace "everyone has everything" with a list that holds kGraf's one
/// entry and nothing else, and so would lock the owner's own session out of
/// its object directory ([`grant`] refuses).
fn dacl_of(step: &'static str, sd: &[u32]) -> Result<Option<*mut ACL>, SandboxError> {
    let psd: PSECURITY_DESCRIPTOR = sd.as_ptr().cast_mut().cast();
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    // SAFETY: `psd` points at a descriptor this frame holds; the three outputs
    // are pointers to locals.
    let got = unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) };
    if got == 0 {
        return Err(win32_fail(step, "GetSecurityDescriptorDacl"));
    }
    // `present == 0` is a list that is not there at all. `present != 0` with a
    // null pointer is a NULL list, which Windows reads the same way: every
    // principal holds every right.
    if present == 0 || dacl.is_null() {
        return Ok(None);
    }
    Ok(Some(dacl))
}

/// A SID read from its text form (`ConvertStringSidToSidW`), freed on drop.
struct Sid(Local);

fn parse_sid(step: &'static str, text: &str) -> Result<Sid, SandboxError> {
    let w: Vec<u16> = OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sid: PSID = ptr::null_mut();
    // SAFETY: NUL-terminated text; the SID is allocated by the call and freed
    // by `Local`.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
        return Err(fail(
            step,
            format!(
                "ConvertStringSidToSidW could not read {text} as a SID: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(Sid(Local(sid)))
}

/// Every entry of `acl`, in list order.
fn entries(acl: *const ACL) -> Vec<*mut c_void> {
    if acl.is_null() {
        return Vec::new();
    }
    // SAFETY: a valid list the caller holds.
    let count = unsafe { (*acl).AceCount };
    (0..u32::from(count))
        .filter_map(|i| {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: `i` is below the entry count of a valid list.
            (unsafe { GetAce(acl, i, &mut ace) } != 0).then_some(ace)
        })
        .collect()
}

/// Whether `ace` is kGraf's grant: a plain access-allowed entry for `sid` with
/// exactly `mask` and no inheritance flag.
///
/// The mask is matched exactly, not as a subset, and the flags must be zero.
/// A caller that takes an entry off names the mask the entry was written with,
/// from the record, so a later version of kGraf that grants other rights
/// neither leaves an older entry behind nor takes off an entry wider than the
/// one it wrote. An object directory does propagate an inheritable entry to
/// the objects made in it, so an entry of kGraf's that carried an inheritance
/// flag would reach objects it never meant to; none is ever written, and an
/// entry that carries one is not recognized as kGraf's.
fn is_grant_entry(ace: *const c_void, sid: &Sid, mask: u32) -> bool {
    // SAFETY: every entry starts with a header.
    let header = unsafe { &*(ace as *const ACE_HEADER) };
    if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != 0 {
        return false;
    }
    // SAFETY: a plain allow entry has this layout: header, mask, then the SID.
    let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
    let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
    // SAFETY: both are valid SIDs.
    body.Mask == mask && unsafe { EqualSid(ace_sid, sid.0.0) } != 0
}

/// Every entry of `acl` that names `sid`, as its entry type, its flags and its
/// mask. For the refusal in [`grant`]: an entry for that SID which is not the
/// one kGraf writes must stop the write, not be merged with it.
fn entries_for(acl: *const ACL, sid: &Sid) -> Vec<(u8, u8, u32)> {
    entries(acl)
        .into_iter()
        .filter_map(|ace| {
            // SAFETY: every entry starts with a header.
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            let kind = header.AceType;
            // Only the plain allow and deny entries put their SID at this
            // offset; another type is reported with no SID match rather than
            // read at the wrong bytes.
            if u32::from(kind) != ACCESS_ALLOWED_ACE_TYPE
                && u32::from(kind) != ACCESS_DENIED_ACE_TYPE
            {
                return None;
            }
            // SAFETY: both of those types have this layout: header, mask, SID.
            let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
            // SAFETY: both are valid SIDs.
            (unsafe { EqualSid(ace_sid, sid.0.0) } != 0).then_some((
                kind,
                header.AceFlags,
                body.Mask,
            ))
        })
        .collect()
}

/// Write `acl` as the access list of the object behind `h`, and nothing else
/// of its security (`SetKernelObjectSecurity` with
/// `DACL_SECURITY_INFORMATION`). `h` needs `WRITE_DAC`.
///
/// The descriptor is built here rather than reused from the read: a descriptor
/// Windows returned is self-relative, and its list cannot be replaced in
/// place. No owner, group or label is written, so nothing but the list
/// changes, and no control bit is set: an object directory's list takes part
/// in no automatic inheritance of the kind a file system list does, so the
/// protected and auto-inherited bits a folder write must carry
/// (`super::super::acl::lone_write_control`) have nothing to carry here.
fn write_dacl(
    step: &'static str,
    h: &OwnedHandle,
    acl: *const ACL,
    path: &str,
) -> Result<(), SandboxError> {
    // SAFETY: zeroed is a valid value of this plain struct, initialized next.
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let psd: PSECURITY_DESCRIPTOR = ptr::addr_of_mut!(sd).cast();
    // SAFETY: `psd` points at a descriptor this frame owns, and `acl` is valid
    // for the call; no owner, group or SACL is written.
    let written = unsafe {
        InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) != 0
            && SetSecurityDescriptorDacl(psd, 1, acl, 0) != 0
            && SetKernelObjectSecurity(h.as_raw_handle(), DACL_SECURITY_INFORMATION, psd) != 0
    };
    if written {
        return Ok(());
    }
    let last = io::Error::last_os_error();
    Err(fail(
        step,
        format!("SetKernelObjectSecurity on {path} failed: {last}"),
    ))
}

// ---- the measurement line -----------------------------------------------------

/// The security descriptor of the session's object directory in SDDL, for a
/// measurement line: who owns it, who is admitted to it and with what, and
/// its integrity label.
///
/// Opened with `READ_CONTROL` alone, so a reading needs no right to change
/// anything. Calls: `NtOpenDirectoryObject` (ntdll),
/// `GetKernelObjectSecurity`, `ConvertSecurityDescriptorToStringSecurityDescriptorW`.
pub fn read_sddl(session: u32) -> Result<String, SandboxError> {
    const STEP: &str = "read the session object directory's access list";
    let (h, path) = open_directory(STEP, session, READ_CONTROL)?;
    let what = OWNER_SECURITY_INFORMATION
        | GROUP_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | LABEL_SECURITY_INFORMATION;
    let sd = descriptor_of(STEP, &h, what, &path)?;
    let psd: PSECURITY_DESCRIPTOR = sd.as_ptr().cast_mut().cast();
    let mut text: *mut u16 = ptr::null_mut();
    let mut units = 0u32;
    // SAFETY: `psd` points at a descriptor this frame holds; the text is
    // allocated by the call and freed by `Local`.
    let got = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            psd,
            SDDL_REVISION_1,
            what,
            &mut text,
            &mut units,
        )
    };
    if got == 0 {
        return Err(win32_fail(
            STEP,
            "ConvertSecurityDescriptorToStringSecurityDescriptorW",
        ));
    }
    let _free = Local(text.cast());
    let units = units as usize;
    // SAFETY: the call wrote `units` units, not counting the terminator.
    let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, units) });
    Ok(sddl.trim_end_matches('\0').to_owned())
}

// ---- the grant ----------------------------------------------------------------

/// Admit the sandbox group (`group_sid`) to the object directory of `session`
/// with [`GRANT_MASK`], and return the mask that is on the directory.
///
/// One access-allowed entry is added and every existing entry is kept. The
/// entry carries no inheritance flag. Where an entry for that SID with that
/// mask is already there, nothing is written and the mask is returned, so a
/// second setup is the same as the first, as every other step of this setup
/// is.
///
/// Opened with `READ_CONTROL | WRITE_DAC`. Calls:
/// `NtOpenDirectoryObject` (ntdll), `GetKernelObjectSecurity`,
/// `GetSecurityDescriptorDacl`, `ConvertStringSidToSidW`, `SetEntriesInAclW`,
/// `SetKernelObjectSecurity`. A refused open names the account that runs the
/// setup as the cause and says why that account can try at all.
///
/// An entry for that SID which is **not** the one kGraf writes stops the
/// write. `SetEntriesInAclW` with `GRANT_ACCESS` would fold the new rights
/// into that entry, and the directory would then carry a mask no record of
/// kGraf's names, which a removal could not take off exactly.
pub fn grant(session: u32, group_sid: &str) -> Result<u32, SandboxError> {
    const STEP: &str = "admit the sandbox group to the session object directory";
    let sid = parse_sid(STEP, group_sid)?;
    let (h, path) = open_directory(STEP, session, READ_CONTROL | WRITE_DAC)?;
    let sd = descriptor_of(STEP, &h, DACL_SECURITY_INFORMATION, &path)?;
    // No list means every principal holds every right. kGraf does not write
    // the first list on this directory: `SetEntriesInAclW` against no list
    // builds one that holds kGraf's entry alone, and installing that would
    // take the owner's own session out of its own object directory, which
    // every program of that session opens. A directory in that state is not
    // what this code was written against, so it says so and stops.
    let dacl = dacl_of(STEP, &sd)?.ok_or_else(|| {
        fail(
            STEP,
            format!(
                "{path} has no access list, so every principal already holds every right on it \
                 and the sandbox group needs no grant. kGraf will not write the first access \
                 list of that directory: the list it would write holds its own entry alone, and \
                 the owner's own session would lose the directory every one of its programs \
                 opens"
            ),
        )
    })?;
    if entries(dacl)
        .into_iter()
        .any(|ace| is_grant_entry(ace, &sid, GRANT_MASK))
    {
        return Ok(GRANT_MASK);
    }
    let others = entries_for(dacl, &sid);
    if !others.is_empty() {
        let found: Vec<String> = others
            .iter()
            .map(|(kind, flags, mask)| {
                format!("type {kind:#04x}, flags {flags:#04x}, access {mask:#010x}")
            })
            .collect();
        return Err(fail(
            STEP,
            format!(
                "{path} already holds an entry for {group_sid} that kGraf did not write \
                 ({}); refusing to add rights to it, because the two would be folded into one \
                 entry that no record names and a removal could not take off exactly",
                found.join("; ")
            ),
        ));
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GRANT_MASK,
        grfAccessMode: GRANT_ACCESS,
        // An object directory propagates an inheritable entry to what is made
        // in it; this entry is for the directory alone.
        grfInheritance: 0,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.0.0.cast(),
        },
    };
    let mut new_acl: *mut ACL = ptr::null_mut();
    // SAFETY: one valid entry and the directory's current list, which `sd`
    // holds; the new list is allocated by the call and freed by `Local`.
    let code = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut new_acl) };
    if code != ERROR_SUCCESS {
        return Err(fail(
            STEP,
            format!(
                "SetEntriesInAclW on {path} failed: {}",
                io::Error::from_raw_os_error(code as i32)
            ),
        ));
    }
    let _free = Local(new_acl.cast());
    write_dacl(STEP, &h, new_acl, &path)?;
    // Read back: only the directory's own list says what it holds, and this
    // list is the system's, which another writer may have replaced between the
    // read and the write.
    if !is_granted(session, group_sid, GRANT_MASK)? {
        return Err(fail(
            STEP,
            format!(
                "the entry for the sandbox group is missing on {path} after it was written; \
                 no sandboxed command can make a named object there"
            ),
        ));
    }
    Ok(GRANT_MASK)
}

/// Whether the object directory of `session` holds an access-allowed entry
/// for `group_sid` with exactly `mask` and no inheritance flag. Opened with
/// `READ_CONTROL`, so a status report needs no right to change anything.
pub fn is_granted(session: u32, group_sid: &str, mask: u32) -> Result<bool, SandboxError> {
    const STEP: &str = "read the session object directory's access list";
    let sid = parse_sid(STEP, group_sid)?;
    let (h, path) = open_directory(STEP, session, READ_CONTROL)?;
    let sd = descriptor_of(STEP, &h, DACL_SECURITY_INFORMATION, &path)?;
    // No list holds no entry of kGraf's, so the answer is no.
    let dacl = dacl_of(STEP, &sd)?.unwrap_or(ptr::null_mut());
    Ok(entries(dacl)
        .into_iter()
        .any(|ace| is_grant_entry(ace, &sid, mask)))
}

/// Take exactly the entry that names `group_sid` with `mask` off the object
/// directory of `session`, and nothing else. `false` when it was not there:
/// an entry that is already gone is a removal that finished, and a removal
/// must always finish.
///
/// Every other entry keeps its place and its bytes: the new list is built from
/// the entries that are kept, in their order, so an entry another program
/// wrote is neither rewritten nor reordered. `mask` comes from the record of
/// what was written, so this never takes off a wider entry than kGraf made.
///
/// Opened with `READ_CONTROL | WRITE_DAC`. Calls: `NtOpenDirectoryObject`
/// (ntdll), `GetKernelObjectSecurity`, `GetSecurityDescriptorDacl`,
/// `ConvertStringSidToSidW`, `InitializeAcl`, `AddAce`,
/// `SetKernelObjectSecurity`.
pub fn revoke(session: u32, group_sid: &str, mask: u32) -> Result<bool, SandboxError> {
    const STEP: &str = "take the sandbox group off the session object directory";
    let sid = parse_sid(STEP, group_sid)?;
    let (h, path) = open_directory(STEP, session, READ_CONTROL | WRITE_DAC)?;
    let sd = descriptor_of(STEP, &h, DACL_SECURITY_INFORMATION, &path)?;
    // No list holds no entry of kGraf's, so there is nothing to take off, and
    // a removal that has nothing to do has finished.
    let Some(dacl) = dacl_of(STEP, &sd)? else {
        return Ok(false);
    };
    let all = entries(dacl);
    let keep: Vec<*mut c_void> = all
        .iter()
        .copied()
        .filter(|ace| !is_grant_entry(*ace, &sid, mask))
        .collect();
    if keep.len() == all.len() {
        return Ok(false);
    }
    // SAFETY: a valid list; the new one holds a subset of its entries, so the
    // old size is room enough.
    let (size, revision) = unsafe { (usize::from((*dacl).AclSize), (*dacl).AclRevision) };
    let mut words = vec![0u32; size.div_ceil(4)];
    let new_acl: *mut ACL = words.as_mut_ptr().cast();
    // SAFETY: `words` holds `size` bytes, aligned for an ACL.
    if unsafe { InitializeAcl(new_acl, size as u32, u32::from(revision)) } == 0 {
        return Err(win32_fail(STEP, "InitializeAcl"));
    }
    for ace in keep {
        // SAFETY: every entry starts with a header that gives its size.
        let len = u32::from(unsafe { (*(ace as *const ACE_HEADER)).AceSize });
        // SAFETY: appends one valid entry of `len` bytes to a list with room.
        if unsafe { AddAce(new_acl, u32::from(revision), u32::MAX, ace, len) } == 0 {
            return Err(win32_fail(STEP, "AddAce"));
        }
    }
    write_dacl(STEP, &h, new_acl, &path)?;
    if is_granted(session, group_sid, mask)? {
        return Err(fail(
            STEP,
            format!(
                "the entry for the sandbox group is still on {path} after it was taken off; a \
                 sandboxed command can still put a named object in the directory the owner's \
                 own programs read"
            ),
        ));
    }
    Ok(true)
}
