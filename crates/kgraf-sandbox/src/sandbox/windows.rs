//! Windows backend: a write-restricted token, a per-root capability SID
//! admitted by an ACL on each writable root, a Job Object, and a private
//! desktop. No administrator rights, no installer, no service.
//!
//! **Mechanism (decision 0011).** Chosen over
//! AppContainer because the evidence records that AppContainer breaks the
//! shell, Node, MSBuild, git and per-user toolchains without elevation, and
//! it is the toolchain that must run here. Windows Sandbox, WSL2-as-sandbox
//! and Hyperlight are ruled out by the same evidence.
//!
//! - **Token.** `CreateRestrictedToken` with `WRITE_RESTRICTED | LUA_TOKEN |
//!   DISABLE_MAX_PRIVILEGE`. The restricting SIDs are one capability SID per
//!   writable root, a fresh per-spawn SID, and Everyone. A write succeeds only
//!   if both the normal access check and the check against the restricting SIDs
//!   pass. The token integrity level is then dropped to Low.
//! - **Cross-sandbox isolation.** The token's default DACL grants the user SID
//!   and the per-spawn SID, never the logon SID. Because the per-spawn SID is
//!   unique and is a restricting SID, only the processes of one spawn pass the
//!   restricting write check on each other; a second sandbox (a different
//!   per-spawn SID) cannot open them for write. This closes cross-sandbox
//!   process injection, which a shared logon SID would allow.
//! - **Low integrity (defense in depth).** The
//!   token runs at the Low integrity level and each writable root is labeled
//!   Low. Mandatory Integrity Control then blocks writes up to Medium objects
//!   (the user's files, an Everyone-writable Medium directory) regardless of
//!   any `WRITE_DAC` subtlety, and UIPI blocks `SendInput` and window messages
//!   to the user's desktop. This is defense in depth over the capability
//!   confinement, which stays the primary write boundary. **Unverified:**
//!   whether cargo and the MSVC linker run at Low integrity must be confirmed
//!   on real Windows; if they do not, drop `set_integrity_low` and rely on the
//!   capability SIDs, the per-spawn SID and the private desktop.
//! - **ACL.** Each writable root gets an allow ACE for its capability SID.
//!   Children inherit `DELETE` instead of the root granting
//!   `FILE_DELETE_CHILD`, so the deny ACE on the top-level `.git` entry cannot
//!   be bypassed by deleting it through the parent. The root itself does not
//!   get `DELETE`, so it cannot be removed or moved. Effective write access is
//!   verified with `AccessCheck` under the restricted token, on the root and a
//!   child, so a partial ACL propagation fails loud.
//! - **Reparse and filesystem guards.** No root or `.git` entry that is a
//!   reparse point (a junction or symlink) is touched, so a planted junction
//!   cannot redirect an ACL change onto the user's profile. A root on a volume
//!   without persistent ACLs (FAT, exFAT, some SMB shares) is refused, because
//!   the whole mechanism is ACL-based.
//! - **Job Object.** Kill on job close, no breakaway (no `BREAKAWAY_OK` flag),
//!   every UI restriction (desktop switching, clipboard, global atoms, USER
//!   handles of processes outside the job, system parameters, exit Windows).
//!   The process is created suspended and joins the job before it runs.
//! - **Private desktop.** Reused per workspace, labeled Low, on the process's
//!   own window station (queried, not hard-coded, so scheduled or service runs
//!   work). Restricted processes must run on a non-default desktop so they
//!   cannot send window messages to unrestricted applications.
//!
//! **Partial isolation.** `WRITE_RESTRICTED` makes Windows consult the
//! restricting SIDs for writes only, and Low integrity blocks writes up, not
//! reads. Reads are therefore not confined: credential stores, browser
//! profiles and the Credential Manager API stay readable, and a Low process
//! can open the Medium harness process for `PROCESS_VM_READ`. Network egress
//! is not blocked (only AppContainer or administrator-installed filtering
//! rules can do that). [`STRENGTH`] reports all of this, and the runner
//! refuses unless the policy opts in to partial isolation (fail loud).
//!
//! **Provenance.** The token construction (restricting-SID order, flags,
//! default DACL, `SeChangeNotifyPrivilege`), the write-ACE mask without
//! `FILE_DELETE_CHILD`, the deny-write mask, the capability SID format and the
//! private-desktop ACL are ported from openai/codex
//! `codex-rs/windows-sandbox-rs/src/{token.rs,acl.rs,cap.rs,desktop.rs,process.rs}`,
//! commit e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0, Copyright 2025
//! OpenAI (see `third_party/codex/UPSTREAM`). Departures: ACL failures are
//! errors instead of being ignored; capability SIDs are derived from the root
//! path instead of persisted in a file; a new desktop per spawn instead of a
//! process-wide cache; no network environment rewrite (it is not isolation).
//!
//! **Status: compile-checked for x86_64-pc-windows-msvc only.** Nothing here
//! has run on Windows. The escape tests in `tests/escape_windows.rs` are
//! written for real Windows and have not run.

// The pure helpers compile on every platform so their unit tests run on the
// development host; only Windows uses them outside tests.
#![cfg_attr(not(windows), allow(dead_code))]

use super::{Backend, IsolationStrength, ReadConfinement};

pub(super) const BACKEND_NAME: &str = "Windows restricted token";

pub(super) const STRENGTH: IsolationStrength = IsolationStrength {
    backend: Backend::WindowsRestrictedToken,
    // Everyone is a restricting SID (codex found tools fail without it), so an
    // object whose DACL grants write to Everyone stays DACL-writable; the Low
    // integrity level then blocks the write unless that object is also labeled
    // Low. writes_confined stays false until both are verified on Windows.
    writes_confined: false,
    reads: ReadConfinement::Unconfined,
    unix_sockets_confined: false,
    network_deniable: false,
    // The deny ACE and the Low integrity level are applied, but neither has
    // been verified on Windows.
    gitlink_protected: false,
    gaps: "reads are not confined: the Low integrity level blocks writes up, not reads, so \
           %USERPROFILE%\\.ssh, browser profiles, the Credential Manager API and every file \
           the user can read stay readable; a Low process can also open the (Medium) harness \
           process for PROCESS_VM_READ, and the harness holds provider API keys while egress \
           is open, so a read there is exfiltratable; network egress is not blocked; a DACL \
           that grants write to Everyone still needs a Low label to be writable, but a Low, \
           Everyone-writable location (some temp and shared paths) is writable; named pipes \
           and sockets are confined only by their own ACLs and integrity labels; the gitlink \
           deny, the Low label on the writable roots, and cross-sandbox write denial are \
           unverified on Windows",
};

/// The capability SID for one writable root. Deterministic, so repeated runs
/// reuse one ACE instead of adding one per run (and re-propagating it through
/// the tree). Shape `S-1-5-21-a-b-c-d` follows codex `cap.rs`; codex draws
/// random values and persists them, which needs a state file. A SID is not a
/// secret: a process cannot add a SID to its own token without privilege.
pub(super) fn capability_sid_string(root_key: &str) -> String {
    let key = root_key.to_lowercase();
    let h1 = fnv1a64(0xcbf2_9ce4_8422_2325, key.as_bytes());
    let h2 = fnv1a64(0x6c62_272e_07bb_0142, key.as_bytes());
    format!(
        "S-1-5-21-{}-{}-{}-{}",
        (h1 >> 32) as u32,
        h1 as u32,
        (h2 >> 32) as u32,
        h2 as u32
    )
}

/// The domain separator keeps the product's former name (decision 0062): it
/// is part of every capability SID already granted in an ACL on a user's
/// disk, and a new separator would give each root a second ACE with nothing
/// gained. It is an identifier, never shown.
fn fnv1a64(seed: u64, bytes: &[u8]) -> u64 {
    b"haustoria-sandbox-cap-v1\0"
        .iter()
        .chain(bytes)
        .fold(seed, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// Append one argument to a UTF-16 command line under the MSVC runtime
/// parsing rules (the rules `CommandLineToArgvW` and Rust's std follow).
pub(super) fn append_arg(out: &mut Vec<u16>, arg: &[u16]) {
    const QUOTE: u16 = b'"' as u16;
    const SLASH: u16 = b'\\' as u16;
    if !out.is_empty() {
        out.push(u16::from(b' '));
    }
    let quote = arg.is_empty()
        || arg
            .iter()
            .any(|c| *c == u16::from(b' ') || *c == u16::from(b'\t'));
    if quote {
        out.push(QUOTE);
    }
    let mut slashes = 0usize;
    for &c in arg {
        if c == SLASH {
            slashes += 1;
        } else {
            if c == QUOTE {
                // Double the run of backslashes before a quote, then escape it.
                out.extend(std::iter::repeat_n(SLASH, slashes + 1));
            }
            slashes = 0;
        }
        out.push(c);
    }
    if quote {
        // Backslashes before the closing quote must be doubled.
        out.extend(std::iter::repeat_n(SLASH, slashes));
        out.push(QUOTE);
    }
}

/// `true` if the program is a batch file. `CreateProcess` runs those through
/// `cmd.exe`, whose argument parsing differs from the MSVC rules (the
/// BatBadBut class of injection); callers must invoke `cmd.exe` themselves.
pub(super) fn is_batch_file(program: &str) -> bool {
    let lower = program.to_ascii_lowercase();
    lower.ends_with(".bat") || lower.ends_with(".cmd")
}

/// An environment variable as UTF-16 name and value.
pub(super) type WideVar = (Vec<u16>, Vec<u16>);

/// Merge overrides into the inherited environment (names compare without
/// case, as Windows does) and encode a `CREATE_UNICODE_ENVIRONMENT` block:
/// `NAME=VALUE\0` entries sorted by upper-cased name, then a final `\0`.
pub(super) fn env_block(
    inherited: Vec<WideVar>,
    overrides: Vec<(Vec<u16>, Option<Vec<u16>>)>,
) -> Vec<u16> {
    let upper = |name: &[u16]| String::from_utf16_lossy(name).to_uppercase();
    let mut vars: std::collections::BTreeMap<String, WideVar> = inherited
        .into_iter()
        .map(|(name, value)| (upper(&name), (name, value)))
        .collect();
    for (name, value) in overrides {
        let key = upper(&name);
        match value {
            Some(value) => {
                vars.insert(key, (name, value));
            }
            None => {
                vars.remove(&key);
            }
        }
    }
    let mut block = Vec::new();
    for (name, value) in vars.into_values() {
        block.extend_from_slice(&name);
        block.push(u16::from(b'='));
        block.extend_from_slice(&value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(windows)]
pub use imp::SandboxedChild;
#[cfg(windows)]
pub(super) use imp::{Prepared, default_stdio, output, prepare, spawn, status};

#[cfg(windows)]
mod imp {
    use std::ffi::{OsStr, OsString, c_void};
    use std::io::{self, Read};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Output};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_INVALID_SID, ERROR_SUCCESS, GENERIC_ALL,
        GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LUID,
        LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        GetNamedSecurityInfoW, SE_FILE_OBJECT, SE_WINDOW_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AccessCheck, AddMandatoryAce,
        AdjustTokenPrivileges, CONTAINER_INHERIT_ACE, CopySid, CreateRestrictedToken,
        DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, EqualSid,
        GENERIC_MAPPING, GROUP_SECURITY_INFORMATION, GetAce, GetLengthSid, GetTokenInformation,
        INHERIT_ONLY_ACE, InitializeAcl, IsValidSid, LABEL_SECURITY_INFORMATION, LUA_TOKEN,
        LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, NO_INHERITANCE, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PSECURITY_DESCRIPTOR, PSID,
        SE_PRIVILEGE_ENABLED, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, SecurityImpersonation,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_IMPERSONATE,
        TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl,
        TokenGroups, TokenImpersonation, TokenIntegrityLevel, TokenUser, WRITE_RESTRICTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_DELETE_CHILD,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
        GetVolumeInformationW, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_UI_RESTRICTIONS,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, CreateDesktopW, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE,
        DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READ_CONTROL,
        DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS, GetProcessWindowStation,
        GetUserObjectInformationW, HDESK, UOI_NAME,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, FILE_PERSISTENT_ACLS,
        JOB_OBJECT_UILIMIT_ALL, SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID,
        SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };

    use super::super::{Network, Redirect, Resolved};
    use super::{BACKEND_NAME, append_arg, capability_sid_string, env_block, is_batch_file};
    use crate::error::SandboxError;

    // ---- errors -------------------------------------------------------------

    fn last_error(step: &'static str) -> SandboxError {
        SandboxError::PlatformSetup {
            step,
            source: io::Error::last_os_error(),
        }
    }

    fn win32(step: &'static str, code: u32) -> SandboxError {
        SandboxError::PlatformSetup {
            step,
            source: io::Error::from_raw_os_error(code as i32),
        }
    }

    fn acl_error(path: &Path, code: u32) -> SandboxError {
        SandboxError::Io {
            path: path.to_path_buf(),
            source: io::Error::from_raw_os_error(code as i32),
        }
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Take ownership of a handle a Win32 call returned, or report the call's
    /// error. Null and `INVALID_HANDLE_VALUE` both mean failure here.
    fn owned(h: HANDLE, step: &'static str) -> Result<OwnedHandle, SandboxError> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(last_error(step));
        }
        // SAFETY: `h` is a valid handle just returned to this process, and
        // nothing else owns it.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// Memory the system allocated with `LocalAlloc` (SIDs, ACLs, security
    /// descriptors), freed on drop.
    struct Local(*mut c_void);

    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from a Win32 call documented to
                // allocate with LocalAlloc, and is freed once.
                unsafe { LocalFree(self.0 as HLOCAL) };
            }
        }
    }

    // ---- SIDs ---------------------------------------------------------------

    /// An owned SID in a `u32` buffer, so the subauthority array is aligned.
    #[derive(Debug, Clone)]
    pub(in crate::sandbox) struct Sid(Vec<u32>);

    impl Sid {
        fn from_string(s: &str) -> Result<Self, SandboxError> {
            let text = wide(OsStr::new(s));
            let mut psid: PSID = ptr::null_mut();
            // SAFETY: `text` is NUL-terminated; on success the system writes a
            // LocalAlloc'd SID pointer, released by `Local`.
            if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut psid) } == 0 {
                return Err(last_error("ConvertStringSidToSidW"));
            }
            let _guard = Local(psid);
            // SAFETY: `psid` is a valid SID returned above.
            unsafe { Self::copy_from(psid) }
        }

        /// # Safety
        /// `psid` must point to a valid SID.
        unsafe fn copy_from(psid: PSID) -> Result<Self, SandboxError> {
            // SAFETY: caller guarantees a valid SID.
            if unsafe { IsValidSid(psid) } == 0 {
                return Err(win32("IsValidSid", ERROR_INVALID_SID));
            }
            // SAFETY: valid SID.
            let len = unsafe { GetLengthSid(psid) };
            let mut buf = vec![0u32; (len as usize).div_ceil(4)];
            // SAFETY: `buf` holds at least `len` bytes.
            if unsafe { CopySid(len, buf.as_mut_ptr().cast(), psid) } == 0 {
                return Err(last_error("CopySid"));
            }
            Ok(Self(buf))
        }

        fn as_psid(&self) -> PSID {
            self.0.as_ptr() as PSID
        }
    }

    fn trustee(sid: &Sid) -> TRUSTEE_W {
        TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_psid().cast(),
        }
    }

    fn explicit(sid: &Sid, mask: u32, mode: i32, inheritance: u32) -> EXPLICIT_ACCESS_W {
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: mode,
            grfInheritance: inheritance,
            Trustee: trustee(sid),
        }
    }

    // ---- ACLs on writable roots ----------------------------------------------

    /// Read, write and execute without `DELETE`: applied to the root itself.
    const ROOT_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
    /// Inherited by everything below the root. `DELETE` on each child instead
    /// of `FILE_DELETE_CHILD` on the parent, which would bypass a deny ACE on
    /// a protected child (codex `acl.rs`, `WRITE_ALLOW_MASK`).
    const TREE_MASK: u32 = ROOT_MASK | DELETE;
    const TREE_INHERIT: u32 = INHERIT_ONLY_ACE | OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    /// What the gitlink deny removes. Not `FILE_GENERIC_WRITE`: that includes
    /// `SYNCHRONIZE` and `READ_CONTROL`, which reads need.
    const DENY_WRITE_MASK: u32 = FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD
        | WRITE_DAC
        | WRITE_OWNER;

    /// A file object's DACL and the descriptor that owns it.
    struct Dacl {
        acl: *mut ACL,
        _sd: Local,
    }

    fn read_dacl(path: &Path) -> Result<Dacl, SandboxError> {
        let name = wide(path.as_os_str());
        let mut acl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; outputs are written on success and
        // the descriptor is freed by `Local`.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut acl,
                ptr::null_mut(),
                &mut sd,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        Ok(Dacl {
            acl,
            _sd: Local(sd),
        })
    }

    /// Whether `dacl` holds an explicit (not inherited) ACE of `ace_type` for
    /// `sid` covering `mask`, with exactly the inheritance flags `flags`.
    fn has_ace(dacl: &Dacl, ace_type: u32, sid: &Sid, mask: u32, flags: u32) -> bool {
        if dacl.acl.is_null() {
            return false;
        }
        const INHERITANCE_BITS: u32 = 0x0F; // OI | CI | NP | IO
        const INHERITED_ACE: u32 = 0x10;
        // SAFETY: `acl` points into the descriptor `dacl` owns.
        let count = unsafe { (*dacl.acl).AceCount };
        (0..u32::from(count)).any(|i| {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: `i` is below the ACE count of a valid ACL.
            if unsafe { GetAce(dacl.acl, i, &mut ace) } == 0 {
                return false;
            }
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            if u32::from(header.AceType) != ace_type
                || u32::from(header.AceFlags) & INHERITED_ACE != 0
                || u32::from(header.AceFlags) & INHERITANCE_BITS != flags
            {
                return false;
            }
            // SAFETY: allowed and denied ACEs share the ACCESS_ALLOWED_ACE
            // layout; the SID starts at `SidStart`.
            let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
            // SAFETY: both are valid SIDs.
            body.Mask & mask == mask && unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0
        })
    }

    /// Merge `entries` into the DACL of `path` and write it back. Writing a
    /// directory's DACL propagates inheritable ACEs to every existing child,
    /// so callers skip this when the ACEs are already present.
    fn apply(path: &Path, dacl: &Dacl, entries: &[EXPLICIT_ACCESS_W]) -> Result<(), SandboxError> {
        let mut new_acl: *mut ACL = ptr::null_mut();
        // SAFETY: `entries` is a valid slice; the old ACL is valid or null;
        // the new ACL is LocalAlloc'd and freed by `Local`.
        let code = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                dacl.acl,
                &mut new_acl,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        let _guard = Local(new_acl.cast());
        let name = wide(path.as_os_str());
        // SAFETY: `name` is NUL-terminated; `new_acl` is a valid ACL.
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                new_acl,
                ptr::null(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        Ok(())
    }

    /// Admit `sid` to write beneath `root`. The owner of an object may always
    /// rewrite its DACL, so this needs no administrator rights for a
    /// user-owned worktree. Verified by reading the DACL back (fail loud).
    fn grant_writable_root(root: &Path, sid: &Sid) -> Result<(), SandboxError> {
        let present = |d: &Dacl| {
            has_ace(d, ACCESS_ALLOWED_ACE_TYPE, sid, ROOT_MASK, NO_INHERITANCE)
                && has_ace(d, ACCESS_ALLOWED_ACE_TYPE, sid, TREE_MASK, TREE_INHERIT)
        };
        let dacl = read_dacl(root)?;
        if present(&dacl) {
            return Ok(());
        }
        apply(
            root,
            &dacl,
            &[
                explicit(sid, ROOT_MASK, GRANT_ACCESS, NO_INHERITANCE),
                explicit(sid, TREE_MASK, GRANT_ACCESS, TREE_INHERIT),
            ],
        )?;
        if !present(&read_dacl(root)?) {
            return Err(SandboxError::PlatformSetup {
                step: "verify writable-root ACEs",
                source: io::Error::other(format!(
                    "the allow ACEs for {} are missing after SetNamedSecurityInfoW",
                    root.display()
                )),
            });
        }
        Ok(())
    }

    /// Deny writes to `path` for `sid` (the top-level `.git` entry). Deny ACEs
    /// are ordered before allows by `SetEntriesInAclW`.
    fn deny_writes(path: &Path, sid: &Sid) -> Result<(), SandboxError> {
        let flags = if path.is_dir() {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            NO_INHERITANCE
        };
        let dacl = read_dacl(path)?;
        if has_ace(&dacl, ACCESS_DENIED_ACE_TYPE, sid, DENY_WRITE_MASK, flags) {
            return Ok(());
        }
        apply(
            path,
            &dacl,
            &[explicit(sid, DENY_WRITE_MASK, DENY_ACCESS, flags)],
        )
    }

    /// `\\?\C:\x` -> `C:\x`. `fs::canonicalize` returns verbatim paths;
    /// codex strips them (`dunce`) before the ACL calls, and so does this.
    fn strip_verbatim(path: &Path) -> PathBuf {
        let s = path.as_os_str().to_string_lossy();
        match s.strip_prefix(r"\\?\") {
            Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
            _ => path.to_path_buf(),
        }
    }

    // ---- reparse, volume and integrity-label guards ----------------------------

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    /// The Low integrity level SID (`S-1-16-4096`).
    const LOW_INTEGRITY_SID: &str = "S-1-16-4096";

    /// Refuse a path that is a reparse point (junction or symlink), so a
    /// planted junction cannot redirect an ACL or label change elsewhere
    /// `symlink_metadata` does not follow the final component.
    fn refuse_reparse(path: &Path) -> Result<(), SandboxError> {
        use std::os::windows::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).map_err(|source| SandboxError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SandboxError::PlatformSetup {
                step: "reject a reparse point before changing its security",
                source: io::Error::other(format!(
                    "{} is a reparse point (junction or symlink); refusing to follow it",
                    path.display()
                )),
            });
        }
        Ok(())
    }

    /// Refuse a writable root on a volume whose filesystem does not keep ACLs
    /// (FAT, exFAT, some SMB shares); the whole mechanism is ACL-based.
    fn require_acl_volume(root: &Path) -> Result<(), SandboxError> {
        // The volume-root prefix, e.g. `C:\`. Without a prefix, let the ACL
        // calls themselves fail rather than guess.
        let Some(std::path::Component::Prefix(prefix)) = root.components().next() else {
            return Ok(());
        };
        let mut mount = prefix.as_os_str().to_os_string();
        mount.push("\\");
        let mount = wide(&mount);
        let mut flags = 0u32;
        // SAFETY: `mount` is NUL-terminated; the name and serial buffers are
        // null (not requested); `flags` receives the filesystem flags.
        let ok = unsafe {
            GetVolumeInformationW(
                mount.as_ptr(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut flags,
                ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(last_error("GetVolumeInformationW"));
        }
        if flags & FILE_PERSISTENT_ACLS == 0 {
            return Err(SandboxError::PlatformSetup {
                step: "require an ACL-preserving filesystem for a writable root",
                source: io::Error::other(format!(
                    "the volume of {} does not preserve ACLs (FAT, exFAT or a share); \
                     the sandbox confines writes with ACLs and cannot use it",
                    root.display()
                )),
            });
        }
        Ok(())
    }

    /// Build the Low integrity SID.
    fn low_sid() -> Result<Sid, SandboxError> {
        Sid::from_string(LOW_INTEGRITY_SID)
    }

    /// Label `root` with a Low mandatory label (`no-write-up`, inherited), so a
    /// Low-integrity sandbox can write there while still being unable to write
    /// up to Medium objects elsewhere.
    fn label_root_low(root: &Path) -> Result<(), SandboxError> {
        let low = low_sid()?;
        // ACL buffer: header + one mandatory-label ACE + the label SID.
        let mut acl = vec![0u32; 64];
        let bytes = std::mem::size_of_val(acl.as_slice()) as u32;
        // SAFETY: `acl` is a DWORD-aligned buffer of `bytes` bytes.
        if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), bytes, ACL_REVISION) } == 0 {
            return Err(last_error("InitializeAcl(label)"));
        }
        // SAFETY: a valid empty ACL and a valid label SID.
        let ok = unsafe {
            AddMandatoryAce(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                low.as_psid(),
            )
        };
        if ok == 0 {
            return Err(last_error("AddMandatoryAce"));
        }
        let name = wide(root.as_os_str());
        // SAFETY: `name` is NUL-terminated; the SACL is a valid ACL.
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                acl.as_ptr().cast(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(root, code));
        }
        Ok(())
    }

    // ---- prepare --------------------------------------------------------------

    /// Capability SIDs for the policy's writable roots, whose ACLs are in place.
    #[derive(Debug)]
    pub(in crate::sandbox) struct Prepared {
        caps: Vec<Sid>,
        /// Identifies the workspace, so its private desktop is reused.
        desktop_key: String,
    }

    /// Check what the backend can enforce and set the root ACLs. Runs when the
    /// command is built, so a failure surfaces before anything starts.
    pub(in crate::sandbox) fn prepare(policy: &Resolved) -> Result<Prepared, SandboxError> {
        if policy.network == Network::Deny {
            return Err(SandboxError::PolicyNotEnforceable {
                backend: BACKEND_NAME,
                what: "network denial (without administrator rights only AppContainer can deny \
                       sockets, and AppContainer breaks the toolchains)",
            });
        }
        if !policy.unix_sockets.is_empty() {
            return Err(SandboxError::PolicyNotEnforceable {
                backend: BACKEND_NAME,
                what: "a unix-socket allowlist (a socket outside the writable roots stays \
                       write-denied, so an allowlisted socket would not open)",
            });
        }
        let roots: Vec<PathBuf> = policy.writable.iter().map(|r| strip_verbatim(r)).collect();
        let caps = roots
            .iter()
            .map(|root| Sid::from_string(&capability_sid_string(&root.to_string_lossy())))
            .collect::<Result<Vec<_>, _>>()?;
        for (root, sid) in roots.iter().zip(&caps) {
            // Order matters: refuse a junction and a non-ACL volume before any
            // security call, so neither is followed or silently ineffective.
            refuse_reparse(root)?;
            require_acl_volume(root)?;
            grant_writable_root(root, sid)?;
            label_root_low(root)?;
            verify_effective_write(root, sid)?;
        }
        // Deny writes to a real (non-reparse) `.git` under a writable root. A
        // planted `scratch\.git` junction is refused by `refuse_reparse`, so an
        // ACL change is never redirected onto the user's profile. Every
        // capability SID, since a root nested above another would otherwise
        // reach the inner `.git` through inheritance.
        for root in &roots {
            let gitlink = root.join(".git");
            if !gitlink.exists() {
                continue;
            }
            refuse_reparse(&gitlink)?;
            for sid in &caps {
                deny_writes(&gitlink, sid)?;
            }
        }
        // The first writable root (the worktree root under the standard policy)
        // names the workspace for desktop reuse.
        let desktop_key = roots
            .first()
            .map(|r| r.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        Ok(Prepared { caps, desktop_key })
    }

    /// Confirm the restricted token is actually granted write on `root` and on
    /// a child, with `AccessCheck`, not just that the ACE is present: a partial
    /// propagation must fail loud.
    fn verify_effective_write(root: &Path, cap: &Sid) -> Result<(), SandboxError> {
        let token = restricted_token_for(std::slice::from_ref(cap))?.0;
        // An impersonation token, which is what AccessCheck requires.
        let mut imp: HANDLE = ptr::null_mut();
        // SAFETY: `token` is a valid primary token; output written on success.
        let ok = unsafe {
            DuplicateTokenEx(
                token.as_raw_handle(),
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut imp,
            )
        };
        if ok == 0 {
            return Err(last_error("DuplicateTokenEx(AccessCheck)"));
        }
        let imp = owned(imp, "DuplicateTokenEx")?;
        let probe = |path: &Path| effective_write_granted(path, &imp);
        if !probe(root)? {
            return Err(access_gap(root));
        }
        // A child directory created for the probe (removed after). If the tree
        // ACE did not propagate, this fails.
        let child = root.join(".kgraf-acl-probe");
        std::fs::create_dir_all(&child).map_err(|source| SandboxError::Io {
            path: child.clone(),
            source,
        })?;
        let granted = probe(&child);
        let _ = std::fs::remove_dir_all(&child);
        if !granted? {
            return Err(access_gap(&child));
        }
        Ok(())
    }

    fn access_gap(path: &Path) -> SandboxError {
        SandboxError::PlatformSetup {
            step: "verify effective write access under the restricted token",
            source: io::Error::other(format!(
                "the restricted token is not granted write on {}; the ACL did not fully \
                 propagate",
                path.display()
            )),
        }
    }

    /// Whether `token` (impersonation) is granted `FILE_GENERIC_WRITE` on `path`.
    fn effective_write_granted(path: &Path, token: &OwnedHandle) -> Result<bool, SandboxError> {
        let name = wide(path.as_os_str());
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; the descriptor is freed by `Local`.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut sd,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(acl_error(path, code));
        }
        let _guard = Local(sd);
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        let mut privset = PRIVILEGE_SET::default();
        let mut privlen = std::mem::size_of::<PRIVILEGE_SET>() as u32;
        let mut granted = 0u32;
        let mut status = 0;
        // SAFETY: a valid self-relative descriptor and impersonation token;
        // all output pointers are valid and sized.
        let ok = unsafe {
            AccessCheck(
                sd,
                token.as_raw_handle(),
                FILE_GENERIC_WRITE,
                &mapping,
                &mut privset,
                &mut privlen,
                &mut granted,
                &mut status,
            )
        };
        if ok == 0 {
            return Err(last_error("AccessCheck"));
        }
        Ok(status != 0)
    }

    // ---- token ----------------------------------------------------------------

    fn logon_sid(token: &OwnedHandle) -> Result<Sid, SandboxError> {
        let h = token.as_raw_handle();
        let mut needed = 0u32;
        // SAFETY: size query with a null buffer.
        unsafe { GetTokenInformation(h, TokenGroups, ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(last_error("GetTokenInformation(TokenGroups) size"));
        }
        // u64 backing keeps TOKEN_GROUPS and its pointers aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` holds `needed` bytes.
        if unsafe {
            GetTokenInformation(h, TokenGroups, buf.as_mut_ptr().cast(), needed, &mut needed)
        } == 0
        {
            return Err(last_error("GetTokenInformation(TokenGroups)"));
        }
        let groups = buf.as_ptr() as *const TOKEN_GROUPS;
        // SAFETY: the system wrote a TOKEN_GROUPS whose `Groups` array has
        // `GroupCount` entries inside `buf`.
        let entries = unsafe {
            std::slice::from_raw_parts((*groups).Groups.as_ptr(), (*groups).GroupCount as usize)
        };
        let logon = entries
            .iter()
            .find(|g| g.Attributes & (SE_GROUP_LOGON_ID as u32) == SE_GROUP_LOGON_ID as u32)
            .ok_or_else(|| SandboxError::PlatformSetup {
                step: "find the logon SID",
                source: io::Error::other("the process token has no logon SID"),
            })?;
        // SAFETY: the SID pointer refers into `buf`, still alive.
        unsafe { Sid::copy_from(logon.Sid) }
    }

    /// The token's user SID (`TokenUser`).
    fn user_sid(token: &OwnedHandle) -> Result<Sid, SandboxError> {
        let h = token.as_raw_handle();
        let mut needed = 0u32;
        // SAFETY: size query with a null buffer.
        unsafe { GetTokenInformation(h, TokenUser, ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(last_error("GetTokenInformation(TokenUser) size"));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` holds `needed` bytes.
        if unsafe {
            GetTokenInformation(h, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed)
        } == 0
        {
            return Err(last_error("GetTokenInformation(TokenUser)"));
        }
        let user = buf.as_ptr() as *const TOKEN_USER;
        // SAFETY: the system wrote a TOKEN_USER whose SID points into `buf`.
        unsafe { Sid::copy_from((*user).User.Sid) }
    }

    /// A fresh, unique SID for one spawn. Granted in the default DACL and added
    /// to the restricting set, it isolates one spawn's process tree from every
    /// other sandbox's: only this spawn's processes pass the restricting
    /// write check on each other. Shape follows `capability_sid_string`.
    fn per_spawn_sid() -> Result<Sid, SandboxError> {
        let (a, b) = (random_u64(), random_u64());
        Sid::from_string(&format!(
            "S-1-5-21-{}-{}-{}-{}",
            (a >> 32) as u32,
            a as u32,
            (b >> 32) as u32,
            b as u32
        ))
    }

    /// Objects the sandboxed process creates without an inheritable ACL (its
    /// own children, pipes, events) are reachable from this spawn's own tree
    /// only. The default DACL grants the user SID (for the process's own normal
    /// access check) and the per-spawn SID (a restricting SID unique to this
    /// spawn), never the logon SID (which every same-session sandbox shares and
    /// which allowed cross-sandbox process injection). OWNER RIGHTS removes the
    /// owner's implicit `WRITE_DAC` (codex).
    fn set_default_dacl(token: &OwnedHandle, user: &Sid, spawn: &Sid) -> Result<(), SandboxError> {
        let owner_rights = Sid::from_string("S-1-3-4")?;
        let entries = [
            explicit(user, GENERIC_ALL, GRANT_ACCESS, NO_INHERITANCE),
            explicit(spawn, GENERIC_ALL, GRANT_ACCESS, NO_INHERITANCE),
            explicit(&owner_rights, READ_CONTROL, GRANT_ACCESS, NO_INHERITANCE),
        ];
        let mut acl: *mut ACL = ptr::null_mut();
        // SAFETY: valid entries, no old ACL; result freed by `Local`.
        let code = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                ptr::null(),
                &mut acl,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(win32("SetEntriesInAclW(default DACL)", code));
        }
        let _guard = Local(acl.cast());
        let info = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
        // SAFETY: `info` is a TOKEN_DEFAULT_DACL of the stated size.
        let ok = unsafe {
            SetTokenInformation(
                token.as_raw_handle(),
                TokenDefaultDacl,
                ptr::addr_of!(info).cast(),
                std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error("SetTokenInformation(TokenDefaultDacl)"));
        }
        Ok(())
    }

    /// Bypass-traverse checking. Without it, opening a path fails wherever an
    /// ancestor directory denies listing (codex enables it the same way).
    fn enable_change_notify(token: &OwnedHandle) -> Result<(), SandboxError> {
        let name = wide(OsStr::new("SeChangeNotifyPrivilege"));
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        // SAFETY: `name` is NUL-terminated.
        if unsafe { LookupPrivilegeValueW(ptr::null(), name.as_ptr(), &mut luid) } == 0 {
            return Err(last_error("LookupPrivilegeValueW"));
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        // SAFETY: a valid TOKEN_PRIVILEGES; previous state not requested.
        let ok = unsafe {
            AdjustTokenPrivileges(
                token.as_raw_handle(),
                0,
                &privileges,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        // AdjustTokenPrivileges reports a missing privilege through
        // GetLastError even when it returns success.
        // SAFETY: plain call.
        if ok == 0 || unsafe { GetLastError() } != ERROR_SUCCESS {
            return Err(last_error("AdjustTokenPrivileges(SeChangeNotifyPrivilege)"));
        }
        Ok(())
    }

    /// Drop the token's integrity level to Low. A Low process cannot write up
    /// to a Medium object (the user's files, an Everyone-writable Medium
    /// directory) and UIPI blocks `SendInput` to the user's desktop, whatever
    /// the `WRITE_DAC` semantics of the restricted token are.
    fn set_integrity_low(token: &OwnedHandle) -> Result<(), SandboxError> {
        let low = low_sid()?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: low.as_psid(),
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        // SAFETY: `label` is a valid TOKEN_MANDATORY_LABEL; the size includes
        // the SID it points at.
        let ok = unsafe {
            SetTokenInformation(
                token.as_raw_handle(),
                TokenIntegrityLevel,
                ptr::addr_of!(label).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(low.as_psid()),
            )
        };
        if ok == 0 {
            return Err(last_error("SetTokenInformation(TokenIntegrityLevel=Low)"));
        }
        Ok(())
    }

    /// A restricted primary token whose restricting SIDs are exactly
    /// `restricting`, with change-notify enabled but no default DACL or
    /// integrity change. Returns the token, its user SID and its logon SID.
    fn restricted_token_for(restricting: &[Sid]) -> Result<(OwnedHandle, Sid, Sid), SandboxError> {
        let mut raw: HANDLE = ptr::null_mut();
        // SAFETY: pseudo-handle of this process; output written on success.
        let ok = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE
                    | TOKEN_QUERY
                    | TOKEN_ASSIGN_PRIMARY
                    | TOKEN_ADJUST_DEFAULT
                    | TOKEN_ADJUST_PRIVILEGES,
                &mut raw,
            )
        };
        if ok == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        let base = owned(raw, "OpenProcessToken")?;
        let user = user_sid(&base)?;
        let logon = logon_sid(&base)?;
        let restricting_sa: Vec<SID_AND_ATTRIBUTES> = restricting
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_psid(),
                Attributes: 0,
            })
            .collect();
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `restricting_sa` points at SIDs that outlive the call.
        let ok = unsafe {
            CreateRestrictedToken(
                base.as_raw_handle(),
                DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
                0,
                ptr::null(),
                0,
                ptr::null(),
                restricting_sa.len() as u32,
                restricting_sa.as_ptr(),
                &mut token,
            )
        };
        if ok == 0 {
            return Err(last_error("CreateRestrictedToken"));
        }
        let token = owned(token, "CreateRestrictedToken")?;
        enable_change_notify(&token)?;
        Ok((token, user, logon))
    }

    /// The primary token for a spawn: restricted by the capability SIDs, a
    /// fresh per-spawn SID and Everyone; default DACL scoped to the user and
    /// the per-spawn SID; integrity dropped to Low. Returns the token, the
    /// logon SID and the user SID (both for the desktop DACL).
    fn build_sandbox_token(caps: &[Sid]) -> Result<(OwnedHandle, Sid, Sid), SandboxError> {
        let per_spawn = per_spawn_sid()?;
        let everyone = Sid::from_string("S-1-1-0")?;
        let mut restricting: Vec<Sid> = Vec::with_capacity(caps.len() + 2);
        restricting.extend(caps.iter().cloned());
        restricting.push(per_spawn.clone());
        restricting.push(everyone);
        let (token, user, logon) = restricted_token_for(&restricting)?;
        set_default_dacl(&token, &user, &per_spawn)?;
        set_integrity_low(&token)?;
        Ok((token, logon, user))
    }

    // ---- job and desktop --------------------------------------------------------

    fn create_job() -> Result<OwnedHandle, SandboxError> {
        // SAFETY: anonymous job, default security.
        let job = owned(
            unsafe { CreateJobObjectW(ptr::null(), ptr::null()) },
            "CreateJobObjectW",
        )?;
        // SAFETY: zeroed is a valid initial value for this plain C struct.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // No JOB_OBJECT_LIMIT_BREAKAWAY_OK: CREATE_BREAKAWAY_FROM_JOB fails.
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        // SAFETY: a valid struct of the stated size and class.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::addr_of!(limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(last_error("SetInformationJobObject(extended limits)"));
        }
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_ALL,
        };
        // SAFETY: as above.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicUIRestrictions,
                ptr::addr_of!(ui).cast(),
                std::mem::size_of_val(&ui) as u32,
            )
        } == 0
        {
            return Err(last_error("SetInformationJobObject(UI restrictions)"));
        }
        Ok(job)
    }

    /// Rights the sandbox gets on its desktop: everything except switching to
    /// it, deleting it and changing its DACL or owner.
    const DESKTOP_PARTICIPANT: u32 = DESKTOP_READOBJECTS
        | DESKTOP_CREATEWINDOW
        | DESKTOP_CREATEMENU
        | DESKTOP_HOOKCONTROL
        | DESKTOP_JOURNALRECORD
        | DESKTOP_JOURNALPLAYBACK
        | DESKTOP_ENUMERATE
        | DESKTOP_WRITEOBJECTS
        | DESKTOP_READ_CONTROL;

    struct Desktop {
        handle: HDESK,
        /// `<WindowStation>\<name>`, NUL-terminated, for
        /// `STARTUPINFOW::lpDesktop`.
        startup_name: Vec<u16>,
    }

    // SAFETY: a desktop handle is a kernel handle usable from any thread.
    unsafe impl Send for Desktop {}
    // SAFETY: no interior state beyond the handle and an immutable name.
    unsafe impl Sync for Desktop {}

    /// The current process's window station name (`WinSta0` interactively, but
    /// a distinct name under a service or a scheduled, non-interactive run;
    /// decision 0052). Never hard-coded.
    fn window_station_name() -> Result<String, SandboxError> {
        // SAFETY: the returned handle is a pseudo-handle not to be closed.
        let winsta = unsafe { GetProcessWindowStation() };
        if winsta.is_null() {
            return Err(last_error("GetProcessWindowStation"));
        }
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        // SAFETY: `buf` has the stated size in bytes.
        let ok = unsafe {
            GetUserObjectInformationW(
                winsta,
                UOI_NAME,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(&buf) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error("GetUserObjectInformationW(window station)"));
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..len]))
    }

    impl Desktop {
        fn create(user: &Sid) -> Result<Self, SandboxError> {
            let name = format!("kGrafSandbox-{:016x}", random_u64());
            let wname = wide(OsStr::new(&name));
            // SAFETY: NUL-terminated name, no device or mode, default security;
            // the handle is closed by Drop. Full rights for this process.
            let handle = unsafe {
                CreateDesktopW(
                    wname.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    0x000F_01FF,
                    ptr::null(),
                )
            };
            if handle.is_null() {
                return Err(last_error("CreateDesktopW"));
            }
            let station = window_station_name()?;
            let desktop = Self {
                handle,
                startup_name: wide(OsStr::new(&format!("{station}\\{name}"))),
            };
            // Replace the DACL: the sandbox (its token carries the user SID as
            // a normal group) participates; nobody else is admitted.
            let entries = [explicit(
                user,
                DESKTOP_PARTICIPANT,
                GRANT_ACCESS,
                NO_INHERITANCE,
            )];
            let mut acl: *mut ACL = ptr::null_mut();
            // SAFETY: valid entries, no old ACL.
            let code = unsafe { SetEntriesInAclW(1, entries.as_ptr(), ptr::null(), &mut acl) };
            if code != ERROR_SUCCESS {
                return Err(win32("SetEntriesInAclW(desktop)", code));
            }
            let _guard = Local(acl.cast());
            // SAFETY: a desktop handle with WRITE_DAC and a valid ACL.
            let code = unsafe {
                SetSecurityInfo(
                    desktop.handle,
                    SE_WINDOW_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    acl,
                    ptr::null(),
                )
            };
            if code != ERROR_SUCCESS {
                return Err(win32("SetSecurityInfo(desktop)", code));
            }
            // Low-label the desktop so a Low-integrity sandbox can use it while
            // still being blocked (UIPI) from the user's Medium desktop.
            desktop.label_low()?;
            Ok(desktop)
        }

        fn label_low(&self) -> Result<(), SandboxError> {
            let low = low_sid()?;
            let mut acl = vec![0u32; 64];
            let bytes = std::mem::size_of_val(acl.as_slice()) as u32;
            // SAFETY: a DWORD-aligned buffer of `bytes` bytes.
            if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), bytes, ACL_REVISION) } == 0 {
                return Err(last_error("InitializeAcl(desktop label)"));
            }
            // SAFETY: a valid empty ACL and a valid label SID.
            if unsafe {
                AddMandatoryAce(
                    acl.as_mut_ptr().cast(),
                    ACL_REVISION,
                    NO_INHERITANCE,
                    SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                    low.as_psid(),
                )
            } == 0
            {
                return Err(last_error("AddMandatoryAce(desktop)"));
            }
            // SAFETY: a desktop handle with WRITE_OWNER and a valid SACL.
            let code = unsafe {
                SetSecurityInfo(
                    self.handle,
                    SE_WINDOW_OBJECT,
                    LABEL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                    acl.as_ptr().cast(),
                )
            };
            if code != ERROR_SUCCESS {
                return Err(win32("SetSecurityInfo(desktop label)", code));
            }
            Ok(())
        }
    }

    impl Drop for Desktop {
        fn drop(&mut self) {
            // SAFETY: a desktop handle this struct owns. The desktop itself
            // lives on while a process still uses it.
            unsafe { CloseDesktop(self.handle) };
        }
    }

    /// One private desktop per workspace, reused across spawns. Held for
    /// the process lifetime; a worktree is short-lived and few run at once.
    fn desktop_for(key: &str, user: &Sid) -> Result<std::sync::Arc<Desktop>, SandboxError> {
        use std::sync::{Arc, Mutex, OnceLock};
        static DESKTOPS: OnceLock<Mutex<std::collections::HashMap<String, Arc<Desktop>>>> =
            OnceLock::new();
        let map = DESKTOPS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let mut map = map.lock().map_err(|_| SandboxError::PlatformSetup {
            step: "lock the private-desktop cache",
            source: io::Error::other("the desktop cache mutex was poisoned"),
        })?;
        if let Some(desktop) = map.get(key) {
            return Ok(Arc::clone(desktop));
        }
        let desktop = Arc::new(Desktop::create(user)?);
        map.insert(key.to_string(), Arc::clone(&desktop));
        Ok(desktop)
    }

    /// A name suffix that is unique enough to avoid collisions between
    /// concurrent sandboxes. Not a security value: the DACL is the boundary.
    fn random_u64() -> u64 {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        h.finish()
    }

    // ---- stdio ----------------------------------------------------------------

    /// One standard stream: the handle the child inherits and, when piped,
    /// the parent's end.
    struct Stream {
        child: Option<OwnedHandle>,
        parent: Option<OwnedHandle>,
    }

    fn inheritable(h: HANDLE, step: &'static str) -> Result<OwnedHandle, SandboxError> {
        let mut dup: HANDLE = ptr::null_mut();
        // SAFETY: duplicates a valid handle within this process.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h,
                GetCurrentProcess(),
                &mut dup,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(last_error(step));
        }
        owned(dup, step)
    }

    fn stream(which: STD_HANDLE, redirect: Redirect) -> Result<Stream, SandboxError> {
        match redirect {
            Redirect::Null => {
                let sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: ptr::null_mut(),
                    bInheritHandle: 1,
                };
                let nul = wide(OsStr::new("NUL"));
                // SAFETY: NUL-terminated device name, inheritable handle.
                let h = unsafe {
                    CreateFileW(
                        nul.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        &sa,
                        OPEN_EXISTING,
                        0,
                        ptr::null_mut(),
                    )
                };
                Ok(Stream {
                    child: Some(owned(h, "open NUL")?),
                    parent: None,
                })
            }
            Redirect::Piped => {
                let (mut read, mut write): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
                // SAFETY: outputs written on success; handles not inheritable.
                if unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 0) } == 0 {
                    return Err(last_error("CreatePipe"));
                }
                let read = owned(read, "CreatePipe")?;
                let write = owned(write, "CreatePipe")?;
                let (child, parent) = if which == STD_INPUT_HANDLE {
                    (read, write)
                } else {
                    (write, read)
                };
                Ok(Stream {
                    child: Some(inheritable(child.as_raw_handle(), "DuplicateHandle(pipe)")?),
                    parent: Some(parent),
                })
            }
            Redirect::Inherit => {
                // SAFETY: plain call; the handle is borrowed, not owned.
                let h = unsafe { GetStdHandle(which) };
                if h.is_null() || h == INVALID_HANDLE_VALUE {
                    // No such stream in this process (a GUI app): the child
                    // gets none either, as std does.
                    return Ok(Stream {
                        child: None,
                        parent: None,
                    });
                }
                let mut mode = 0;
                // SAFETY: probes whether `h` is a console handle.
                if which == STD_INPUT_HANDLE && unsafe { GetConsoleMode(h, &mut mode) } != 0 {
                    return Err(SandboxError::PolicyNotEnforceable {
                        backend: BACKEND_NAME,
                        what: "an inherited console input handle (WriteConsoleInput through it \
                               injects keystrokes into the user's console); use Redirect::Null \
                               or Redirect::Piped",
                    });
                }
                Ok(Stream {
                    child: Some(inheritable(h, "DuplicateHandle(std handle)")?),
                    parent: None,
                })
            }
        }
    }

    // ---- program and command line --------------------------------------------

    fn final_env(cmd: &Command) -> (Vec<u16>, Option<OsString>) {
        let inherited: Vec<_> = std::env::vars_os()
            .map(|(k, v)| (k.encode_wide().collect(), v.encode_wide().collect()))
            .collect();
        let overrides: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.encode_wide().collect(),
                    v.map(|v| v.encode_wide().collect()),
                )
            })
            .collect();
        let block = env_block(inherited, overrides);
        // PATH as the child will see it, for program lookup.
        let path = cmd
            .get_envs()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.map(OsStr::to_os_string))
            .unwrap_or_else(|| std::env::var_os("PATH"));
        (block, path)
    }

    fn with_exe(candidate: PathBuf) -> Option<PathBuf> {
        if candidate.is_file() {
            return Some(candidate);
        }
        if candidate.extension().is_none() {
            let exe = candidate.with_extension("exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
        None
    }

    /// Resolve the program against the child's PATH (not the parent's, and
    /// never the current directory), so `lpApplicationName` is exact and a
    /// planted binary in the working directory is not picked up.
    fn resolve_program(
        program: &OsStr,
        cwd: &Path,
        path: Option<&OsStr>,
    ) -> Result<PathBuf, SandboxError> {
        let p = Path::new(program);
        let found = if p.is_absolute() {
            with_exe(p.to_path_buf())
        } else if p.components().count() > 1 {
            with_exe(cwd.join(p))
        } else {
            path.into_iter()
                .flat_map(std::env::split_paths)
                .find_map(|dir| with_exe(dir.join(p)))
        };
        found.ok_or_else(|| {
            SandboxError::Spawn(io::Error::new(
                io::ErrorKind::NotFound,
                format!("program {} not found", p.display()),
            ))
        })
    }

    // ---- spawn ----------------------------------------------------------------

    /// A running sandboxed process. Dropping it closes the job, which kills
    /// the process and every descendant still running (the teardown
    /// guarantee); std's `Child` would leave them running.
    #[derive(Debug)]
    pub struct SandboxedChild {
        pub stdin: Option<ChildStdin>,
        pub stdout: Option<ChildStdout>,
        pub stderr: Option<ChildStderr>,
        process: OwnedHandle,
        pid: u32,
        job: OwnedHandle,
        /// Keeps the (shared, per-workspace) desktop alive while the child runs.
        _desktop: DesktopGuard,
    }

    /// Debug-printable wrapper so `SandboxedChild` can derive Debug.
    struct DesktopGuard(#[allow(dead_code)] std::sync::Arc<Desktop>);

    impl std::fmt::Debug for DesktopGuard {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Desktop")
        }
    }

    impl SandboxedChild {
        pub fn id(&self) -> u32 {
            self.pid
        }

        /// The process handle, for waiting from an async runtime.
        pub fn process_handle(&self) -> BorrowedHandle<'_> {
            // SAFETY: the handle is owned by `self` and outlives the borrow.
            unsafe { BorrowedHandle::borrow_raw(self.process.as_raw_handle()) }
        }

        fn exit_status(&self) -> io::Result<ExitStatus> {
            let mut code = 0u32;
            // SAFETY: a valid process handle with query access.
            if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(ExitStatus::from_raw(code))
        }

        /// Wait for the process to exit. Closes stdin first, as std does.
        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            drop(self.stdin.take());
            // SAFETY: a valid process handle.
            match unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } {
                WAIT_OBJECT_0 => self.exit_status(),
                _ => Err(io::Error::last_os_error()),
            }
        }

        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            // SAFETY: a valid process handle.
            match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => self.exit_status().map(Some),
                WAIT_TIMEOUT => Ok(None),
                _ => Err(io::Error::last_os_error()),
            }
        }

        /// Terminate the process and all its descendants.
        pub fn kill(&mut self) -> io::Result<()> {
            // SAFETY: a valid job handle with terminate access.
            if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Collect stdout and stderr, wait, then kill any descendant still
        /// running so a background process holding a pipe cannot block the
        /// read forever.
        pub fn wait_with_output(mut self) -> io::Result<Output> {
            drop(self.stdin.take());
            let reader = |pipe: Option<Box<dyn Read + Send>>| {
                std::thread::spawn(move || -> io::Result<Vec<u8>> {
                    let mut buf = Vec::new();
                    if let Some(mut pipe) = pipe {
                        pipe.read_to_end(&mut buf)?;
                    }
                    Ok(buf)
                })
            };
            let out = reader(
                self.stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
            );
            let err = reader(
                self.stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
            );
            let status = self.wait()?;
            self.kill()?;
            let join = |h: std::thread::JoinHandle<io::Result<Vec<u8>>>| {
                h.join()
                    .map_err(|_| io::Error::other("pipe reader thread panicked"))?
            };
            Ok(Output {
                status,
                stdout: join(out)?,
                stderr: join(err)?,
            })
        }
    }

    /// Owns a PROC_THREAD_ATTRIBUTE_LIST and the handle array it points to.
    struct AttributeList {
        buf: Vec<u64>,
        _handles: Box<[HANDLE]>,
    }

    impl AttributeList {
        /// A list with one attribute: the exact set of handles to inherit.
        fn with_handles(handles: Vec<HANDLE>) -> Result<Self, SandboxError> {
            let mut size = 0usize;
            // SAFETY: size query with a null list.
            unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
            if size == 0 {
                return Err(last_error("InitializeProcThreadAttributeList size"));
            }
            let mut list = Self {
                buf: vec![0u64; size.div_ceil(8)],
                _handles: handles.into_boxed_slice(),
            };
            // SAFETY: `buf` holds `size` bytes.
            if unsafe { InitializeProcThreadAttributeList(list.as_ptr(), 1, 0, &mut size) } == 0 {
                // Not initialized: Drop must not delete it.
                list.buf.clear();
                return Err(last_error("InitializeProcThreadAttributeList"));
            }
            // SAFETY: the boxed slice has a stable address for the life of
            // the list, and the size covers it.
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    list.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    list._handles.as_ptr().cast(),
                    std::mem::size_of_val(&*list._handles),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if ok == 0 {
                return Err(last_error("UpdateProcThreadAttribute(handle list)"));
            }
            Ok(list)
        }

        fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buf.as_mut_ptr().cast()
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.buf.is_empty() {
                // SAFETY: an initialized list, deleted once.
                unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
            }
        }
    }

    /// Start `cmd` under the prepared restrictions. `stdio` is stdin, stdout,
    /// stderr.
    pub(in crate::sandbox) fn spawn(
        cmd: &Command,
        prepared: &Prepared,
        stdio: [Redirect; 3],
    ) -> Result<SandboxedChild, SandboxError> {
        let cwd = cmd.get_current_dir().ok_or_else(|| {
            SandboxError::InvalidPolicy("sandboxed command has no working directory".into())
        })?;
        let (env, path) = final_env(cmd);
        let program = resolve_program(cmd.get_program(), cwd, path.as_deref())?;
        let program_text = program.to_string_lossy();
        if is_batch_file(&program_text) {
            return Err(SandboxError::Spawn(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch files are refused: cmd.exe parses their arguments differently; \
                 run cmd.exe /d /c explicitly",
            )));
        }
        if program_text.contains('"') {
            return Err(SandboxError::Spawn(io::Error::new(
                io::ErrorKind::InvalidInput,
                "program path contains a quote",
            )));
        }
        let mut line: Vec<u16> = Vec::new();
        // argv[0] is always quoted and never escaped, as std does.
        line.push(u16::from(b'"'));
        line.extend(cmd.get_program().encode_wide());
        line.push(u16::from(b'"'));
        for arg in cmd.get_args() {
            append_arg(&mut line, &arg.encode_wide().collect::<Vec<_>>());
        }
        line.push(0);

        let (token, _logon, user) = build_sandbox_token(&prepared.caps)?;
        let desktop = desktop_for(&prepared.desktop_key, &user)?;
        let job = create_job()?;
        let [stdin, stdout, stderr] = [
            stream(STD_INPUT_HANDLE, stdio[0])?,
            stream(STD_OUTPUT_HANDLE, stdio[1])?,
            stream(STD_ERROR_HANDLE, stdio[2])?,
        ];
        let raw = |s: &Stream| {
            s.child
                .as_ref()
                .map_or(ptr::null_mut(), |h| h.as_raw_handle())
        };
        let handles: Vec<HANDLE> = [&stdin, &stdout, &stderr]
            .iter()
            .filter_map(|s| s.child.as_ref().map(|h| h.as_raw_handle()))
            .collect();
        // Inherit exactly the intended handles through the handle list, and
        // never rely on global inheritance: bInheritHandles is TRUE only with a
        // handle list present, and FALSE otherwise. The listed handles
        // must be marked inheritable, so a concurrent CreateProcess elsewhere
        // in this process that passes bInheritHandles=TRUE without a list could
        // still inherit them during the brief window they are open; std has the
        // same race, and the handle list is the accepted mitigation.
        let inherit = !handles.is_empty();
        let mut attrs = if inherit {
            Some(AttributeList::with_handles(handles)?)
        } else {
            None
        };

        // SAFETY: zeroed is a valid initial STARTUPINFOEXW.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.lpDesktop = desktop.startup_name.as_ptr().cast_mut();
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = raw(&stdin);
        si.StartupInfo.hStdOutput = raw(&stdout);
        si.StartupInfo.hStdError = raw(&stderr);
        if let Some(attrs) = attrs.as_mut() {
            si.lpAttributeList = attrs.as_ptr();
        }
        let app = wide(program.as_os_str());
        let cwd_w = wide(cwd.as_os_str());
        // SAFETY: zeroed is a valid initial PROCESS_INFORMATION.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // CREATE_NO_WINDOW: no console is attached, so the child cannot reach
        // the user's console (the Windows counterpart of `setsid` on Unix).
        // CREATE_SUSPENDED: it joins the job before running any code.
        let flags = CREATE_SUSPENDED
            | CREATE_UNICODE_ENVIRONMENT
            | CREATE_NO_WINDOW
            | if inherit {
                EXTENDED_STARTUPINFO_PRESENT
            } else {
                0
            };
        // SAFETY: every pointer is NUL-terminated or a valid struct that
        // outlives the call; `line` is mutable as the API requires.
        let ok = unsafe {
            CreateProcessAsUserW(
                token.as_raw_handle(),
                app.as_ptr(),
                line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                i32::from(inherit),
                flags,
                env.as_ptr().cast(),
                cwd_w.as_ptr(),
                ptr::addr_of!(si.StartupInfo),
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(SandboxError::Spawn(io::Error::last_os_error()));
        }
        let process = owned(pi.hProcess, "CreateProcessAsUserW process")?;
        let thread = owned(pi.hThread, "CreateProcessAsUserW thread")?;
        // SAFETY: valid job and process handles.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
            let err = last_error("AssignProcessToJobObject");
            // SAFETY: the suspended process never ran; terminate it.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return Err(err);
        }
        // SAFETY: the primary thread handle of the suspended process.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            let err = last_error("ResumeThread");
            // SAFETY: valid job handle.
            unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
            return Err(err);
        }
        drop(attrs);
        Ok(SandboxedChild {
            stdin: stdin.parent.map(ChildStdin::from),
            stdout: stdout.parent.map(ChildStdout::from),
            stderr: stderr.parent.map(ChildStderr::from),
            process,
            pid: pi.dwProcessId,
            job,
            _desktop: DesktopGuard(desktop),
        })
    }

    pub(in crate::sandbox) fn output(
        cmd: &Command,
        prepared: &Prepared,
        stdio: [Option<Redirect>; 3],
    ) -> Result<Output, SandboxError> {
        let child = spawn(
            cmd,
            prepared,
            [
                stdio[0].unwrap_or(Redirect::Null),
                stdio[1].unwrap_or(Redirect::Piped),
                stdio[2].unwrap_or(Redirect::Piped),
            ],
        )?;
        child.wait_with_output().map_err(SandboxError::Spawn)
    }

    pub(in crate::sandbox) fn status(
        cmd: &Command,
        prepared: &Prepared,
        stdio: [Option<Redirect>; 3],
    ) -> Result<ExitStatus, SandboxError> {
        let mut child = spawn(cmd, prepared, default_stdio(stdio))?;
        child.wait().map_err(SandboxError::Spawn)
    }

    /// Defaults for `spawn` and `status`: stdin is null (an inherited console
    /// input handle is refused, see [`stream`]); stdout and stderr inherit.
    pub(in crate::sandbox) fn default_stdio(stdio: [Option<Redirect>; 3]) -> [Redirect; 3] {
        [
            stdio[0].unwrap_or(Redirect::Null),
            stdio[1].unwrap_or(Redirect::Inherit),
            stdio[2].unwrap_or(Redirect::Inherit),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let mut out = Vec::new();
        for a in args {
            append_arg(&mut out, &a.encode_utf16().collect::<Vec<_>>());
        }
        String::from_utf16(&out).unwrap()
    }

    #[test]
    fn arguments_follow_msvc_quoting() {
        assert_eq!(line(&["a", "b c"]), r#"a "b c""#);
        assert_eq!(line(&[""]), r#""""#);
        assert_eq!(line(&[r#"say "hi""#]), r#""say \"hi\"""#);
        // Backslashes are literal unless they precede a quote.
        assert_eq!(line(&[r"C:\dir\"]), r"C:\dir\");
        assert_eq!(line(&[r"C:\my dir\"]), r#""C:\my dir\\""#);
        assert_eq!(line(&[r#"a\"b"#]), r#"a\\\"b"#);
    }

    #[test]
    fn capability_sids_are_stable_distinct_and_case_insensitive() {
        let a = capability_sid_string(r"C:\ws\one");
        assert_eq!(a, capability_sid_string(r"C:\WS\ONE"));
        assert_ne!(a, capability_sid_string(r"C:\ws\two"));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(&parts[..4], &["S", "1", "5", "21"]);
        assert_eq!(parts.len(), 8);
        assert!(parts[4..].iter().all(|p| p.parse::<u32>().is_ok()), "{a}");
    }

    #[test]
    fn batch_files_are_detected() {
        assert!(is_batch_file(r"C:\x\build.BAT"));
        assert!(is_batch_file("run.cmd"));
        assert!(!is_batch_file("cargo.exe"));
    }

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn env_block_overrides_and_removes_without_case() {
        let block = env_block(
            vec![
                (w("Path"), w("C:\\bin")),
                (w("GITHUB_TOKEN"), w("secret")),
                (w("b"), w("1")),
            ],
            vec![(w("PATH"), Some(w("C:\\new"))), (w("github_token"), None)],
        );
        let text = String::from_utf16(&block).unwrap();
        assert_eq!(text, "b=1\0PATH=C:\\new\0\0");
    }

    #[test]
    fn empty_env_block_is_two_nuls() {
        assert_eq!(env_block(vec![], vec![]), vec![0, 0]);
    }
}
