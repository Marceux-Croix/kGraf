//! What the elevated step writes outside the accounts: the undo record's
//! folder and file under ProgramData, and the deny entries that close the
//! folders every local user may write to the sandbox group.
//!
//! Every path is opened without following a reparse point
//! (`FILE_FLAG_OPEN_REPARSE_POINT`), and a folder is trusted only when its
//! owner is an administrator principal and no one else may change it
//! ([`super::super::acl::distrust`]). Writes go to a new file (`CREATE_NEW`)
//! that is then renamed over the old one: a rename replaces a link's name,
//! never its target, and a planted hard link is only unlinked.
//!
//! The deny entries are written with `SetKernelObjectSecurity` on a handle,
//! which sets the one folder's list and propagates nothing: the entries are
//! for that folder alone, and a call that propagates would walk a whole
//! drive.

use std::ffi::{OsStr, OsString, c_void};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    ERROR_SUCCESS, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, AddAce, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorControl, InitializeAcl, InitializeSecurityDescriptor, NO_INHERITANCE,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_AUTO_INHERITED,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetKernelObjectSecurity,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileDispositionInfo, GetDriveTypeW, GetFileInformationByHandle, GetLogicalDrives,
    GetVolumeInformationW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    OPEN_EXISTING, READ_CONTROL, SetFileInformationByHandle, WRITE_DAC,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_DENIED_ACE_TYPE, FILE_PERSISTENT_ACLS, SECURITY_DESCRIPTOR_REVISION,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_Public, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};

use super::super::acl::{AceView, CLOSED_FOLDER_DENY, Locations, closed_folders, distrust};
use crate::error::SandboxError;

fn fail(step: &'static str, why: impl std::fmt::Display) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: io::Error::other(why.to_string()),
    }
}

fn os_fail(step: &'static str, e: io::Error) -> SandboxError {
    SandboxError::PlatformSetup { step, source: e }
}

fn wide(p: &OsStr) -> Vec<u16> {
    p.encode_wide().chain(std::iter::once(0)).collect()
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

// ---- known folders ------------------------------------------------------------

/// A known folder's path from the shell, not from an environment variable
/// the caller could have set.
fn known_folder(id: &windows_sys::core::GUID) -> Result<PathBuf, SandboxError> {
    let mut raw: *mut u16 = ptr::null_mut();
    // SAFETY: `id` is a valid GUID; on success `raw` is a string the call
    // allocated with CoTaskMemAlloc, freed below (also on failure, as
    // documented).
    let hr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, ptr::null_mut(), &mut raw) };
    let path = if hr >= 0 && !raw.is_null() {
        let mut len = 0usize;
        // SAFETY: a NUL-terminated string the call returned.
        while unsafe { *raw.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` units were just read.
        let units = unsafe { std::slice::from_raw_parts(raw, len) };
        Ok(PathBuf::from(OsString::from_wide(units)))
    } else {
        Err(os_fail(
            "find a known folder",
            io::Error::from_raw_os_error(hr),
        ))
    };
    // SAFETY: frees what the call allocated; null is allowed.
    unsafe { CoTaskMemFree(raw as *const c_void) };
    path
}

/// `%ProgramData%`, from the shell.
pub fn program_data() -> Result<PathBuf, SandboxError> {
    known_folder(&FOLDERID_ProgramData)
}

// ---- handles that never follow a link ----------------------------------------

fn open_no_follow(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated; the handle, if any, is owned below.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle this code now owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

fn attributes(h: &OwnedHandle) -> io::Result<u32> {
    // SAFETY: zeroed is a valid value of this plain struct.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid handle and an output struct of the right type.
    if unsafe { GetFileInformationByHandle(h.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.dwFileAttributes)
}

fn is_not_found(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error().map(|c| c as u32),
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
    )
}

// ---- reading a security descriptor -------------------------------------------

fn sid_text(sid: PSID) -> Result<String, SandboxError> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID; `text` is allocated by the call and freed
    // by `Local`.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(os_fail("write a SID as text", io::Error::last_os_error()));
    }
    let _free = Local(text.cast());
    let mut len = 0usize;
    // SAFETY: a NUL-terminated string the call allocated.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read.
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, len)
    }))
}

/// The owner, the access list and its control bits of the object behind `h`.
struct Security {
    owner: PSID,
    dacl: *mut ACL,
    control: u16,
    _sd: Local,
}

fn security_of(h: &OwnedHandle, path: &Path) -> Result<Security, SandboxError> {
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: a valid handle opened with READ_CONTROL; the outputs point into
    // `sd`, which `Local` frees.
    let code = unsafe {
        GetSecurityInfo(
            h.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(SandboxError::Io {
            path: path.to_path_buf(),
            source: io::Error::from_raw_os_error(code as i32),
        });
    }
    let sd = Local(sd);
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: a valid descriptor.
    if unsafe { GetSecurityDescriptorControl(sd.0, &mut control, &mut revision) } == 0 {
        return Err(os_fail(
            "read a security descriptor's control bits",
            io::Error::last_os_error(),
        ));
    }
    Ok(Security {
        owner,
        dacl,
        control,
        _sd: sd,
    })
}

/// The entries of `acl` as the judgment in `acl.rs` reads them. An entry of
/// a type other than plain allow and deny (object and callback entries,
/// which file systems do not use) counts as a grant of everything to an
/// unknown principal, so it can only make a folder less trusted.
fn ace_views(acl: *const ACL) -> Result<Option<Vec<AceView>>, SandboxError> {
    if acl.is_null() {
        return Ok(None);
    }
    // SAFETY: `acl` points into a descriptor the caller holds.
    let count = unsafe { (*acl).AceCount };
    let mut out = Vec::with_capacity(usize::from(count));
    for i in 0..u32::from(count) {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `i` is below the entry count of a valid list.
        if unsafe { GetAce(acl, i, &mut ace) } == 0 {
            return Err(os_fail("read an access entry", io::Error::last_os_error()));
        }
        // SAFETY: every entry starts with a header.
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        let kind = u32::from(header.AceType);
        if kind == 0 || kind == ACCESS_DENIED_ACE_TYPE {
            // SAFETY: allow and deny entries share this layout.
            let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let sid = ptr::addr_of!(body.SidStart) as PSID;
            out.push(AceView {
                deny: kind == ACCESS_DENIED_ACE_TYPE,
                sid: sid_text(sid)?,
                mask: body.Mask,
            });
        } else {
            out.push(AceView {
                deny: false,
                sid: format!("an access entry of type {kind}"),
                mask: u32::MAX,
            });
        }
    }
    Ok(Some(out))
}

// ---- the undo record's folder ------------------------------------------------

/// Why the folder at `path` cannot hold the undo record, or `None` when it
/// can. `elevated_user` is the SID the elevated step runs as.
fn folder_distrust(path: &Path, elevated_user: &str) -> Result<Option<String>, SandboxError> {
    let h = open_no_follow(path, READ_CONTROL | FILE_READ_ATTRIBUTES).map_err(|source| {
        SandboxError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let attrs = attributes(&h).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if !reparse && attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Ok(Some("it is a file, not a folder".to_owned()));
    }
    let sec = security_of(&h, path)?;
    let owner = sid_text(sec.owner)?;
    let entries = ace_views(sec.dacl)?;
    Ok(distrust(&owner, entries.as_deref(), reparse, elevated_user))
}

fn refuse_folder(path: &Path, why: &str) -> SandboxError {
    fail(
        "check the undo record's folder",
        format!(
            "{} cannot hold the sandbox undo record: {why}. Another program may have made \
             it to redirect what setup writes there. Check what it holds, delete it, and run \
             the command again.",
            path.display()
        ),
    )
}

/// The folder `base\parts...`, made where missing with an access list only
/// administrators may change ([`super::super::acl::TRUSTED_FOLDER_SDDL`]),
/// and checked where it exists. Each part is checked in turn, so a link or
/// a folder another user made anywhere on the way is refused.
pub fn trusted_folder(
    base: &Path,
    parts: &[&str],
    elevated_user: &str,
) -> Result<PathBuf, SandboxError> {
    let sddl = wide(OsStr::new(super::super::acl::TRUSTED_FOLDER_SDDL));
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; the descriptor is allocated by the
    // call and freed by `Local`.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(os_fail(
            "build the undo record folder's access list",
            io::Error::last_os_error(),
        ));
    }
    let _sd = Local(sd);
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let mut path = base.to_path_buf();
    for part in parts {
        path.push(part);
        let name = wide(path.as_os_str());
        // SAFETY: `name` is NUL-terminated; `attrs` holds a valid descriptor.
        if unsafe { CreateDirectoryW(name.as_ptr(), &attrs) } == 0 {
            // SAFETY: reads the thread's last error.
            let code = unsafe { GetLastError() };
            if code != ERROR_ALREADY_EXISTS {
                return Err(SandboxError::Io {
                    path,
                    source: io::Error::from_raw_os_error(code as i32),
                });
            }
        }
        // Also a folder made just now: its owner follows the system's
        // policy for objects administrators create.
        if let Some(why) = folder_distrust(&path, elevated_user)? {
            return Err(refuse_folder(&path, &why));
        }
    }
    Ok(path)
}

/// [`trusted_folder`] without making anything: `None` when a part does not
/// exist (nothing was ever written there), an error when one exists and is
/// not trusted.
pub fn existing_trusted_folder(
    base: &Path,
    parts: &[&str],
    elevated_user: &str,
) -> Result<Option<PathBuf>, SandboxError> {
    let mut path = base.to_path_buf();
    for part in parts {
        path.push(part);
        match open_no_follow(&path, FILE_READ_ATTRIBUTES) {
            Ok(_) => {}
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(source) => return Err(SandboxError::Io { path, source }),
        }
        if let Some(why) = folder_distrust(&path, elevated_user)? {
            return Err(refuse_folder(&path, &why));
        }
    }
    Ok(Some(path))
}

// ---- files in a trusted folder -----------------------------------------------

/// Delete the name `path` without following it: a link is removed, not its
/// target. A missing name is not an error.
pub fn delete_no_follow(path: &Path) -> io::Result<()> {
    let h = match open_no_follow(path, DELETE | FILE_READ_ATTRIBUTES) {
        Ok(h) => h,
        Err(e) if is_not_found(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: a handle opened with DELETE and an input of the stated size.
    if unsafe {
        SetFileInformationByHandle(
            h.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            ptr::addr_of!(info).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn create_new(path: &Path) -> io::Result<std::fs::File> {
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated; the handle, if any, is owned below.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_WRITE,
            0,
            ptr::null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle this code now owns.
    Ok(std::fs::File::from(unsafe {
        OwnedHandle::from_raw_handle(h)
    }))
}

/// Replace the file `path` with `bytes`: written to a new `.tmp` file beside
/// it, then renamed over it. A `.tmp` name that exists already (a run cut
/// short) is deleted without being followed first.
pub fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = match create_new(&tmp) {
        Ok(f) => f,
        Err(e)
            if matches!(
                e.raw_os_error().map(|c| c as u32),
                Some(ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS)
            ) =>
        {
            delete_no_follow(&tmp)?;
            create_new(&tmp)?
        }
        Err(e) => return Err(e),
    };
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let (from, to) = (wide(tmp.as_os_str()), wide(path.as_os_str()));
    // SAFETY: both names are NUL-terminated.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The text of the file `path`, opened without following a link; `None`
/// when it does not exist. A link is an error.
pub fn read_no_follow(path: &Path) -> io::Result<Option<String>> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    {
        use std::os::windows::fs::MetadataExt;
        let meta = file.metadata()?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !meta.is_file() {
            return Err(io::Error::other(format!(
                "{} is a link or a folder, not the undo record",
                path.display()
            )));
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

// ---- folders closed to the sandbox group --------------------------------------

/// The machine's folders for [`closed_folders`]: fixed drives whose file
/// system keeps access lists, ProgramData, Public and the folders directly
/// in it, and the Windows folder.
pub fn locations() -> Result<Locations, SandboxError> {
    const DRIVE_FIXED: u32 = 3;
    let mut loc = Locations::default();
    // SAFETY: plain call.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", char::from(b'A' + i));
        let w = wide(OsStr::new(&root));
        // SAFETY: NUL-terminated root path.
        if unsafe { GetDriveTypeW(w.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let mut flags = 0u32;
        // SAFETY: NUL-terminated root; only the flags are requested.
        let ok = unsafe {
            GetVolumeInformationW(
                w.as_ptr(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut flags,
                ptr::null_mut(),
                0,
            )
        };
        if ok != 0 && flags & FILE_PERSISTENT_ACLS != 0 {
            loc.fixed_drive_roots.push(PathBuf::from(root));
        }
    }
    loc.program_data = Some(program_data()?);
    if let Ok(public) = known_folder(&FOLDERID_Public) {
        if let Ok(entries) = std::fs::read_dir(&public) {
            for entry in entries.flatten() {
                // `file_type` does not follow a link: a link here is left
                // alone, never closed through.
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    loc.public_children.push(entry.path());
                }
            }
            loc.public_children.sort();
        }
        loc.public = Some(public);
    }
    let mut buf = [0u16; 260];
    // SAFETY: `buf` has the stated length in characters.
    let n = unsafe {
        windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW(
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if n > 0 && (n as usize) < buf.len() {
        loc.windows = Some(PathBuf::from(OsString::from_wide(&buf[..n as usize])));
    }
    Ok(loc)
}

/// [`closed_folders`] of this machine that exist, as text for the record.
pub fn folders_to_close() -> Result<Vec<String>, SandboxError> {
    Ok(closed_folders(&locations()?)
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().into_owned())
        .collect())
}

struct Sid(Local);

fn parse_sid(text: &str) -> Result<Sid, SandboxError> {
    let w = wide(OsStr::new(text));
    let mut sid: PSID = ptr::null_mut();
    // SAFETY: NUL-terminated text; the SID is allocated by the call.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
        return Err(os_fail("read a SID from text", io::Error::last_os_error()));
    }
    Ok(Sid(Local(sid)))
}

const INHERITED_ACE: u8 = 0x10;
const INHERITANCE_BITS: u8 = 0x0F;

/// Whether `acl` holds the group's own deny entry: explicit, on the folder
/// alone, covering [`CLOSED_FOLDER_DENY`].
fn is_closing_entry(ace: *const c_void, sid: &Sid) -> bool {
    // SAFETY: every entry starts with a header.
    let header = unsafe { &*(ace as *const ACE_HEADER) };
    if u32::from(header.AceType) != ACCESS_DENIED_ACE_TYPE
        || header.AceFlags & (INHERITED_ACE | INHERITANCE_BITS) != 0
    {
        return false;
    }
    // SAFETY: a deny entry has the allow entry's layout.
    let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
    let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
    // SAFETY: both are valid SIDs.
    body.Mask & CLOSED_FOLDER_DENY == CLOSED_FOLDER_DENY
        && unsafe { EqualSid(ace_sid, sid.0.0) } != 0
}

fn entries(acl: *const ACL) -> Vec<*mut c_void> {
    if acl.is_null() {
        return Vec::new();
    }
    // SAFETY: a valid list the caller holds.
    let count = unsafe { (*acl).AceCount };
    (0..u32::from(count))
        .filter_map(|i| {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: `i` is below the entry count.
            (unsafe { GetAce(acl, i, &mut ace) } != 0).then_some(ace)
        })
        .collect()
}

fn open_folder_for_dacl(path: &Path) -> Result<OwnedHandle, SandboxError> {
    let h = open_no_follow(path, READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES).map_err(
        |source| SandboxError::Io {
            path: path.to_path_buf(),
            source,
        },
    )?;
    let attrs = attributes(&h).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 || attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(fail(
            "close a folder to the sandbox users",
            format!(
                "{} is a link or not a folder; refusing to change its access list",
                path.display()
            ),
        ));
    }
    Ok(h)
}

/// Write `acl` as the list of the folder behind `h` alone, keeping the
/// protected and auto-inherited bits it had (`control`).
fn set_folder_dacl(
    h: &OwnedHandle,
    acl: *const ACL,
    control: u16,
    path: &Path,
) -> Result<(), SandboxError> {
    // SAFETY: zeroed is a valid value of this plain struct, initialized next.
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let psd: PSECURITY_DESCRIPTOR = ptr::addr_of_mut!(sd).cast();
    let bits = SE_DACL_PROTECTED | SE_DACL_AUTO_INHERITED;
    // SAFETY: `psd` points to a descriptor this frame owns; `acl` is valid
    // for the call.
    let ok = unsafe {
        InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) != 0
            && SetSecurityDescriptorDacl(psd, 1, acl, 0) != 0
            && SetSecurityDescriptorControl(psd, bits, control & bits) != 0
            && SetKernelObjectSecurity(h.as_raw_handle() as HANDLE, DACL_SECURITY_INFORMATION, psd)
                != 0
    };
    if !ok {
        return Err(SandboxError::Io {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Whether the folder `path` holds the sandbox group's deny entry.
pub fn is_closed(path: &Path, group_sid: &str) -> Result<bool, SandboxError> {
    let sid = parse_sid(group_sid)?;
    let h = open_no_follow(path, READ_CONTROL).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let sec = security_of(&h, path)?;
    Ok(entries(sec.dacl)
        .into_iter()
        .any(|ace| is_closing_entry(ace, &sid)))
}

/// Deny the sandbox group (`group_sid`) the right to create or change
/// entries in the folder `path`, on that folder alone. Read back after.
pub fn close_folder(path: &Path, group_sid: &str) -> Result<(), SandboxError> {
    let sid = parse_sid(group_sid)?;
    let h = open_folder_for_dacl(path)?;
    let sec = security_of(&h, path)?;
    if entries(sec.dacl)
        .into_iter()
        .any(|ace| is_closing_entry(ace, &sid))
    {
        return Ok(());
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: CLOSED_FOLDER_DENY,
        grfAccessMode: DENY_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.0.0.cast(),
        },
    };
    let mut new_acl: *mut ACL = ptr::null_mut();
    // SAFETY: one valid entry and the folder's current list; the new list is
    // allocated by the call and freed by `Local`.
    let code = unsafe { SetEntriesInAclW(1, &entry, sec.dacl, &mut new_acl) };
    if code != ERROR_SUCCESS {
        return Err(SandboxError::Io {
            path: path.to_path_buf(),
            source: io::Error::from_raw_os_error(code as i32),
        });
    }
    let _free = Local(new_acl.cast());
    set_folder_dacl(&h, new_acl, sec.control, path)?;
    if !is_closed(path, group_sid)? {
        return Err(fail(
            "close a folder to the sandbox users",
            format!(
                "the deny entry for the sandbox group is missing on {} after it was written",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Take the sandbox group's deny entry off the folder `path` again, and
/// nothing else: every other entry keeps its place. A folder that no longer
/// exists is skipped.
pub fn reopen_folder(path: &Path, group_sid: &str) -> Result<(), SandboxError> {
    let sid = parse_sid(group_sid)?;
    let h = match open_folder_for_dacl(path) {
        Ok(h) => h,
        Err(SandboxError::Io { source, .. }) if is_not_found(&source) => return Ok(()),
        Err(e) => return Err(e),
    };
    let sec = security_of(&h, path)?;
    let all = entries(sec.dacl);
    let keep: Vec<*mut c_void> = all
        .iter()
        .copied()
        .filter(|ace| !is_closing_entry(*ace, &sid))
        .collect();
    if keep.len() == all.len() {
        return Ok(());
    }
    // SAFETY: a valid list; the new one is no larger.
    let (size, revision) = unsafe { (usize::from((*sec.dacl).AclSize), (*sec.dacl).AclRevision) };
    let mut buf = vec![0u32; size.div_ceil(4)];
    let acl: *mut ACL = buf.as_mut_ptr().cast();
    // SAFETY: `buf` holds `size` bytes, aligned for an ACL.
    if unsafe { InitializeAcl(acl, size as u32, u32::from(revision)) } == 0 {
        return Err(os_fail("build an access list", io::Error::last_os_error()));
    }
    for ace in keep {
        // SAFETY: every entry starts with a header that gives its size.
        let len = u32::from(unsafe { (*(ace as *const ACE_HEADER)).AceSize });
        // SAFETY: appends one valid entry of `len` bytes to a list with room.
        if unsafe { AddAce(acl, u32::from(revision), u32::MAX, ace, len) } == 0 {
            return Err(os_fail("build an access list", io::Error::last_os_error()));
        }
    }
    set_folder_dacl(&h, acl, sec.control, path)
}

/// Windows only; compile-checked on the development host. Each test works in
/// its own temporary folder, never under ProgramData.
#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> String {
        super::super::accounts::current_user_sid().expect("this process's SID")
    }

    fn mklink_junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .expect("run mklink");
        assert!(status.success(), "could not make the junction");
    }

    fn dacl_count(path: &Path) -> u16 {
        let h = open_no_follow(path, READ_CONTROL).unwrap();
        let sec = security_of(&h, path).unwrap();
        // SAFETY: a valid list from the descriptor `sec` holds.
        unsafe { (*sec.dacl).AceCount }
    }

    #[test]
    fn a_folder_everyone_may_change_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let folder = base.path().join("kGraf");
        std::fs::create_dir(&folder).unwrap();
        let status = std::process::Command::new("icacls.exe")
            .arg(&folder)
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success(), "icacls failed");
        let err = existing_trusted_folder(base.path(), &["kGraf"], &me()).unwrap_err();
        assert!(err.to_string().contains("S-1-1-0"), "{err}");
    }

    #[test]
    fn a_link_on_the_way_is_refused_and_nothing_is_made_through_it() {
        let base = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        mklink_junction(&base.path().join("kGraf"), elsewhere.path());
        let err = trusted_folder(base.path(), &["kGraf", "sandbox"], &me()).unwrap_err();
        assert!(err.to_string().contains("link"), "{err}");
        assert!(!elsewhere.path().join("sandbox").exists());
        let err = existing_trusted_folder(base.path(), &["kGraf"], &me()).unwrap_err();
        assert!(err.to_string().contains("link"), "{err}");
    }

    #[test]
    fn a_missing_folder_is_no_record_and_is_not_made() {
        let base = tempfile::tempdir().unwrap();
        let found = existing_trusted_folder(base.path(), &["kGraf", "sandbox"], &me()).unwrap();
        assert_eq!(found, None);
        assert!(!base.path().join("kGraf").exists());
    }

    #[test]
    fn a_planted_temporary_name_is_replaced_and_its_target_kept() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        let record = dir.path().join("record.txt");
        // A hard link under the temporary name: writing through it would
        // rewrite the victim.
        std::fs::hard_link(&victim, record.with_extension("tmp")).unwrap();
        replace_file(&record, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&record).unwrap(), "first");
        // A junction under the temporary name is removed, not entered.
        mklink_junction(&record.with_extension("tmp"), elsewhere.path());
        replace_file(&record, b"second").unwrap();
        assert_eq!(read_no_follow(&record).unwrap().as_deref(), Some("second"));
        assert!(victim.is_file());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_link_in_place_of_the_record_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        mklink_junction(&dir.path().join("record.txt"), elsewhere.path());
        assert!(read_no_follow(&dir.path().join("record.txt")).is_err());
    }

    /// A folder this code makes is trusted by the same check. Needs an
    /// elevated process (the access list names Administrators as owner); a
    /// run that is not elevated says so and checks nothing.
    #[test]
    fn a_folder_made_here_passes_its_own_check() {
        let elevated = std::process::Command::new("net.exe")
            .arg("session")
            .output()
            .is_ok_and(|o| o.status.success());
        if !elevated {
            println!("not run: needs an elevated process");
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let made = trusted_folder(base.path(), &["kGraf", "sandbox"], &me()).unwrap();
        assert_eq!(made, base.path().join("kGraf").join("sandbox"));
        // Again: it exists now and is checked, not made.
        trusted_folder(base.path(), &["kGraf", "sandbox"], &me()).unwrap();
    }

    #[test]
    fn a_closed_folder_gains_one_entry_and_loses_exactly_it_again() {
        // A SID no token holds, so the deny changes no one's access here.
        const GROUP: &str = "S-1-5-21-1-2-3-4";
        let dir = tempfile::tempdir().unwrap();
        let before = dacl_count(dir.path());
        assert!(!is_closed(dir.path(), GROUP).unwrap());
        close_folder(dir.path(), GROUP).unwrap();
        close_folder(dir.path(), GROUP).unwrap();
        assert!(is_closed(dir.path(), GROUP).unwrap());
        assert_eq!(dacl_count(dir.path()), before + 1, "written once");
        reopen_folder(dir.path(), GROUP).unwrap();
        assert!(!is_closed(dir.path(), GROUP).unwrap());
        assert_eq!(dacl_count(dir.path()), before);
        // A folder that is gone is skipped; a link is refused.
        reopen_folder(&dir.path().join("gone"), GROUP).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        mklink_junction(&link, elsewhere.path());
        assert!(close_folder(&link, GROUP).is_err());
        assert!(!is_closed(elsewhere.path(), GROUP).unwrap());
    }

    #[test]
    fn this_machine_has_folders_to_close() {
        let folders = folders_to_close().unwrap();
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(
            folders
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&format!("{system}\\"))),
            "{folders:?}"
        );
        println!("folders setup closes to the sandbox users: {folders:?}");
    }
}
