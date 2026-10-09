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
//! The deny entries are written with `SetSecurityInfo` on a handle opened
//! with `MAXIMUM_ALLOWED`, which keeps the write on the one folder: the
//! entries are for that folder alone, and a call that propagates would walk
//! a whole drive. [`set_folder_dacl`] quotes the pages it rests on.

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
    SE_FILE_OBJECT, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, AddAce, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    InitializeAcl, NO_INHERITANCE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileDispositionInfo, GetDriveTypeW, GetFileInformationByHandle, GetLogicalDrives,
    GetVolumeInformationW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    OPEN_EXISTING, READ_CONTROL, SetFileInformationByHandle,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_DENIED_ACE_TYPE, FILE_PERSISTENT_ACLS, MAXIMUM_ALLOWED,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_Public, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};

use super::super::acl::{
    ACE_INHERITANCE_FLAGS, AceView, CLOSED_FOLDER_DENY, CLOSED_FOLDER_FLAGS, Locations,
    closed_folders, distrust,
};
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

fn open_no_follow_shared(path: &Path, access: u32, share: u32) -> io::Result<OwnedHandle> {
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated; the handle, if any, is owned below.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            share,
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

fn open_no_follow(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    open_no_follow_shared(
        path,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )
}

/// A handle that holds the folder `path` where it is: `FILE_SHARE_DELETE` is
/// left out and `FILE_LIST_DIRECTORY | FILE_TRAVERSE` is asked for, which
/// take part in sharing checks, so no other process may delete or rename the
/// folder while the handle lives.
///
/// The step checks a folder and then writes through it by name, which
/// resolves the path again; without this handle the folder could have been
/// renamed away between the two, and the write would land in whatever took
/// its name. The upstream Windows backend states the same rule: keep the
/// handle through the security changes, and leave `FILE_SHARE_DELETE` out
/// while asking for access that takes part in sharing checks.
fn open_held(path: &Path) -> io::Result<OwnedHandle> {
    const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
    const FILE_TRAVERSE: u32 = 0x0000_0020;
    open_no_follow_shared(
        path,
        FILE_LIST_DIRECTORY | FILE_TRAVERSE | FOLDER_CHECK_ACCESS,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
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

/// The owner and the access list of the object behind `h`.
///
/// The control bits are not read: the write in [`set_folder_dacl`] leaves
/// the folder's protection exactly as Windows found it, so this code never
/// has to carry those bits from the read to the write.
struct Security {
    owner: PSID,
    dacl: *mut ACL,
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
    Ok(Security {
        owner,
        dacl,
        _sd: Local(sd),
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

/// The access list of the file or folder at `path`, as the judgments in
/// `acl.rs` read it; `None` for a null list, which grants everyone
/// everything. Read without following a link.
pub fn dacl_of(path: &Path) -> Result<Option<Vec<AceView>>, SandboxError> {
    let h = open_no_follow(path, READ_CONTROL).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let sec = security_of(&h, path)?;
    ace_views(sec.dacl)
}

// ---- the undo record's folder ------------------------------------------------

/// The access a folder check needs: its attributes and its security.
const FOLDER_CHECK_ACCESS: u32 = READ_CONTROL | FILE_READ_ATTRIBUTES;

/// Why the folder at `path`, which the caller holds open, cannot hold the
/// undo record, or `None` when it can. `elevated_user` is the SID the
/// elevated step runs as. The caller opens the folder once and keeps the
/// handle ([`open_held`]), so the path is neither resolved twice nor open to
/// a rename between the check and the writes.
fn folder_distrust_of(
    h: &OwnedHandle,
    path: &Path,
    elevated_user: &str,
) -> Result<Option<String>, SandboxError> {
    let attrs = attributes(h).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if !reparse && attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Ok(Some("it is a file, not a folder".to_owned()));
    }
    let sec = security_of(h, path)?;
    let owner = sid_text(sec.owner)?;
    let entries = ace_views(sec.dacl)?;
    Ok(distrust(&owner, entries.as_deref(), reparse, elevated_user))
}

/// Refuse the folder the record's own folders are made under unless its text
/// is a plain path on a local drive ([`super::super::acl::record_base_shape`])
/// and its volume is a fixed one that keeps access lists. The closed folders
/// are picked by both rules already; the record's folder was not checked by
/// either, and its access list is what keeps every other user out of it.
fn check_record_base(base: &Path) -> Result<(), SandboxError> {
    let text = base.to_string_lossy();
    if let Some(why) = super::super::acl::record_base_shape(&text) {
        return Err(fail("check the undo record's folder", why));
    }
    let root = format!("{}:\\", &text[..1]);
    if !fixed_acl_volume(&root) {
        return Err(fail(
            "check the undo record's folder",
            format!(
                "{root} is not a fixed local drive whose file system keeps access lists, so the \
                 sandbox undo record cannot be kept there"
            ),
        ));
    }
    Ok(())
}

/// Why the folder the record's own folders are made under cannot be trusted,
/// or `None` when it can. `%ProgramData%` is that folder, and the rules are
/// not [`folder_distrust_of`]'s: every local user may create a folder directly
/// under ProgramData by default, which is the premise of the whole scheme,
/// so the writer test would refuse every machine. What must hold is that the
/// base is a folder, not a link, and that an administrator principal owns it,
/// because the folders below it are made with a protected list of their own
/// and are checked by [`folder_distrust_of`].
fn base_distrust(
    h: &OwnedHandle,
    path: &Path,
    elevated_user: &str,
) -> Result<Option<String>, SandboxError> {
    let attrs = attributes(h).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(Some("it is a link (junction or symbolic link)".to_owned()));
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Ok(Some("it is a file, not a folder".to_owned()));
    }
    let sec = security_of(h, path)?;
    let owner = sid_text(sec.owner)?;
    Ok(
        (!super::super::acl::is_trusted_owner(&owner, elevated_user)).then(|| {
            format!("its owner is {owner}, not SYSTEM, Administrators or TrustedInstaller")
        }),
    )
}

/// The record's folder, with every folder from the base down held open for
/// as long as this value lives ([`open_held`]), so none of them can be
/// deleted or renamed between the check and the writes that follow.
#[derive(Debug)]
pub struct TrustedFolder {
    pub path: PathBuf,
    _held: Vec<OwnedHandle>,
}

/// Open and judge the folder the record's folders are made under.
fn held_base(base: &Path, elevated_user: &str) -> Result<OwnedHandle, SandboxError> {
    check_record_base(base)?;
    let h = open_held(base).map_err(|source| SandboxError::Io {
        path: base.to_path_buf(),
        source,
    })?;
    match base_distrust(&h, base, elevated_user)? {
        Some(why) => Err(refuse_folder(base, &why)),
        None => Ok(h),
    }
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
) -> Result<TrustedFolder, SandboxError> {
    let mut held = vec![held_base(base, elevated_user)?];
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
        let h = open_held(&path).map_err(|source| SandboxError::Io {
            path: path.clone(),
            source,
        })?;
        // Also a folder made just now: its owner follows the system's
        // policy for objects administrators create.
        if let Some(why) = folder_distrust_of(&h, &path, elevated_user)? {
            return Err(refuse_folder(&path, &why));
        }
        held.push(h);
    }
    Ok(TrustedFolder { path, _held: held })
}

/// [`trusted_folder`] without making anything: `None` when a part does not
/// exist (nothing was ever written there), an error when one exists and is
/// not trusted.
pub fn existing_trusted_folder(
    base: &Path,
    parts: &[&str],
    elevated_user: &str,
) -> Result<Option<TrustedFolder>, SandboxError> {
    let mut held = vec![held_base(base, elevated_user)?];
    let mut path = base.to_path_buf();
    for part in parts {
        path.push(part);
        // One open, which answers whether the part exists, carries the check
        // and then holds the folder: a second open would resolve the path
        // again.
        let h = match open_held(&path) {
            Ok(h) => h,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(source) => return Err(SandboxError::Io { path, source }),
        };
        if let Some(why) = folder_distrust_of(&h, &path, elevated_user)? {
            return Err(refuse_folder(&path, &why));
        }
        held.push(h);
    }
    Ok(Some(TrustedFolder { path, _held: held }))
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
/// Whether the volume whose root is `root` (`C:\`) is a fixed local drive
/// whose file system keeps access lists. A removable, remote or non-ACL
/// volume is skipped: a deny entry there would not last, and the undo record
/// cannot live there, because its access list is what keeps other users out
/// of it.
fn fixed_acl_volume(root: &str) -> bool {
    const DRIVE_FIXED: u32 = 3;
    let w = wide(OsStr::new(root));
    // SAFETY: NUL-terminated root path.
    if unsafe { GetDriveTypeW(w.as_ptr()) } != DRIVE_FIXED {
        return false;
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
    ok != 0 && flags & FILE_PERSISTENT_ACLS != 0
}

pub fn locations() -> Result<Locations, SandboxError> {
    let mut loc = Locations::default();
    // SAFETY: plain call.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", char::from(b'A' + i));
        if fixed_acl_volume(&root) {
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

/// Every folder [`closed_folders`] names on this machine, as text, whether
/// it exists or not. Removal bounds what it may take a deny entry off by
/// this set.
pub fn closable_folders() -> Result<std::collections::BTreeSet<String>, SandboxError> {
    Ok(closed_folders(&locations()?)
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect())
}

/// [`closed_folders`] of this machine that exist, as text for the record.
///
/// A link is not a folder here: `symlink_metadata` does not follow one,
/// where `is_dir` did, so a junction planted at one of these names is left
/// out instead of being carried to [`close_folder`], which would refuse it
/// and report a failure the owner cannot act on.
pub fn folders_to_close() -> Result<Vec<String>, SandboxError> {
    Ok(closed_folders(&locations()?)
        .into_iter()
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .map(|p| p.to_string_lossy().into_owned())
        .collect())
}

/// The owner of `path` as text, or why it could not be read. For the message
/// about a folder whose access list this step may not write: its owner is
/// who can grant that right.
pub fn owner_of(path: &Path) -> String {
    let read = open_no_follow(path, READ_CONTROL)
        .map_err(|e| e.to_string())
        .and_then(|h| security_of(&h, path).map_err(|e| e.to_string()));
    match read.and_then(|sec| sid_text(sec.owner).map_err(|e| e.to_string())) {
        Ok(sid) => sid,
        Err(why) => format!("its owner could not be read: {why}"),
    }
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

/// Whether the entry `ace` is the deny entry described by `sid`, `mask` and
/// `flags`: a deny entry for that SID with exactly that mask and exactly
/// those flags.
///
/// The mask is matched exactly, not as a subset. A caller that takes an
/// entry off names the mask the entry was written with, from the undo
/// record, so a later version of kGraf that denies more rights neither
/// leaves an older entry behind nor takes off an entry wider than the one
/// it wrote. `flags` must carry no inheritance bit, whatever the caller
/// says: nothing this code writes or removes may reach a child.
fn is_closing_entry(ace: *const c_void, sid: &Sid, mask: u32, flags: u8) -> bool {
    if flags & ACE_INHERITANCE_FLAGS != 0 {
        return false;
    }
    // SAFETY: every entry starts with a header.
    let header = unsafe { &*(ace as *const ACE_HEADER) };
    if u32::from(header.AceType) != ACCESS_DENIED_ACE_TYPE || header.AceFlags != flags {
        return false;
    }
    // SAFETY: a deny entry has the allow entry's layout.
    let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
    let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
    // SAFETY: both are valid SIDs.
    body.Mask == mask && unsafe { EqualSid(ace_sid, sid.0.0) } != 0
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

/// A handle on the folder `path` for a read and a write of its access list.
///
/// The access asked for is `MAXIMUM_ALLOWED`, not `READ_CONTROL | WRITE_DAC`,
/// because that is what keeps the write off the folder's children
/// ([`set_folder_dacl`]). The two rights it stands in for are not asked for
/// by name, so a folder whose list an administrator may not write (one
/// TrustedInstaller owns, say) opens here and fails at the write with
/// `ERROR_ACCESS_DENIED`, which [`super::setup`] reports with the folder's
/// owner.
/// Refuse a link in any component of `path` above the last one. The last one
/// is judged from the handle by [`open_folder_for_dacl`], whose open does not
/// follow it; without this, a link higher up would still be followed and the
/// access list of its target written instead.
///
/// This is a check and then an open, so another process could put a link in
/// place between the two. The atomic form opens every component with
/// `OBJ_DONT_REPARSE` through `NtCreateFile`, which this code does not have
/// (the upstream backend does). The components above a closed folder are a
/// drive root, `C:\Users` and the Windows folder, which administrators alone
/// may write.
fn refuse_links_above(path: &Path) -> Result<(), SandboxError> {
    for part in path.ancestors().skip(1) {
        if part.parent().is_none() {
            // A drive root or a prefix: there is no name here to redirect.
            continue;
        }
        let h = open_no_follow(part, FILE_READ_ATTRIBUTES).map_err(|source| SandboxError::Io {
            path: part.to_path_buf(),
            source,
        })?;
        let attrs = attributes(&h).map_err(|source| SandboxError::Io {
            path: part.to_path_buf(),
            source,
        })?;
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(fail(
                "close a folder to the sandbox users",
                format!(
                    "{} lies below the link {}; refusing to change the access list of whatever \
                     that link points at",
                    path.display(),
                    part.display()
                ),
            ));
        }
    }
    Ok(())
}

fn open_folder_for_dacl(path: &Path) -> Result<OwnedHandle, SandboxError> {
    refuse_links_above(path)?;
    let h = open_no_follow(path, MAXIMUM_ALLOWED).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
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

/// Write `acl` as the list of the folder behind `h`, that folder alone.
///
/// **Not `SetKernelObjectSecurity`.** Its page says: "This function should
/// not be used when setting a security descriptor on file system objects.
/// Instead, use the SetSecurityInfo or SetNamedSecurityInfo functions."
///
/// **How the write stays off the children.** `SetSecurityInfo` normally
/// walks them: "If you are setting the discretionary access control list
/// (DACL) or any elements in the system access control list (SACL) of an
/// object, the system automatically propagates any inheritable access
/// control entries (ACEs) to existing child objects". On a drive root that
/// is a walk of the whole drive. The same page gives the switch, and `h` is
/// opened with it ([`open_folder_for_dacl`]): "Warning: If the supplied
/// handle was opened with an ACCESS_MASK value of MAXIMUM_ALLOWED, then the
/// SetSecurityInfo function will not propagate ACEs to children." The entry
/// this code writes is `NO_INHERITANCE` as well, so there is nothing a child
/// could inherit from it either.
///
/// **How the folder keeps its protection.** Neither
/// `PROTECTED_DACL_SECURITY_INFORMATION` ("The DACL cannot inherit access
/// control entries (ACEs)") nor `UNPROTECTED_DACL_SECURITY_INFORMATION`
/// ("The DACL inherits ACEs from the parent object") is passed, and those
/// two flags are the only channel that sets the state, so ProgramData stays
/// protected and a folder that is not protected stays open to inheritance.
/// On a folder that is not protected the kernel also keeps the inherited
/// entries itself, so this list does not have to carry them:
/// `SeSetSecurityDescriptorInfoEx`, which is what the file system calls,
/// says that with `SE_DACL_PROTECTED` absent it "constructs the output
/// security descriptor from the inherited ACEs of the current security
/// descriptor and noninherited ACEs of SecurityDescriptor. That is, it is
/// impossible to change an inherited ACE by changing the ACL on an object."
fn set_folder_dacl(h: &OwnedHandle, acl: *const ACL, path: &Path) -> Result<(), SandboxError> {
    // SAFETY: a valid handle on a file system object and a valid list; no
    // owner, group or SACL is written.
    let code = unsafe {
        SetSecurityInfo(
            h.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        )
    };
    if code != ERROR_SUCCESS {
        return Err(SandboxError::Io {
            path: path.to_path_buf(),
            source: io::Error::from_raw_os_error(code as i32),
        });
    }
    Ok(())
}

/// Whether the folder `path` holds the deny entry `sid`, `mask` and `flags`
/// describe. [`closed_now`] asks about the entry this version of setup
/// writes; `remove` asks about the entry its record line names.
pub fn is_closed(path: &Path, group_sid: &str, mask: u32, flags: u8) -> Result<bool, SandboxError> {
    let sid = parse_sid(group_sid)?;
    let h = open_no_follow(path, READ_CONTROL).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let sec = security_of(&h, path)?;
    Ok(entries(sec.dacl)
        .into_iter()
        .any(|ace| is_closing_entry(ace, &sid, mask, flags)))
}

/// Whether the folder `path` holds the deny entry this version of setup
/// writes, for the status report and for the setup plan.
pub fn closed_now(path: &Path, group_sid: &str) -> Result<bool, SandboxError> {
    is_closed(path, group_sid, CLOSED_FOLDER_DENY, CLOSED_FOLDER_FLAGS)
}

/// Deny the sandbox group (`group_sid`) the right to create or change
/// entries in the folder `path`, on that folder alone. Read back after.
pub fn close_folder(path: &Path, group_sid: &str) -> Result<(), SandboxError> {
    let sid = parse_sid(group_sid)?;
    let h = open_folder_for_dacl(path)?;
    let sec = security_of(&h, path)?;
    if entries(sec.dacl)
        .into_iter()
        .any(|ace| is_closing_entry(ace, &sid, CLOSED_FOLDER_DENY, CLOSED_FOLDER_FLAGS))
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
    set_folder_dacl(&h, new_acl, path)?;
    if !closed_now(path, group_sid)? {
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
/// exists is skipped. Read back after, as [`close_folder`] does: a write
/// that kept the old list would otherwise leave the deny in place while
/// `remove` deletes the only record of it.
pub fn reopen_folder(
    path: &Path,
    group_sid: &str,
    mask: u32,
    flags: u8,
) -> Result<(), SandboxError> {
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
        .filter(|ace| !is_closing_entry(*ace, &sid, mask, flags))
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
    set_folder_dacl(&h, acl, path)?;
    if is_closed(path, group_sid, mask, flags)? {
        return Err(fail(
            "open a folder to the sandbox users again",
            format!(
                "the deny entry for the sandbox group is still on {} after it was taken off",
                path.display()
            ),
        ));
    }
    Ok(())
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

    /// The control bits of the folder's descriptor, so a test can show the
    /// protected and auto-inherited state is the same after a write as it
    /// was before. Production code never reads them.
    fn dacl_control(path: &Path) -> u16 {
        let h = open_no_follow(path, READ_CONTROL).unwrap();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: a handle opened with READ_CONTROL; `sd` is freed by `Local`.
        let code = unsafe {
            GetSecurityInfo(
                h.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(code, ERROR_SUCCESS, "read {}", path.display());
        let sd = Local(sd);
        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: a valid descriptor.
        let ok = unsafe {
            windows_sys::Win32::Security::GetSecurityDescriptorControl(
                sd.0,
                &mut control,
                &mut revision,
            )
        };
        assert_ne!(ok, 0, "read the control bits of {}", path.display());
        control
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
        assert!(found.is_none());
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
        assert_eq!(made.path, base.path().join("kGraf").join("sandbox"));
        // Again: it exists now and is checked, not made.
        trusted_folder(base.path(), &["kGraf", "sandbox"], &me()).unwrap();
    }

    #[test]
    fn a_closed_folder_gains_one_entry_and_loses_exactly_it_again() {
        // A SID no token holds, so the deny changes no one's access here.
        const GROUP: &str = "S-1-5-21-1-2-3-4";
        let dir = tempfile::tempdir().unwrap();
        let before = dacl_count(dir.path());
        assert!(!closed_now(dir.path(), GROUP).unwrap());
        close_folder(dir.path(), GROUP).unwrap();
        close_folder(dir.path(), GROUP).unwrap();
        assert!(closed_now(dir.path(), GROUP).unwrap());
        assert_eq!(dacl_count(dir.path()), before + 1, "written once");
        // A record line that named a wider mask takes nothing off: the
        // match is exact, so an entry this version did not write stays.
        reopen_folder(dir.path(), GROUP, CLOSED_FOLDER_DENY | 0x1, 0).unwrap();
        assert!(closed_now(dir.path(), GROUP).unwrap());
        assert_eq!(dacl_count(dir.path()), before + 1);
        reopen_folder(dir.path(), GROUP, CLOSED_FOLDER_DENY, CLOSED_FOLDER_FLAGS).unwrap();
        assert!(!closed_now(dir.path(), GROUP).unwrap());
        assert_eq!(dacl_count(dir.path()), before);
        // A folder that is gone is skipped; a link is refused.
        reopen_folder(
            &dir.path().join("gone"),
            GROUP,
            CLOSED_FOLDER_DENY,
            CLOSED_FOLDER_FLAGS,
        )
        .unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        mklink_junction(&link, elsewhere.path());
        assert!(close_folder(&link, GROUP).is_err());
        assert!(!closed_now(elsewhere.path(), GROUP).unwrap());
        // A link above the folder is refused too, so the list written is
        // never that of a folder below the link's target.
        let target = elsewhere.path().join("below");
        std::fs::create_dir(&target).unwrap();
        let through = link.join("below");
        let err = close_folder(&through, GROUP).unwrap_err();
        assert!(err.to_string().contains("link"), "{err}");
        assert!(!closed_now(&target, GROUP).unwrap());
    }

    /// The write must leave the folder's protection as Windows had it and
    /// must change no child. `SetSecurityInfo` on a handle opened with
    /// `MAXIMUM_ALLOWED` propagates nothing ([`set_folder_dacl`]); a write
    /// that propagated would rebuild each child's list.
    #[test]
    fn closing_a_folder_changes_no_child_and_keeps_the_protection() {
        const GROUP: &str = "S-1-5-21-1-2-3-5";
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let grandchild = child.join("leaf");
        std::fs::create_dir(&grandchild).unwrap();
        // An inheritable entry on the folder about to be closed: a call that
        // propagates would push it down and rewrite both lists.
        let status = std::process::Command::new("icacls.exe")
            .arg(dir.path())
            .args(["/grant", "*S-1-5-32-546:(OI)(CI)R"])
            .status()
            .unwrap();
        assert!(status.success(), "icacls failed");
        let control = dacl_control(dir.path());
        let (child_before, leaf_before) = (dacl_of(&child).unwrap(), dacl_of(&grandchild).unwrap());
        close_folder(dir.path(), GROUP).unwrap();
        assert!(closed_now(dir.path(), GROUP).unwrap());
        assert_eq!(dacl_control(dir.path()), control, "protection kept");
        assert_eq!(dacl_of(&child).unwrap(), child_before, "no child touched");
        assert_eq!(dacl_of(&grandchild).unwrap(), leaf_before);
        assert!(!closed_now(&child, GROUP).unwrap(), "the deny stays put");
        reopen_folder(dir.path(), GROUP, CLOSED_FOLDER_DENY, CLOSED_FOLDER_FLAGS).unwrap();
        assert_eq!(dacl_control(dir.path()), control, "protection still kept");
        assert_eq!(dacl_of(&child).unwrap(), child_before);
        assert_eq!(dacl_of(&grandchild).unwrap(), leaf_before);
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
