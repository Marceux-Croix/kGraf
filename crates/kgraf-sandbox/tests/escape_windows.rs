//! Escape attempts against the real Windows backend (acceptance: "verified by an
//! escape attempt test on each supported platform").
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for x86_64-pc-windows-msvc only. Run it
//! on Windows 10 or later, as a normal (non-elevated) user, with git on PATH:
//! `cargo test -p kgraf-sandbox --test escape_windows -- --test-threads=1`.
//! Single-threaded because several tests set process environment variables.
//!
//! Two things a run of this file must get right. Set
//! `KGRAF_REQUIRE_SANDBOX_USER=1` where the run is about the sandbox user
//! (decision 0097): without it, a machine where setup has not run takes the
//! token sandbox branch of every test and the run passes without testing
//! what it meant to. And keep the build output outside the owner's profile
//! (`CARGO_TARGET_DIR`): the probe is this test binary, a sandbox user
//! cannot read the owner's profile, and [`self_exe`] fails loud rather than
//! let every probe die before it runs.
//!
//! Three behaviors the sandboxing evidence flags as unverified must be watched
//! on the first real run: whether cargo and the MSVC linker run at the Low
//! integrity level (`a_real_cargo_build_runs_inside_the_sandbox`); whether the
//! Low label on the writable roots lets the toolchain write there; and the
//! loopback exemption tool, which this backend does not use.
//!
//! The probe is this test binary itself: [`probe_entry`] does nothing in a
//! normal run, and when `KGRAF_PROBE` is set it performs one operation,
//! prints `PROBE=<result>` and exits: 0 success, 5 access denied
//! (ERROR_ACCESS_DENIED), 3 any other failure. No quoting through `cmd.exe`.

#![cfg(windows)]

mod common;

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use common::{fixture, sandbox_user_mode};
use kgraf_sandbox::{
    Backend, Network, ReadConfinement, Redirect, SandboxError, SandboxPolicy, WorkerSlot,
    Workspace, WorkspaceManager, WorkspaceSpec, link_dependency_trees, platform_isolation,
    sandboxed_command, worker_slot,
};

const DENIED: i32 = 5;
const OTHER_ERR: i32 = 3;

// ---- probe side -------------------------------------------------------------

fn exit_with(result: std::io::Result<String>) -> ! {
    match result {
        Ok(text) => {
            println!("PROBE={text}");
            std::process::exit(0)
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5) =>
        {
            println!("PROBE=denied {e}");
            std::process::exit(DENIED)
        }
        Err(e) => {
            println!("PROBE=error {e}");
            std::process::exit(OTHER_ERR)
        }
    }
}

fn arg2() -> String {
    std::env::var("KGRAF_PROBE_ARG2").unwrap_or_default()
}

/// What a probe does inside the sandbox: one name for both halves of this
/// file. The host passes a value of this type, the probe's dispatcher matches
/// on it, and the match is exhaustive, so an action the probe does not
/// perform cannot be asked for and a new one cannot be forgotten. A plain
/// string on the host side gave `unknown probe <name>` at run time instead,
/// and read like a confinement failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Write,
    Delete,
    Rename,
    Read,
    OpenWriteDac,
    OpenWriteOwner,
    Env,
    Sleep,
    SpawnSleeper,
    Breakaway,
    OpenProcessForWrite,
    OpenProcessVmRead,
    HardlinkWrite,
    RegwriteRunKey,
    OpenPipe,
    AttachConsole,
    SendInputHere,
    JunctionWrite,
    UserSid,
    Connect,
    UncList,
    Ui,
    Console,
    NamedSemaphore,
    NamedSemaphoreGlobal,
    OpenSessionDirectory,
    StatAncestors,
    OpenJobserver,
}

impl Act {
    /// Every action, so a name coming back from the environment finds its
    /// value. A variant left out here fails loudly in the probe.
    const ALL: [Act; 28] = [
        Act::Write,
        Act::Delete,
        Act::Rename,
        Act::Read,
        Act::OpenWriteDac,
        Act::OpenWriteOwner,
        Act::Env,
        Act::Sleep,
        Act::SpawnSleeper,
        Act::Breakaway,
        Act::OpenProcessForWrite,
        Act::OpenProcessVmRead,
        Act::HardlinkWrite,
        Act::RegwriteRunKey,
        Act::OpenPipe,
        Act::AttachConsole,
        Act::SendInputHere,
        Act::JunctionWrite,
        Act::UserSid,
        Act::Connect,
        Act::UncList,
        Act::Ui,
        Act::Console,
        Act::NamedSemaphore,
        Act::NamedSemaphoreGlobal,
        Act::OpenSessionDirectory,
        Act::StatAncestors,
        Act::OpenJobserver,
    ];

    fn name(self) -> &'static str {
        match self {
            Act::Write => "write",
            Act::Delete => "delete",
            Act::Rename => "rename",
            Act::Read => "read",
            Act::OpenWriteDac => "open_write_dac",
            Act::OpenWriteOwner => "open_write_owner",
            Act::Env => "env",
            Act::Sleep => "sleep",
            Act::SpawnSleeper => "spawn_sleeper",
            Act::Breakaway => "breakaway",
            Act::OpenProcessForWrite => "open_process_for_write",
            Act::OpenProcessVmRead => "open_process_vm_read",
            Act::HardlinkWrite => "hardlink_write",
            Act::RegwriteRunKey => "regwrite_run_key",
            Act::OpenPipe => "open_pipe",
            Act::AttachConsole => "attach_console",
            Act::SendInputHere => "send_input_here",
            Act::JunctionWrite => "junction_write",
            Act::UserSid => "user_sid",
            Act::Connect => "connect",
            Act::UncList => "unc_list",
            Act::Ui => "ui",
            Act::Console => "console",
            Act::NamedSemaphore => "named_semaphore",
            Act::NamedSemaphoreGlobal => "named_semaphore_global",
            Act::OpenSessionDirectory => "open_session_directory",
            Act::StatAncestors => "stat_ancestors",
            Act::OpenJobserver => "open_jobserver",
        }
    }

    fn from_name(name: &str) -> Option<Act> {
        Act::ALL.into_iter().find(|a| a.name() == name)
    }
}

/// The in-sandbox half. A no-op unless `KGRAF_PROBE` is set.
#[test]
fn probe_entry() {
    let Ok(action) = std::env::var("KGRAF_PROBE") else {
        return;
    };
    let arg = std::env::var("KGRAF_PROBE_ARG").unwrap_or_default();
    let Some(act) = Act::from_name(&action) else {
        exit_with(Err(std::io::Error::other(format!(
            "{action} is no probe action of this test binary"
        ))))
    };
    let result = match act {
        Act::Write => std::fs::write(&arg, b"escaped").map(|()| "written".into()),
        Act::Delete => std::fs::remove_file(&arg).map(|()| "deleted".into()),
        Act::Rename => std::fs::rename(&arg, format!("{arg}.moved")).map(|()| "renamed".into()),
        Act::Read => std::fs::read_to_string(&arg),
        // WRITE_DAC and WRITE_OWNER are not in FILE_GENERIC_WRITE. If a
        // write-restricted token did not treat them as writes, the owner's
        // implicit WRITE_DAC would let the sandbox grant itself access to any
        // file the user owns.
        Act::OpenWriteDac => open_with(&arg, 0x0004_0000),
        Act::OpenWriteOwner => open_with(&arg, 0x0008_0000),
        Act::Env => Ok(std::env::var(&arg).unwrap_or_else(|_| "<unset>".into())),
        Act::Sleep => {
            std::thread::sleep(std::time::Duration::from_secs(120));
            Ok("woke".into())
        }
        Act::SpawnSleeper => spawn_self(Act::Sleep, "", 0).map(|child| child.id().to_string()),
        Act::Breakaway => {
            // CREATE_BREAKAWAY_FROM_JOB: the job has no BREAKAWAY_OK flag.
            spawn_self(Act::Sleep, "", 0x0100_0000).map(|c| format!("escaped pid {}", c.id()))
        }
        Act::OpenProcessForWrite => open_process(arg.parse().unwrap_or(0), true),
        Act::OpenProcessVmRead => open_process(arg.parse().unwrap_or(0), false),
        Act::HardlinkWrite => hardlink_write(&arg, &arg2()),
        Act::RegwriteRunKey => reg_write_run_key(&arg, &arg2()),
        Act::OpenPipe => open_with(&arg, 0xC000_0000), // GENERIC_READ | GENERIC_WRITE
        Act::AttachConsole => attach_console(arg.parse().unwrap_or(0)),
        Act::JunctionWrite => junction_write(&arg, &arg2()),
        Act::UserSid => own_user_sid(),
        Act::Connect => connect_to(&arg),
        Act::UncList => unc_list(&arg),
        Act::Ui => ui_objects(),
        Act::SendInputHere => send_input_here(),
        Act::Console => Ok(console_window().to_string()),
        Act::NamedSemaphore => named_semaphore(""),
        Act::NamedSemaphoreGlobal => named_semaphore("Global\\"),
        Act::OpenSessionDirectory => open_session_directory(&arg),
        Act::StatAncestors => stat_ancestors(&arg),
        Act::OpenJobserver => open_jobserver(),
    };
    exit_with(result)
}

/// Create a named semaphore whose name starts with `prefix`: with no prefix
/// in the session's `BaseNamedObjects`, as the `jobserver` crate does when
/// cargo or rustc finds no jobserver to join; with `Global\` in the global
/// namespace, which creating a semaphore needs no privilege for (only a
/// file mapping or a symbolic link there needs `SeCreateGlobalPrivilege`).
fn named_semaphore(prefix: &str) -> std::io::Result<String> {
    use windows_sys::Win32::System::Threading::CreateSemaphoreW;
    let name = format!("{prefix}kgraf-escape-probe-{}", std::process::id());
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name and no security attributes.
    let h = unsafe { CreateSemaphoreW(std::ptr::null(), 1, 1, wide.as_ptr()) };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh handle, closed once.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    Ok(format!("created {name}"))
}

/// The rights `kernelbase.dll` first opens the session's object directory
/// with: `DIRECTORY_QUERY | DIRECTORY_TRAVERSE | DIRECTORY_CREATE_OBJECT |
/// DIRECTORY_CREATE_SUBDIRECTORY`.
const DIRECTORY_AS_KERNELBASE: u32 = 0x000f;
/// `DIRECTORY_TRAVERSE` alone: what it asks for when that open fails, before
/// it tries the directory's `Restricted` subdirectory.
const DIRECTORY_TRAVERSE: u32 = 0x0001;

/// This process's session's object directory: `\Sessions\<n>\BaseNamedObjects`,
/// or `\BaseNamedObjects` in session 0.
fn session_directory() -> String {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }
    let mut id = 0u32;
    // SAFETY: an out pointer to a local.
    let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut id) };
    if ok == 0 || id == 0 {
        r"\BaseNamedObjects".to_owned()
    } else {
        format!(r"\Sessions\{id}\BaseNamedObjects")
    }
}

/// Open the object directory `path` with `access` (`NtOpenDirectoryObject`).
/// The handle on success; the `NTSTATUS` otherwise, STATUS_ACCESS_DENIED as
/// the Win32 error 5, so the probe reports it as denied.
fn open_directory(path: &str, access: u32) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::NtOpenDirectoryObject;
    use windows_sys::Win32::Foundation::UNICODE_STRING;
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    let bytes = u16::try_from(wide.len() * 2).map_err(std::io::Error::other)?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        ObjectName: &name,
        // OBJ_CASE_INSENSITIVE, as kernelbase opens it.
        Attributes: 0x40,
        ..Default::default()
    };
    let mut handle = std::ptr::null_mut();
    // SAFETY: `name` and `wide` outlive the call; the handle is an out
    // pointer, owned below on success.
    let status = unsafe { NtOpenDirectoryObject(&mut handle, access, &attributes) };
    if status >= 0 {
        // SAFETY: a fresh handle this code now owns.
        return Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) });
    }
    if status as u32 == 0xC000_0022 {
        return Err(std::io::Error::from_raw_os_error(5));
    }
    Err(std::io::Error::other(format!(
        "NTSTATUS 0x{:08x}",
        status as u32
    )))
}

/// Open the session's object directory with the rights in `access`, a hex
/// number, as `kernelbase.dll` does before it looks up any object name.
fn open_session_directory(access: &str) -> std::io::Result<String> {
    let mask = u32::from_str_radix(access.trim_start_matches("0x"), 16)
        .map_err(|e| std::io::Error::other(format!("{access}: {e}")))?;
    let path = session_directory();
    open_directory(&path, mask).map(|_| format!("opened {path} for 0x{mask:x}"))
}

/// Read the attributes of every folder above `path`, from the drive root
/// down, as git does before it writes a ref (`safe_create_leading_directories`
/// stats each folder of the absolute path, then makes the ones it cannot
/// stat). The first folder whose attributes cannot be read is the error.
fn stat_ancestors(path: &str) -> std::io::Result<String> {
    let path = Path::new(path);
    let mut above: Vec<&Path> = path.ancestors().skip(1).collect();
    above.reverse();
    // The drive root itself is never made, so git does not stat it.
    for folder in above.into_iter().skip(1) {
        if let Err(e) = std::fs::metadata(folder) {
            return Err(std::io::Error::new(
                e.kind(),
                format!("{}: {e}", folder.display()),
            ));
        }
    }
    Ok(format!("every folder above {} was read", path.display()))
}

/// The owner's reading of an object directory's owner, access list and
/// label, in SDDL, or why it could not be read.
fn directory_sddl(path: &str) -> String {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::GetKernelObjectSecurity;
    // READ_CONTROL.
    let handle = match open_directory(path, 0x0002_0000) {
        Ok(h) => h,
        Err(e) => return format!("not read: {e}"),
    };
    // OWNER, GROUP, DACL and LABEL.
    let what = 0x1 | 0x2 | 0x4 | 0x10;
    let mut needed = 0u32;
    // SAFETY: a size query with no buffer; the call fills `needed`.
    unsafe {
        GetKernelObjectSecurity(
            handle.as_raw_handle(),
            what,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    let mut sd = vec![0u8; needed as usize];
    // SAFETY: a buffer of the size the query above asked for.
    if unsafe {
        GetKernelObjectSecurity(
            handle.as_raw_handle(),
            what,
            sd.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return format!("not read: {}", std::io::Error::last_os_error());
    }
    let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `sd` holds the descriptor read above; the call allocates
    // `text`, freed below.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd.as_mut_ptr().cast(),
            SDDL_REVISION_1,
            what,
            &mut text,
            &mut len,
        )
    } == 0
    {
        return format!("not converted: {}", std::io::Error::last_os_error());
    }
    // SAFETY: `text` holds `len` UTF-16 units, the terminator included.
    let units = unsafe { std::slice::from_raw_parts(text, len as usize) };
    let sddl = String::from_utf16_lossy(units)
        .trim_end_matches('\0')
        .to_owned();
    // SAFETY: frees the string the conversion allocated, once.
    unsafe { LocalFree(text as HLOCAL) };
    sddl
}

/// Open the jobserver `CARGO_MAKEFLAGS` names, with the rights the
/// `jobserver` crate asks for (`SYNCHRONIZE | SEMAPHORE_MODIFY_STATE`).
fn open_jobserver() -> std::io::Result<String> {
    use windows_sys::Win32::System::Threading::{OpenSemaphoreW, SEMAPHORE_MODIFY_STATE};
    let flags = std::env::var("CARGO_MAKEFLAGS").map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "CARGO_MAKEFLAGS is not set")
    })?;
    let name = flags
        .split_whitespace()
        .find_map(|w| w.strip_prefix("--jobserver-auth="))
        .ok_or_else(|| std::io::Error::other(format!("no jobserver in {flags:?}")))?
        .to_owned();
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name; SYNCHRONIZE is 0x00100000.
    let h = unsafe { OpenSemaphoreW(0x0010_0000 | SEMAPHORE_MODIFY_STATE, 0, wide.as_ptr()) };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh handle, closed once.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    Ok(format!("opened {name}"))
}

fn spawn_self(action: Act, arg: &str, flags: u32) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_PROBE", action.name())
        .env("KGRAF_PROBE_ARG", arg)
        .creation_flags(flags)
        .stdout(std::process::Stdio::null())
        .spawn()
}

fn open_with(path: &str, access: u32) -> std::io::Result<String> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .access_mode(access)
        // Needed to open a directory handle.
        .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
        .open(path)
        .map(|_| "opened".into())
}

/// The SID of the user this process runs as, in string form. The sandbox
/// user path starts a command as another user, and this is how a test tells
/// which one.
fn own_user_sid() -> std::io::Result<String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut raw: HANDLE = std::ptr::null_mut();
    // SAFETY: opens this process's own token for reading.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh handle this code now owns.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut needed = 0u32;
    // SAFETY: size query with a null buffer.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let user = buf.as_ptr() as *const TOKEN_USER;
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the system wrote a TOKEN_USER whose SID points into `buf`; the
    // call allocates `text`, freed below.
    if unsafe { ConvertSidToStringSidW((*user).User.Sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut len = 0usize;
    // SAFETY: a NUL-terminated string the call allocated.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read from `text`.
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: frees the string the conversion allocated.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(out)
}

/// Open a connection to `addr` (`127.0.0.1:port`), so a test can see whether
/// a command that must not reach the network can reach a listener.
fn connect_to(addr: &str) -> std::io::Result<String> {
    let addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|_| std::io::Error::other(format!("{addr} is not an address")))?;
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(5))
        .map(|_| "connected".into())
}

/// List a UNC path. Windows connects on the command's behalf: the kernel's
/// SMB client for `\\host\share`, the WebClient service for the WebDAV form
/// `\\host@port\share`. Neither runs as the sandbox user, so the firewall
/// rules scoped to that user do not cover what they send.
fn unc_list(path: &str) -> std::io::Result<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
        if names.len() == 4 {
            break;
        }
    }
    Ok(format!("listed {names:?}"))
}

fn open_process(pid: u32, for_write: bool) -> std::io::Result<String> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_CREATE_THREAD, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
    };
    let access = if for_write {
        PROCESS_VM_WRITE | PROCESS_VM_OPERATION | PROCESS_CREATE_THREAD
    } else {
        PROCESS_VM_READ
    };
    // SAFETY: plain call; the handle, if any, is closed below.
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a handle just returned to us.
    drop(unsafe {
        <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(
            h,
        )
    });
    Ok("opened".into())
}

fn hardlink_write(link: &str, target: &str) -> std::io::Result<String> {
    // A hard link inside the root pointing at an outside file shares the file's
    // data; a write through the link would rewrite the outside file.
    std::fs::hard_link(target, link)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(link)?
        .write_all(b"escaped-via-hardlink")?;
    Ok("wrote through hardlink".into())
}

fn junction_write(link: &str, target_dir: &str) -> std::io::Result<String> {
    // mklink /J creates a directory junction without any privilege. Writing a
    // file under it would land in the outside target directory.
    let status = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J", link, target_dir])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("mklink /J failed"));
    }
    let planted = Path::new(link).join("planted.txt");
    std::fs::write(&planted, b"escaped-via-junction")?;
    Ok("wrote through junction".into())
}

/// Write a value in a Run key, which would start a program at the next
/// logon. `owner_sid` names whose key: under `HKEY_USERS` that is the
/// owner's own hive, the asset at risk. A command's own `HKEY_CURRENT_USER`
/// is not it: a sandbox user runs with no profile loaded, so its
/// `HKEY_CURRENT_USER` is the default user's and a write there says nothing
/// about the owner.
fn reg_write_run_key(value_name: &str, owner_sid: &str) -> std::io::Result<String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_USERS, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegOpenKeyExW, RegSetValueExW,
    };
    let sub: Vec<u16> = format!("{owner_sid}\\Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut key = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey; output written on success.
    let rc = unsafe { RegOpenKeyExW(HKEY_USERS, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    let name: Vec<u16> = value_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let data: Vec<u16> = "C:\\evil.exe"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: valid key and NUL-terminated name and data.
    let rc = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    // SAFETY: a key handle we opened.
    unsafe { RegCloseKey(key) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    Ok("run key written".into())
}

fn attach_console(pid: u32) -> std::io::Result<String> {
    use windows_sys::Win32::System::Console::AttachConsole;
    // SAFETY: plain call.
    if unsafe { AttachConsole(pid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok("attached".into())
}

fn send_input() -> std::io::Result<String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_KEYBOARD, SendInput};
    // SAFETY: a zeroed INPUT is a valid keyboard event (VK 0).
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    // SAFETY: one valid INPUT of the stated size.
    let sent = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    // UIPI on a separate, Low-labeled desktop should inject nothing.
    Ok(format!("sent {sent}"))
}

fn desktop_name() -> std::io::Result<String> {
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: the thread's desktop handle need not be closed; `buf` has the
    // stated size in bytes.
    let ok = unsafe {
        GetUserObjectInformationW(
            GetThreadDesktop(GetCurrentThreadId()),
            UOI_NAME,
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

/// The name of the window station this process runs on. On the sandbox user
/// path kGraf's process makes one station for the commands of each sandbox
/// user, named `kgraf-cmd-<hex>`, with one desktop called `Default` on it
/// (`sandbox_user::windows::launch`), so the desktop name alone cannot tell
/// a private desktop from the owner's.
fn station_name() -> std::io::Result<String> {
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_NAME,
    };
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: the process's own station handle need not be closed; `buf` has
    // the stated size in bytes.
    let ok = unsafe {
        GetUserObjectInformationW(
            GetProcessWindowStation(),
            UOI_NAME,
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

/// The window station and the desktop of this process, as
/// `<station>\<desktop>`. Read in one probe: a station read in one command
/// and a desktop read in another say nothing about either process.
fn ui_objects() -> std::io::Result<String> {
    Ok(format!("{}\\{}", station_name()?, desktop_name()?))
}

/// [`send_input`] and the objects it ran on, in one line, for the same
/// reason: a count from one command and a station from another are two
/// processes.
fn send_input_here() -> std::io::Result<String> {
    Ok(format!("{} on {}", send_input()?, ui_objects()?))
}

fn console_window() -> bool {
    // SAFETY: plain call.
    !unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null()
}

// ---- host side ----------------------------------------------------------------

struct Env {
    f: common::Fixture,
    _mgr: WorkspaceManager,
    ws: Workspace,
    other: Workspace,
    outside: tempfile::TempDir,
}

fn env() -> Env {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let spec = |name| WorkspaceSpec {
        name,
        base: "HEAD",
        branch: None,
    };
    let ws = mgr.create(&spec("w")).unwrap();
    let other = mgr.create(&spec("other")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    // Reading the mode here makes KGRAF_REQUIRE_SANDBOX_USER cover every test
    // in this file, the backend-agnostic ones included: all of them build
    // their fixture through this function.
    if sandbox_user_mode() {
        sweep_shared_folders();
        let profile = PathBuf::from(std::env::var_os("USERPROFILE").expect("USERPROFILE"));
        let plain = |p: &Path| kgraf_core::plain_path::canonicalize(p).unwrap_or(p.to_path_buf());
        assert!(
            plain(outside.path()).starts_with(plain(&profile)),
            "the temporary folder {} is outside the owner's profile {}. Every test here that \
             writes \"outside the workspace\" then measures something other than the \
             profile's access list, which is what confines a sandbox user: point TEMP back \
             into the profile, or read each result again",
            outside.path().display(),
            profile.display()
        );
    }
    Env {
        f,
        _mgr: mgr,
        ws,
        other,
        outside,
    }
}

/// Which sandbox user a command runs as: the slot user of its own folder
/// without the network, or the one with it. The pool follows the policy's
/// network (decision 0097), so a probe is run under both where both users
/// must be confined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Offline,
    Online,
}

impl Kind {
    fn network(self) -> Network {
        match self {
            Kind::Offline => Network::Deny,
            Kind::Online => Network::Allow,
        }
    }

    /// The words for a failure message.
    fn word(self) -> &'static str {
        match self {
            Kind::Offline => "as the slot user without the network",
            Kind::Online => "as the slot user with the network",
        }
    }
}

/// A short name for one kind, for a file or value name that must differ
/// between the two runs of a probe.
fn kinds_tag(kind: Kind) -> &'static str {
    match kind {
        Kind::Offline => "offline",
        Kind::Online => "online",
    }
}

/// The kinds a probe runs under on this machine. Both where commands run as
/// a sandbox user: the two pools are two different users, and confinement
/// must hold for each. Only the open-network kind under the token sandbox,
/// which cannot deny the network at all and refuses the policy instead.
fn kinds() -> &'static [Kind] {
    if sandbox_user_mode() {
        &[Kind::Offline, Kind::Online]
    } else {
        &[Kind::Online]
    }
}

/// The policy for one kind. The folder's own worker slot is held for the
/// command's life where there is one, which is how a worker's commands run
/// ([`worker_slot`], [`SandboxPolicy::with_worker_slot`]); the slot is free
/// again when the policy is dropped.
fn policy_as(ws: &Workspace, kind: Kind) -> SandboxPolicy {
    let policy = SandboxPolicy::for_workspace(ws, kind.network()).accept_partial_isolation(true);
    if kind == Kind::Offline
        && let Some(slot) = worker_slot(ws.root()).expect("a worker slot for this folder")
    {
        return policy.with_worker_slot(slot);
    }
    policy
}

/// The open-network policy, which every backend can run.
fn policy(ws: &Workspace) -> SandboxPolicy {
    policy_as(ws, Kind::Online)
}

/// This test binary, which is also the probe. A sandbox user must be able to
/// read and run it, and the owner's profile admits only the owner, SYSTEM and
/// administrators: a target folder there would make every probe fail before
/// it ran a single check, with no probe line to say why.
fn self_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    if sandbox_user_mode() {
        let profile = PathBuf::from(std::env::var_os("USERPROFILE").expect("USERPROFILE"));
        // Compared as paths, each resolved first: a string prefix would read
        // `C:\\Users\\marcotest` as inside `C:\\Users\\marco`, and would miss a
        // verbatim `\\\\?\\C:\\...` form of one side.
        let plain = |p: &Path| kgraf_core::plain_path::canonicalize(p).unwrap_or(p.to_path_buf());
        let under = plain(&exe).starts_with(plain(&profile));
        assert!(
            !under,
            "the probe binary {} is inside the owner's profile {}, which a sandbox user \
             cannot read, so every probe in this file would fail with no result. Set \
             CARGO_TARGET_DIR to a folder outside the profile (C:\\kgraf-target, say) and \
             run the tests again, or copy this binary to a folder every local user may read",
            exe.display(),
            profile.display()
        );
    }
    exe
}

fn probe_output(policy: &SandboxPolicy, cwd: &Path, action: Act, arg: &str, arg2: &str) -> Output {
    sandboxed_command(policy, self_exe(), cwd)
        .unwrap()
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_PROBE", action.name())
        .env("KGRAF_PROBE_ARG", arg)
        .env("KGRAF_PROBE_ARG2", arg2)
        .output()
        .unwrap()
}

/// Run `cmd` to its end, or fail with what it printed so far after `limit`.
/// A program that waits for input nobody gives (a credential prompt, a
/// toolchain installer) would otherwise hold the whole test binary until the
/// outer timeout and hide every other result.
fn output_within(cmd: &mut kgraf_sandbox::SandboxedCommand, limit: Duration) -> Output {
    output_within_quiesce(cmd, limit, QUIESCE)
}

/// [`output_within`] with its own bound on how long the command's processes
/// may take to end after the kill: a thread inside a network redirector is
/// not always terminable at once.
fn output_within_quiesce(
    cmd: &mut kgraf_sandbox::SandboxedCommand,
    limit: Duration,
    quiesce: Duration,
) -> Output {
    cmd.stdin(Redirect::Null)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped);
    wait_within_quiesce(cmd.spawn().unwrap(), limit, quiesce)
}

/// How long the command's processes may take to end after the job is killed.
const QUIESCE: Duration = Duration::from_secs(10);

/// [`output_within`] for a child that is already running.
fn wait_within(child: kgraf_sandbox::SandboxedChild, limit: Duration) -> Output {
    wait_within_quiesce(child, limit, QUIESCE)
}

fn wait_within_quiesce(
    mut child: kgraf_sandbox::SandboxedChild,
    limit: Duration,
    quiesce: Duration,
) -> Output {
    use kgraf_sandbox::output_tail::read_tail;
    // Read with `read`, as the check runners do, not `read_to_end` on the
    // pipe: only the former goes through std's overlapped pipe reads.
    let out = read_tail(child.stdout.take(), 1 << 20);
    let err = read_tail(child.stderr.take(), 1 << 20);
    drop(child.stdin.take());
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    // The job ends every process the command left behind; the pipes then
    // close and the readers finish. Bounded both ways, so a pipe whose end
    // never comes fails this test with what arrived instead of hanging it.
    let quiet = child.kill_tree_and_wait(quiesce).unwrap();
    let (out, err) = (
        out.finish(Duration::from_secs(5)),
        err.finish(Duration::from_secs(5)),
    );
    assert!(
        quiet && out.complete && err.complete,
        "processes gone: {quiet}; stdout whole: {}; stderr whole: {}; status {status:?}; \
         stdout: {}; stderr: {}",
        out.complete,
        err.complete,
        out.text(),
        err.text()
    );
    let (stdout, stderr) = (out.bytes, err.bytes);
    match status {
        Some(status) => Output {
            status,
            stdout,
            stderr,
        },
        None => panic!(
            "no exit within {limit:?}; stdout: {}; stderr: {}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        ),
    }
}

/// [`probe_output`] for a command that the backend may refuse to build: with
/// one sandbox user per pool, a second folder is refused a user while the
/// first holds it, and that refusal is a result, not a failure.
fn try_probe(
    ws: &Workspace,
    kind: Kind,
    action: Act,
    arg: &Path,
) -> Result<(i32, String), SandboxError> {
    let mut cmd = sandboxed_command(&policy_as(ws, kind), self_exe(), ws.root())?;
    let out = cmd
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_PROBE", action.name())
        .env("KGRAF_PROBE_ARG", arg.to_string_lossy().as_ref())
        .env("KGRAF_PROBE_ARG2", "")
        .output()
        .unwrap();
    Ok(parse(&out))
}

/// [`probe_output`] with a time limit, for a probe that can wait on the
/// network.
fn probe_within(
    policy: &SandboxPolicy,
    cwd: &Path,
    action: Act,
    arg: &str,
    limit: Duration,
    quiesce: Duration,
) -> Output {
    let mut cmd = sandboxed_command(policy, self_exe(), cwd).unwrap();
    cmd.args([
        "--exact",
        "probe_entry",
        "--nocapture",
        "--test-threads=1",
        "-q",
    ])
    .env("KGRAF_PROBE", action.name())
    .env("KGRAF_PROBE_ARG", arg)
    .env("KGRAF_PROBE_ARG2", "");
    output_within_quiesce(&mut cmd, limit, quiesce)
}

/// A folder under ProgramData, outside the owner's profile, which a sandbox
/// user can reach: setup denies the sandbox group the right to create entries
/// in ProgramData itself and leaves the folders below it alone (decision
/// 0097), so a folder made here keeps its own access list. Removed when it is
/// dropped.
struct Shared(PathBuf);

impl Shared {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shared_folder(tag: &str) -> Shared {
    let base = PathBuf::from(std::env::var_os("ProgramData").expect("ProgramData"));
    let dir = base.join(format!("kgraf-escape-{}-{tag}", std::process::id()));
    // A run that was killed leaves its folder behind, and `create_dir_all`
    // would reuse it with whatever is in it and whatever access list it got.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        panic!(
            "create {}, which every local user may do under ProgramData: {e}",
            dir.display()
        )
    });
    Shared(dir)
}

/// How long a folder of [`shared_folder`] whose process cannot be read at
/// all may stand before it counts as a leftover. Longer than a whole run of
/// this file, which is minutes.
const STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// Remove the ProgramData folders of earlier runs ([`shared_folder`]). One
/// that a killed run left behind grants every local user write, which is the
/// shape `kgraf sandbox setup` closes, so none is left to find.
fn sweep_shared_folders() {
    let Some(base) = std::env::var_os("ProgramData").map(PathBuf::from) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("kgraf-escape-") else {
            continue;
        };
        // The folder of a run that is still going is left alone: another test
        // binary of this kind may be using it, and deleting it would fail
        // that run's measurements for a reason of this one's making.
        //
        // Windows gives a process id out again once its process is gone, so a
        // live process of that number is not the run that made the folder.
        // The question is settled by the times: a process that started after
        // the folder was made is another one that took the number. Where the
        // process cannot be read at all, the folder's age decides.
        let pid: Option<u32> = rest.split('-').next().and_then(|p| p.parse().ok());
        if pid == Some(std::process::id()) {
            continue;
        }
        let made = entry.metadata().and_then(|m| m.created()).ok();
        let keep = match (pid.and_then(process_started), made) {
            // The process of this number is older than the folder, so it is
            // the run that made it, and that run is still going.
            (Some(started), Some(made)) => started <= made,
            // No times to compare: fall back to the folder's own age.
            _ => made.is_some_and(|t| t.elapsed().unwrap_or_default() < STALE_AFTER),
        };
        if keep {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// When the process of this id started, or `None` where there is no such
/// process or its times cannot be read. Used to tell the run that made a
/// folder from a later process that took its number.
fn process_started(pid: u32) -> Option<std::time::SystemTime> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain call; the handle, if any, is owned below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    // SAFETY: a handle just returned to us.
    let owned = unsafe {
        <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(
            h,
        )
    };
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: a process handle with query access, and four outputs.
    let ok = unsafe {
        GetProcessTimes(
            owned.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    // A FILETIME counts 100-nanosecond units from 1601-01-01.
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let since_unix = ticks.checked_sub(UNIX_EPOCH_TICKS)?;
    std::time::SystemTime::UNIX_EPOCH.checked_add(Duration::from_nanos(since_unix * 100))
}

/// Whether a process with this id is running. A process this one cannot open
/// counts as alive.
fn process_lives(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: plain call; the handle, if any, is owned below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return std::io::Error::last_os_error().raw_os_error()
            != Some(ERROR_INVALID_PARAMETER as i32);
    }
    // SAFETY: a handle just returned to us.
    drop(unsafe {
        <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(
            h,
        )
    });
    true
}

/// Under the sandbox user, a folder outside the owner's profile that every
/// local user may write. An "outside" write refused inside the profile is
/// refused by the profile's access list, which says nothing about the
/// mechanism (a hard link, a junction, a DACL rewrite); in this folder the
/// mechanism is what decides. `None` under the token sandbox, where the
/// integrity label and not the user is the boundary.
fn writable_outside(tag: &str) -> Option<Shared> {
    sandbox_user_mode().then(|| {
        let shared = shared_folder(tag);
        grant_everyone(shared.path());
        shared
    })
}

/// Grant Everyone full control on `dir` and on what is made in it.
fn grant_everyone(dir: &Path) {
    let status = std::process::Command::new("icacls.exe")
        .args([
            dir.to_string_lossy().as_ref(),
            "/grant",
            "*S-1-1-0:(OI)(CI)F",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "icacls grant on {} failed", dir.display());
}

fn probe(ws: &Workspace, kind: Kind, action: Act, arg: &Path) -> (i32, String) {
    parse(&probe_output(
        &policy_as(ws, kind),
        ws.root(),
        action,
        &arg.to_string_lossy(),
        "",
    ))
}

fn probe2(ws: &Workspace, kind: Kind, action: Act, arg: &Path, arg2: &Path) -> (i32, String) {
    parse(&probe_output(
        &policy_as(ws, kind),
        ws.root(),
        action,
        &arg.to_string_lossy(),
        &arg2.to_string_lossy(),
    ))
}

fn parse(out: &Output) -> (i32, String) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("PROBE="))
        .unwrap_or_else(|| panic!("no probe line: {out:?}"))
        .to_string();
    (out.status.code().unwrap_or(-1), line)
}

fn assert_denied(ws: &Workspace, kind: Kind, action: Act, target: &Path) {
    let (code, line) = probe(ws, kind, action, target);
    assert_eq!(
        code,
        DENIED,
        "{} {} {}: {line}",
        action.name(),
        target.display(),
        kind.word()
    );
}

/// A sleeper sandbox in `ws`; returns its OS process id and the running child.
fn spawn_sleeper(ws: &Workspace) -> (u32, kgraf_sandbox::SandboxedChild) {
    let child = sandboxed_command(&policy(ws), self_exe(), ws.root())
        .unwrap()
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_PROBE", Act::Sleep.name())
        .spawn()
        .unwrap();
    (child.id(), child)
}

// ---- policy plumbing ----------------------------------------------------------

/// What the strength indicator says, against the backend that is in use.
/// The two Windows backends enforce different things, and a test that pinned
/// one of them passed on that backend only.
#[test]
fn the_strength_indicator_matches_the_backend_in_use() {
    let s = platform_isolation().unwrap();
    // Reads the sandbox user mode in the same way every other test does, so
    // KGRAF_REQUIRE_SANDBOX_USER is honored here too.
    let as_user = sandbox_user_mode();
    match s.backend {
        Backend::WindowsSandboxUser => {
            assert!(as_user, "the backend and the mode disagree");
            // The firewall rules scoped to the user's SID deny the network;
            // write confinement by access list is not yet verified on
            // Windows, so that flag stays false.
            assert!(s.network_deniable, "{}", s.gaps);
            assert!(!s.writes_confined, "{}", s.gaps);
            assert_eq!(s.reads, ReadConfinement::CredentialDenyList);
            for needle in [
                "not the owner's profile",
                "DNS",
                "UNC path",
                "WebDAV",
                "unverified on Windows",
            ] {
                assert!(s.gaps.contains(needle), "{needle:?} is missing: {}", s.gaps);
            }
        }
        Backend::WindowsRestrictedToken => {
            assert!(!as_user, "the backend and the mode disagree");
            assert!(!s.network_deniable && !s.writes_confined);
            assert_eq!(s.reads, ReadConfinement::Unconfined);
            assert!(
                s.gaps.contains("network egress is not blocked"),
                "{}",
                s.gaps
            );
        }
        other => panic!("{other:?} is not a Windows backend"),
    }
}

/// The opt-in for partial isolation is asked for where isolation is partial.
/// The token sandbox confines no read and denies no network, so a policy must
/// accept that; the sandbox user does neither, so the same policy runs
/// without the opt-in.
#[test]
fn the_opt_in_for_partial_isolation_is_asked_for_only_where_isolation_is_partial() {
    let e = env();
    let result = sandboxed_command(
        &SandboxPolicy::for_workspace(&e.ws, Network::Allow),
        "cmd.exe",
        e.ws.root(),
    );
    if sandbox_user_mode() {
        if let Err(err) = result {
            panic!("the sandbox user asked for the partial-isolation opt-in: {err}");
        }
        return;
    }
    let Err(err) = result else {
        panic!("the token sandbox ran a policy that did not accept its partial isolation");
    };
    assert!(
        matches!(err, SandboxError::PartialIsolation { .. }),
        "{err}"
    );
}

/// A policy the backend cannot enforce is refused, and one it can enforce is
/// not. The token sandbox can deny neither the network nor a socket outside
/// the roots, so it refuses both policies. The sandbox user denies the
/// network with firewall rules, so that policy runs; a unix-socket allowlist
/// is still refused, because nothing there confines a socket.
#[test]
fn a_policy_the_backend_cannot_enforce_is_refused() {
    let e = env();
    let deny = SandboxPolicy::for_workspace(&e.ws, Network::Deny).accept_partial_isolation(true);
    let result = sandboxed_command(&deny, "cmd.exe", e.ws.root());
    if sandbox_user_mode() {
        if let Err(err) = result {
            panic!("the sandbox user refused a network denial it enforces: {err}");
        }
    } else {
        let Err(err) = result else {
            panic!("the token sandbox ran a network denial it cannot enforce");
        };
        assert!(
            matches!(err, SandboxError::PolicyNotEnforceable { .. }),
            "{err}"
        );
    }
    let socket = policy(&e.ws).allow_unix_socket(e.outside.path().join("s.sock"));
    let err = sandboxed_command(&socket, "cmd.exe", e.ws.root()).unwrap_err();
    assert!(
        matches!(err, SandboxError::PolicyNotEnforceable { .. }),
        "{err}"
    );
}

// ---- writes -------------------------------------------------------------------

#[test]
fn write_inside_the_workspace_and_scratch_succeeds() {
    let e = env();
    for &kind in kinds() {
        for target in [e.ws.root().join("new.txt"), e.ws.scratch().join("t.txt")] {
            let (code, line) = probe(&e.ws, kind, Act::Write, &target);
            assert_eq!(code, 0, "{} {line}", kind.word());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "escaped");
            std::fs::remove_file(&target).expect("remove the written file");
        }
    }
}

#[test]
fn write_outside_the_workspace_is_denied() {
    let e = env();
    let target = e.outside.path().join("escaped.txt");
    for &kind in kinds() {
        assert_denied(&e.ws, kind, Act::Write, &target);
        assert!(!target.exists());
    }
}

#[test]
fn write_into_the_user_profile_is_denied() {
    let e = env();
    let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
    let target = profile.join(format!("kgraf-escape-{}.txt", std::process::id()));
    for &kind in kinds() {
        assert_denied(&e.ws, kind, Act::Write, &target);
        assert!(!target.exists());
    }
}

#[test]
fn writes_to_a_sibling_workspace_and_the_main_repo_are_denied() {
    let e = env();
    let sibling = e.other.root().join("from-w.txt");
    let readme = e.f.repo.join("README");
    for &kind in kinds() {
        assert_denied(&e.ws, kind, Act::Write, &sibling);
        assert!(!sibling.exists());
        assert_denied(&e.ws, kind, Act::Write, &readme);
        assert_eq!(std::fs::read_to_string(&readme).unwrap(), "hello\n");
    }
}

#[test]
fn an_existing_outside_file_cannot_be_modified_or_deleted() {
    let e = env();
    let target = e.outside.path().join("keep.txt");
    std::fs::write(&target, "original").unwrap();
    for &kind in kinds() {
        assert_denied(&e.ws, kind, Act::Write, &target);
        assert_denied(&e.ws, kind, Act::Delete, &target);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }
}

#[test]
fn the_owner_cannot_rewrite_the_dacl_of_an_outside_file() {
    let e = env();
    let target = e.outside.path().join("owned.txt");
    std::fs::write(&target, "original").unwrap();
    for &kind in kinds() {
        assert_denied(&e.ws, kind, Act::OpenWriteDac, &target);
        assert_denied(&e.ws, kind, Act::OpenWriteOwner, &target);
        assert_denied(&e.ws, kind, Act::OpenWriteDac, e.outside.path());
    }
    // The premise of this test is the owner's implicit WRITE_DAC: a file the
    // command itself owns. In the profile a sandbox user owns nothing, so the
    // denial above is the profile's access list. This phase gives it a file
    // of its own, outside the profile, and records what WRITE_DAC does there.
    let Some(shared) = writable_outside("write-dac") else {
        return;
    };
    for &kind in kinds() {
        let own = shared.path().join(format!("own-{}.txt", kinds_tag(kind)));
        let (code, line) = probe(&e.ws, kind, Act::Write, &own);
        assert_eq!(
            code,
            0,
            "a sandbox user could not make a file in an Everyone-writable folder below \
             ProgramData {}, which the gaps text of the backend says it can. If this \
             machine closes that, narrow the claim in sandbox/windows.rs; WRITE_DAC on a \
             file the command owns then needs another place to be measured: {line}",
            kind.word()
        );
        let (code, line) = probe(&e.ws, kind, Act::OpenWriteDac, &own);
        println!(
            "measurement: WRITE_DAC on a file the command owns {}: exit {code}, {line}",
            kind.word()
        );
        let _ = std::fs::remove_file(&own);
    }
}

/// An Everyone-full-control directory, against what the backend claims.
///
/// Under the token sandbox such a directory is DACL-writable, and the Low
/// integrity level blocks the write up while the directory stays at Medium:
/// a write that succeeds means the Low label did not take and the strength
/// text must change.
///
/// Under the sandbox user the directory's own access list decides, and the
/// gaps text claims the gap: a folder below the folders setup closes that
/// grants every user write stays writable. So the write must succeed, in a
/// folder under ProgramData (setup's deny is on ProgramData alone). A denial
/// there means the claim is wider than the machine and must be narrowed.
#[test]
fn a_write_into_an_everyone_writable_directory_matches_what_the_backend_claims() {
    let e = env();
    if !sandbox_user_mode() {
        let dir = e.outside.path().join("world");
        std::fs::create_dir_all(&dir).unwrap();
        grant_everyone(&dir);
        let target = dir.join("planted.txt");
        assert_denied(&e.ws, Kind::Online, Act::Write, &target);
        assert!(!target.exists());
        return;
    }
    let gaps = platform_isolation().expect("a backend").gaps;
    assert!(
        gaps.contains("below the folders setup closes"),
        "the sandbox user's gaps text no longer claims this gap, and this test measures it: \
         {gaps}"
    );
    let shared = shared_folder("everyone-write");
    grant_everyone(shared.path());
    for &kind in kinds() {
        let target = shared
            .path()
            .join(format!("planted-{}.txt", kinds_tag(kind)));
        let (code, line) = probe(&e.ws, kind, Act::Write, &target);
        assert_eq!(
            code,
            0,
            "a sandbox user could not write an Everyone-writable folder below ProgramData \
             {}, which the gaps text says it can. Measure it by hand, then narrow the claim \
             in sandbox/windows.rs: {line}",
            kind.word()
        );
        assert!(target.exists());
        std::fs::remove_file(&target).expect("remove the planted file");
    }
}

#[test]
fn a_hard_link_to_an_outside_file_cannot_escape() {
    let e = env();
    let outside = e.outside.path().join("secret.txt");
    std::fs::write(&outside, "original").unwrap();
    for &kind in kinds() {
        let link = e.ws.root().join(format!("link-{}.txt", kinds_tag(kind)));
        let (code, line) = probe2(&e.ws, kind, Act::HardlinkWrite, &link, &outside);
        assert_ne!(code, 0, "hard-link write succeeded {}: {line}", kind.word());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
    }
    // The same attempt where the target's own access list, and not the
    // owner's profile, decides whether the write lands. The gaps text
    // concedes a write in such a folder, so this phase records what the hard
    // link does there; the phase above is what pins a file outside the
    // sandbox user's reach.
    let Some(shared) = writable_outside("hardlink") else {
        return;
    };
    let target = shared.path().join("shared.txt");
    std::fs::write(&target, "original").expect("write the shared file");
    for &kind in kinds() {
        let link =
            e.ws.root()
                .join(format!("shared-link-{}.txt", kinds_tag(kind)));
        let (code, line) = probe2(&e.ws, kind, Act::HardlinkWrite, &link, &target);
        println!(
            "measurement: a hard link to {} {}: exit {code}, {line}",
            target.display(),
            kind.word()
        );
        let _ = std::fs::remove_file(&link);
    }
}

#[test]
fn a_junction_inside_the_root_cannot_escape() {
    let e = env();
    let outside = e.outside.path().join("target-dir");
    std::fs::create_dir_all(&outside).unwrap();
    for &kind in kinds() {
        let link = e.ws.root().join(format!("jn-{}", kinds_tag(kind)));
        let acl_before = icacls_listing(&outside);
        let (code, line) = probe2(&e.ws, kind, Act::JunctionWrite, &link, &outside);
        assert_ne!(code, 0, "junction write succeeded {}: {line}", kind.word());
        assert!(!outside.join("planted.txt").exists(), "{line}");
        // The junction the probe planted stays in the tree. The probe's own
        // user already has its entry on the tree, so the next command of the
        // same user writes no entry and walks no link: the grant returns
        // before any write when the entry is present. With nothing written,
        // nothing can be carried through the link, and that command starts.
        // The junction's target must keep its access list. The refusal of a
        // grant that would write is proved below, on a tree no user has an
        // entry on. The link is then taken out, so the next kind measures
        // its own write.
        if std::fs::symlink_metadata(&link).is_ok() {
            let next = sandboxed_command(&policy_as(&e.ws, kind), self_exe(), e.ws.root())
                .map(|_| ())
                .map_err(|e| e.to_string());
            if sandbox_user_mode() {
                assert!(
                    next.is_ok(),
                    "the next command of the user whose entry is already on the tree did not \
                     start beside the junction {}: {next:?}",
                    kind.word()
                );
            } else {
                println!("measurement: the next command beside the planted junction: {next:?}");
            }
            assert_eq!(
                icacls_listing(&outside),
                acl_before,
                "the access list of the junction's target changed {}",
                kind.word()
            );
            std::fs::remove_dir(&link).expect("remove the planted junction");
        }
    }
    // A tree that holds a junction out of it, where no sandbox user has an
    // entry yet: the first grant for each user must be refused before any
    // entry is written, and the target's access list must not change.
    if sandbox_user_mode() {
        let planted = e.other.root().join("jn-planted");
        let status = std::process::Command::new("cmd.exe")
            .args([
                "/d",
                "/c",
                "mklink",
                "/J",
                planted.to_string_lossy().as_ref(),
                outside.to_string_lossy().as_ref(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "could not plant the junction");
        let acl_before = icacls_listing(&outside);
        for &kind in kinds() {
            let refused = sandboxed_command(&policy_as(&e.other, kind), self_exe(), e.other.root())
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                refused.contains("holds the link") && refused.contains("nothing was written"),
                "a tree holding a junction out of it was granted on {}: {refused:?}",
                kind.word()
            );
        }
        assert_eq!(
            icacls_listing(&outside),
            acl_before,
            "the access list of the junction's target changed"
        );
        std::fs::remove_dir(&planted).expect("remove the planted junction");
    }
    // As in the hard-link test: a target whose own access list decides, so
    // the junction itself is what the result measures.
    let Some(shared) = writable_outside("junction") else {
        return;
    };
    let target = shared.path().join("target-dir");
    std::fs::create_dir_all(&target).expect("the shared target folder");
    for &kind in kinds() {
        let link = e.ws.root().join(format!("shared-jn-{}", kinds_tag(kind)));
        let (code, line) = probe2(&e.ws, kind, Act::JunctionWrite, &link, &target);
        println!(
            "measurement: a junction to {} {}: exit {code}, {line}",
            target.display(),
            kind.word()
        );
        // The link alone, never what it points at.
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_dir(&link).expect("remove the planted junction");
        }
    }
}

/// A planted `scratch\.git` junction must be refused at prepare, so the
/// host never rewrites DACLs across the junction target (the user profile).
///
/// Two guards can refuse it, and either holds the claim: the walk of a tree
/// for links before an inheritable entry is written on it (it meets the
/// junction inside the scratch folder first), and the reparse-point check on
/// the path itself. The refusal must name the planted junction, and the
/// target's access list must be unchanged afterwards.
#[test]
fn a_planted_git_junction_is_refused_at_prepare() {
    let e = env();
    let victim = e.outside.path().join("victim-profile");
    std::fs::create_dir_all(&victim).unwrap();
    let acl_before = icacls_listing(&victim);
    let gitlink = e.ws.scratch().join(".git");
    let status = std::process::Command::new("cmd.exe")
        .args([
            "/d",
            "/c",
            "mklink",
            "/J",
            gitlink.to_string_lossy().as_ref(),
            victim.to_string_lossy().as_ref(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "could not plant the junction");
    let err = sandboxed_command(&policy(&e.ws), "cmd.exe", e.ws.root()).unwrap_err();
    // The step, not only its kind: a refused slot user, a runner that cannot
    // start and every other platform failure is also `PlatformSetup`, and
    // none of those refused the junction.
    let SandboxError::PlatformSetup { step, source } = &err else {
        panic!("not a setup refusal: {err}");
    };
    assert!(
        PRE_WRITE_LINK_REFUSALS.contains(step),
        "refused at {step:?}, not by a guard that runs before the write: {err}"
    );
    // Paths compare without a verbatim prefix and without case, as Windows
    // compares them.
    let plain = |s: String| s.replace(r"\\?\", "").to_lowercase();
    assert!(
        plain(source.to_string()).contains(&plain(gitlink.display().to_string())),
        "the refusal does not name the planted junction {}: {err}",
        gitlink.display()
    );
    // The victim's access list is exactly what it was: nothing was written
    // through the junction.
    assert!(victim.is_dir());
    assert_eq!(
        icacls_listing(&victim),
        acl_before,
        "the access list of the junction's target changed"
    );
}

/// The steps that refuse a planted link before any entry is written. The
/// step that finds a link only after granting is not one of them.
const PRE_WRITE_LINK_REFUSALS: [&str; 2] = [
    "check a tree for links before granting on it",
    "reject a reparse point before changing its security",
];

/// The access list of `path` as `icacls` prints it.
fn icacls_listing(path: &Path) -> String {
    let out = std::process::Command::new("icacls.exe")
        .arg(path)
        .output()
        .expect("icacls runs");
    assert!(out.status.success(), "icacls {}: {out:?}", path.display());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Opening the OpenSSH agent pipe for read and write, against what the
/// backend in use does.
///
/// Under the token sandbox the open must fail: the agent's pipe is Medium and
/// the command is Low. Under the sandbox user there is no integrity label and
/// no restricting SID, and the agent's pipe admits authenticated users, which
/// a sandbox user is; the gaps text of that backend concedes exactly this
/// ("named objects every user may write are not closed"). So the result there
/// is a measurement, and a successful open is the documented gap, not a
/// surprise. Either way the run records which it saw.
///
/// With no agent running there is no pipe at all, and a not-found says
/// nothing about either claim, so a run that must measure this fails instead.
#[test]
fn the_ssh_agent_pipe_cannot_be_opened() {
    let e = env();
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    const PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    // Whether the pipe is there at all, from the host. Only a not-found says
    // it is absent: a busy pipe (every instance taken for a moment) or a
    // refused open means the agent runs, and reading either as absence would
    // name the wrong cause.
    let absent = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE)
    {
        Ok(_) => false,
        Err(e) => e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32),
    };
    assert!(
        !absent || !common::require_sandbox_user(),
        "the OpenSSH agent is not running, so nothing here measures whether a command can \
         reach its pipe. Start it (`Start-Service ssh-agent`) and run again"
    );
    for &kind in kinds() {
        let (code, line) = probe(&e.ws, kind, Act::OpenPipe, Path::new(PIPE));
        if absent {
            println!("measurement: the ssh-agent pipe is absent; exit {code}, {line}");
            continue;
        }
        if sandbox_user_mode() {
            println!(
                "measurement: the ssh-agent pipe {}: exit {code}, {line}",
                kind.word()
            );
        } else {
            // A busy pipe (every instance taken for a moment, which the
            // host's own open above can cause) is not a result either way.
            let busy = line.contains("os error 231");
            assert!(
                code == DENIED || busy,
                "the ssh-agent pipe was neither opened nor denied to a Low command {}, so \
                 this run proves nothing about it: {line}",
                kind.word()
            );
            if busy {
                println!(
                    "measurement: the ssh-agent pipe was busy {}: {line}",
                    kind.word()
                );
            }
        }
    }
}

/// Whether a Run value is one this test left behind in an earlier run:
/// `kgraf-escape-<pid>-<kind>`, where the process of that id is gone. A value
/// of a run that is still going is left alone, and so is every value of the
/// owner's own.
fn stale_probe_value(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("kgraf-escape-") else {
        return false;
    };
    let mut parts = rest.split('-');
    let Some(Ok(pid)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    if !parts.next().is_some_and(|tag| {
        [Kind::Offline, Kind::Online]
            .iter()
            .any(|k| kinds_tag(*k) == tag)
    }) || parts.next().is_some()
    {
        return false;
    }
    pid != std::process::id() && !process_lives(pid)
}

#[test]
fn the_registry_run_key_cannot_be_written() {
    let e = env();
    // The owner's own key, named by SID, not the command's
    // HKEY_CURRENT_USER: a sandbox user runs with no profile loaded, so its
    // own HKEY_CURRENT_USER is the default user's and neither the write nor
    // a read of it says anything about the owner.
    let owner = own_sid_of_this_process();
    let key = format!(r"HKEY_USERS\{owner}\Software\Microsoft\Windows\CurrentVersion\Run");
    // reg.exe takes the key and switches only, so each call passes exactly
    // the arguments it needs: an empty padding argument is a second
    // positional, which reg.exe refuses as a syntax error.
    let query = |args: &[&str]| {
        std::process::Command::new("reg.exe")
            .args(args)
            .output()
            .expect("run reg.exe")
    };
    // The key itself must be there, or a refused write and an absent key are
    // the same result and this test measures nothing. A plain query of an
    // existing key exits 0 even with no values in it.
    assert!(
        query(&["query", &key]).status.success(),
        "{key} is not there, so a refused write and a missing key cannot be told apart. \
         Log on once as this user, or point the test at a key that exists"
    );
    // Values an earlier run left behind, where a write landed and the run was
    // killed before it could clean up. Named after this test, so no value of
    // the owner's own is touched.
    let listing = query(&["query", &key]);
    for stale in String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| stale_probe_value(name))
        .map(str::to_owned)
        .collect::<Vec<_>>()
    {
        let out = query(&["delete", &key, "/v", &stale, "/f"]);
        assert!(
            out.status.success(),
            "{stale} is in {key} from an earlier run and could not be removed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for &kind in kinds() {
        let value = format!("kgraf-escape-{}-{}", std::process::id(), kinds_tag(kind));
        let (code, line) = probe2(
            &e.ws,
            kind,
            Act::RegwriteRunKey,
            Path::new(&value),
            Path::new(&owner),
        );
        // The value is removed before anything is asserted: where the write
        // did land, a panic here would leave a program to start at the next
        // logon. `/f` because reg.exe otherwise asks, and a command whose
        // input is closed answers nothing and deletes nothing.
        let found = query(&["query", &key, "/v", &value]).status.success();
        if found {
            let out = query(&["delete", &key, "/v", &value, "/f"]);
            assert!(
                out.status.success(),
                "the value {value} was written in {key} and could not be removed, so it \
                 stays and would start a program at the owner's next logon: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert!(
            !found,
            "the value {value} was written in {key}, so a command can start a program at \
             the owner's next logon {}: {line}",
            kind.word()
        );
        // A denial, not any failure: with the key present, an exit that is
        // neither a denial nor a success says the probe never reached it.
        assert_eq!(
            code,
            DENIED,
            "the write to {key} neither landed nor was denied {}, so this run proves \
             nothing about it: {line}",
            kind.word()
        );
    }
}

// ---- reads (documented gaps) --------------------------------------------------

/// Reads outside the workspace are not confined, against what the backend
/// claims. The token sandbox reads every file the owner can read, including
/// one in the owner's own profile. A sandbox user cannot read the owner's
/// profile (the test further down pins that), so its measurement is a file
/// outside the profile, under ProgramData, which every local user may read:
/// the gaps text says such a read stays open.
#[test]
fn reads_outside_the_workspace_are_not_confined() {
    let e = env();
    if !sandbox_user_mode() {
        let target = e.outside.path().join("readable.txt");
        std::fs::write(&target, "visible").unwrap();
        let (code, line) = probe(&e.ws, Kind::Online, Act::Read, &target);
        assert_eq!((code, line.as_str()), (0, "visible"));
        return;
    }
    let shared = shared_folder("read");
    let target = shared.path().join("readable.txt");
    std::fs::write(&target, "visible").expect("write the file the command reads");
    for &kind in kinds() {
        let (code, line) = probe(&e.ws, kind, Act::Read, &target);
        assert_eq!(
            (code, line.as_str()),
            (0, "visible"),
            "a sandbox user could not read {} {}, which every local user may read and the \
             gaps text says it can. Measure it by hand, then narrow the claim in \
             sandbox/windows.rs",
            target.display(),
            kind.word()
        );
    }
}

/// Whether a command can open the harness process for `PROCESS_VM_READ`,
/// against what the backend in use claims.
///
/// Under the token sandbox the command runs as the same Windows user, so the
/// harness process's own access list admits it, but a process object carries
/// a mandatory label with the no-read-up policy as well as no-write-up, so a
/// Low command is refused `PROCESS_VM_READ` on the Medium harness. Measured
/// on Windows Server 2025 24H2: access denied (os error 5). The strength text
/// claims no such gap. The test holds the text and the
/// measurement together in both directions: an open that works while the
/// text is silent fails, and so does a refusal while the text claims the gap.
/// Under the sandbox user the command runs as another user, which that list
/// does not admit, so the open must fail; that is one of the reasons for
/// decision 0097.
///
/// A refusal and a probe that failed for some other reason are different
/// findings, and this says which it was.
///
/// This is the harness process. The runner, which starts the command and
/// holds the owner's logon SID, is covered by
/// `a_command_cannot_open_the_runner_process_or_token_beyond_reading` in
/// `tests/runner_windows.rs`: it tries PROCESS_VM_READ, PROCESS_VM_WRITE and
/// eight more rights against the runner's process, every one of its threads
/// and its token, and expects access denied for each.
#[test]
fn the_harness_process_vm_read_matches_what_the_backend_claims() {
    let e = env();
    let harness = std::process::id().to_string();
    let (code, line) = probe(
        &e.ws,
        Kind::Online,
        Act::OpenProcessVmRead,
        Path::new(&harness),
    );
    let strength = platform_isolation().expect("a backend");
    let claims_gap = strength.gaps.contains("PROCESS_VM_READ");
    match (strength.backend, code) {
        (Backend::WindowsSandboxUser, DENIED) => {}
        (Backend::WindowsSandboxUser, 0) => panic!(
            "the sandbox user opened the harness for VM_READ, which its own access list \
             should refuse to another user: {line}"
        ),
        (Backend::WindowsSandboxUser, other) => panic!(
            "the VM_READ probe exited {other}, neither opened nor refused, so this run \
             proves nothing about reading the harness: {line}"
        ),
        (_, 0) => assert!(
            claims_gap,
            "VM_READ of the harness works, and the strength text does not claim that \
             gap: the no-read-up label on the harness process did not hold. Claim the gap \
             in STRENGTH.gaps in sandbox/windows.rs, or fix the backend: {line}"
        ),
        (_, DENIED) => assert!(
            !claims_gap,
            "VM_READ of the harness is refused on this machine, and the strength text \
             still claims it as a gap. Measure it by hand, then drop the claim from \
             STRENGTH.gaps in sandbox/windows.rs: {line}"
        ),
        (_, other) => panic!(
            "the VM_READ probe exited {other}, neither opened nor refused, so this run \
             proves nothing about the gap: {line}"
        ),
    }
}

// ---- the sandbox user (decision 0097) ----------------------------------------

/// The sandbox users this machine has, read from the owner's own store, or
/// `None` where `kgraf sandbox setup` has not run. The SIDs here are what a
/// command's own SID is compared against.
fn stored_sandbox_users() -> Option<Vec<(kgraf_sandbox::sandbox_user::StoredUser, Vec<u8>)>> {
    use kgraf_sandbox::sandbox_user::{blob, credentials_path};
    let tag = std::env::var("KGRAF_SANDBOX_TAG")
        .ok()
        .filter(|t| !t.is_empty());
    let store = credentials_path(tag.as_deref())?;
    let bytes = std::fs::read(store).ok()?;
    blob::decode_users(&bytes).ok()
}

/// This process's own user SID, the owner's. The probe reports its own the
/// same way ([`own_user_sid`]).
fn own_sid_of_this_process() -> String {
    own_user_sid().expect("this process's own user SID")
}

/// A command runs as a sandbox user, never as the owner. That is what keeps
/// it out of the owner's own files: the access lists of the owner's profile
/// admit the owner, SYSTEM and administrators, and none of those is the
/// command.
#[test]
fn a_command_runs_as_a_sandbox_user_and_not_as_the_owner() {
    let e = env();
    let (code, line) = probe(&e.ws, Kind::Online, Act::UserSid, Path::new(""));
    assert_eq!(code, 0, "the probe did not report its user: {line}");
    let own = own_sid_of_this_process();
    if !sandbox_user_mode() {
        // The token sandbox runs the command as the owner, with a cut-down
        // copy of the owner's token. That is the design of decision 0011.
        assert_eq!(line, own, "the token sandbox runs as the owner: {line}");
        return;
    }
    assert_ne!(line, own, "the command runs as the owner");
    let users = stored_sandbox_users().expect("the stored sandbox users");
    assert!(
        users.iter().any(|(u, _)| u.sid == line),
        "the command runs as {line}, which is no sandbox user this setup stored"
    );
}

/// Two workers in different worktrees that run at the same time run as
/// different sandbox users, so neither can write the other's files. Where the
/// setup made one user per pool (`--slots 0`), the second worker is refused,
/// never run as the first one's user.
#[test]
fn two_workers_in_different_worktrees_run_as_different_users() {
    if !sandbox_user_mode() {
        return;
    }
    use kgraf_sandbox::sandbox_user::Role;
    let e = env();
    let users = stored_sandbox_users().expect("the stored sandbox users");
    // One user per pool where the setup made no slots, else one per slot.
    let size = users
        .iter()
        .filter(|(u, _)| matches!(u.role, Role::Slot(_)))
        .count()
        .max(1);
    let sid_of = |role: Role| {
        users
            .iter()
            .find(|(u, _)| u.role == role)
            .map(|(u, _)| u.sid.clone())
            .unwrap_or_else(|| panic!("no stored user for {role}"))
    };
    let a = worker_slot(e.ws.root())
        .expect("a worker slot")
        .expect("the sandbox user backend gives a slot");
    let second = worker_slot(e.other.root());
    if size < 2 {
        let err = second.expect_err("a second worktree took the only offline slot user");
        assert!(
            err.to_string().contains("never run as one sandbox user"),
            "{err}"
        );
        return;
    }
    let b = second
        .expect("a second worker slot")
        .expect("the sandbox user backend gives a slot");
    assert_ne!(a.role(), b.role(), "two worktrees hold one slot");
    let run = |ws: &Workspace, slot: &WorkerSlot, action: Act, arg: &str| {
        let policy = SandboxPolicy::for_workspace(ws, Network::Deny)
            .accept_partial_isolation(true)
            .with_worker_slot(slot.clone());
        parse(&probe_output(&policy, ws.root(), action, arg, ""))
    };
    for (ws, slot) in [(&e.ws, &a), (&e.other, &b)] {
        let (code, line) = run(ws, slot, Act::UserSid, "");
        assert_eq!(code, 0, "the probe did not report its user: {line}");
        assert_eq!(
            line,
            sid_of(slot.role()),
            "{} runs as the wrong sandbox user",
            ws.root().display()
        );
    }
    // While both hold their slots, neither writes the other's worktree.
    let target = e.other.root().join("from-w.txt");
    let (code, line) = run(&e.ws, &a, Act::Write, &target.to_string_lossy());
    assert_eq!(code, DENIED, "a worker wrote another worktree: {line}");
    assert!(!target.exists());
}

/// Linking the checkout's installed dependencies into a worktree grants
/// write on that worktree to its own slot's user only. It once granted every
/// sandbox user, so a command in one worktree could write another worktree
/// that had dependencies linked.
#[test]
fn linking_dependencies_grants_only_the_worktrees_own_user() {
    if !sandbox_user_mode() {
        return;
    }
    let canonical =
        |d: &tempfile::TempDir| kgraf_core::plain_path::canonicalize(d.path()).expect("canonical");
    let write = |p: &Path, text: &str| {
        std::fs::create_dir_all(p.parent().expect("parent")).expect("folder");
        std::fs::write(p, text).expect("write");
    };
    let checkout = tempfile::tempdir().expect("checkout");
    let c = canonical(&checkout);
    write(&c.join("package.json"), "{}");
    write(&c.join("node_modules/dep/index.js"), "dep");
    let (one, two) = (
        tempfile::tempdir().expect("tree"),
        tempfile::tempdir().expect("tree"),
    );
    let (t1, t2) = (canonical(&one), canonical(&two));
    for t in [&t1, &t2] {
        write(&t.join("package.json"), "{}");
    }
    let slot1 = worker_slot(&t1)
        .expect("a worker slot")
        .expect("the sandbox user backend gives a slot");
    // This is the only test of the regression where linking granted write to
    // every sandbox user, so it must not skip: without two slots it cannot
    // run, and that is a setup to fix, not a pass.
    let slot2 = worker_slot(&t2)
        .expect(
            "a slot for the second folder. This test needs two offline slot users: run \
             `kgraf sandbox setup --slots 2` or more",
        )
        .expect("the sandbox user backend gives a slot");
    let links1 = link_dependency_trees(&t1, &c);
    assert!(!links1.reads.is_empty(), "nothing was linked: {links1:?}");
    let links2 = link_dependency_trees(&t2, &c);
    assert!(!links2.reads.is_empty(), "nothing was linked: {links2:?}");
    let policy = SandboxPolicy::new(vec![t1.clone()], Network::Deny)
        .with_dependency_links(&links1)
        .accept_partial_isolation(true)
        .with_worker_slot(slot1.clone());
    let target = t2.join("from-one.txt");
    let (code, line) = parse(&probe_output(
        &policy,
        &t1,
        Act::Write,
        &target.to_string_lossy(),
        "",
    ));
    assert_eq!(
        code, DENIED,
        "a worktree's user wrote another worktree with linked dependencies: {line}"
    );
    assert!(!target.exists());
    drop(slot2);
}

/// A file in the owner's profile is not readable by a command that runs as a
/// sandbox user. Under the token sandbox it is: reads are not confined there
/// (decision 0011), which is one reason for decision 0097.
#[test]
fn the_owners_profile_is_not_readable_as_a_sandbox_user() {
    let e = env();
    let profile = PathBuf::from(std::env::var("USERPROFILE").expect("USERPROFILE"));
    let target = profile.join(format!("kgraf-escape-probe-{}.txt", std::process::id()));
    std::fs::write(&target, "owner's own file").expect("write in the owner's profile");
    let as_user = sandbox_user_mode();
    let mut results = Vec::new();
    for &kind in kinds() {
        results.push((kind, probe(&e.ws, kind, Act::Read, &target)));
    }
    let _ = std::fs::remove_file(&target);
    for (kind, (code, line)) in results {
        if as_user {
            assert_eq!(
                code,
                DENIED,
                "a sandbox user read a file in the owner's profile {}: {line}",
                kind.word()
            );
        } else {
            assert_eq!(
                (code, line.as_str()),
                (0, "owner's own file"),
                "the token sandbox does not confine reads; the strength text says so"
            );
        }
    }
}

/// A POSIX shell runs. Git Bash cannot start under the token sandbox, because
/// its runtime opens its own signal pipe for writing and a write-restricted
/// token refuses that; under the sandbox user there are no restricting SIDs,
/// so it starts. This is one of the failures decision 0097 exists to fix.
#[test]
fn a_posix_shell_runs_as_a_sandbox_user() {
    if !sandbox_user_mode() {
        return;
    }
    let e = env();
    let out = sandboxed_command(&policy(&e.ws), "sh", e.ws.root())
        .expect("a shell command")
        .args(["-c", "echo shell-ok"])
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped)
        .output()
        .expect("run the shell");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && text.contains("shell-ok"),
        "Git Bash did not run: status {:?}, stdout {text}, stderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A listener on loopback that accepts and drops every client. `port` 0 takes
/// any free port. The listener lives as long as the process, which is one
/// test binary run.
fn serve(port: u16) -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            drop(client);
        }
    });
    Ok(port)
}

/// An address outside loopback that no network carries: TEST-NET-1 of RFC
/// 5737, reserved for documentation and tests.
const NON_LOOPBACK: &str = "192.0.2.1:80";

/// A command whose policy denies the network reaches kGraf's egress proxy
/// ports and nothing else. The firewall rules of the offline and slot users
/// block loopback outside those ports.
///
/// The positive control is the point of the second half: without it, a probe
/// that cannot connect to anything at all, or one that never ran, passes the
/// first check. So the same command must reach a listener on a proxy port,
/// which the rules leave open.
#[test]
fn a_command_without_the_network_reaches_the_proxy_ports_and_nothing_else() {
    if !sandbox_user_mode() {
        // The token sandbox cannot deny the network at all, and refuses the
        // policy instead; `STRENGTH.network_deniable` says so.
        return;
    }
    use kgraf_sandbox::sandbox_user::PROXY_PORTS;
    let e = env();
    let blocked = std::iter::repeat_with(|| serve(0).expect("bind a local listener"))
        .take(8)
        .find(|p| !PROXY_PORTS.contains(p))
        .expect("a free port outside kGraf's proxy ports");
    let allowed = PROXY_PORTS
        .clone()
        .find_map(|p| serve(p).ok())
        .expect("a free port in kGraf's proxy port range");
    let connect = |addr: &str| -> (i32, String) {
        parse(&probe_output(
            &policy_as(&e.ws, Kind::Offline),
            e.ws.root(),
            Act::Connect,
            addr,
            "",
        ))
    };
    let (code, line) = connect(&format!("127.0.0.1:{blocked}"));
    assert_ne!(
        code, 0,
        "a command with the network denied reached a local listener on port {blocked}: {line}"
    );
    let (code, line) = connect(&format!("127.0.0.1:{allowed}"));
    assert_eq!(
        code, 0,
        "a command with the network denied could not reach port {allowed}, where kGraf's \
         egress proxy listens and the offline user's firewall rules leave the way open. \
         Until that works, the check above proves nothing: a probe that connects to nothing \
         would pass it: {line}"
    );
    let (code, line) = connect(NON_LOOPBACK);
    assert_ne!(
        code, 0,
        "a command with the network denied reached {NON_LOOPBACK}, outside loopback: {line}"
    );
}

/// M1 of decision 0097: traffic Windows sends for a command. An offline-slot
/// command opens a UNC path to an address outside loopback, as a plain SMB
/// share and in the WebDAV form (`\\host@port\share`). The kernel's SMB
/// client carries the first and the WebClient service the second, and neither
/// runs as the sandbox user, so the firewall rules scoped to that user do not
/// cover what they send.
///
/// This test is a measurement, not a block: a pass says the attempt gave the
/// command no usable path, because it was refused or because nothing
/// answered. Nothing here watches the wire, so it cannot say whether a packet
/// left the machine. The address is TEST-NET-1 (RFC 5737), routed nowhere.
#[test]
fn a_unc_path_outside_loopback_is_measured_not_blocked() {
    if !sandbox_user_mode() {
        return;
    }
    let e = env();
    for path in [r"\\192.0.2.1\x", r"\\192.0.2.1@80\x"] {
        let out = probe_within(
            &policy_as(&e.ws, Kind::Offline),
            e.ws.root(),
            Act::UncList,
            path,
            Duration::from_secs(180),
            // A thread waiting in the SMB redirector or the WebClient
            // service can take longer than the usual bound to end.
            Duration::from_secs(60),
        );
        let (code, line) = parse(&out);
        assert_ne!(
            code, 0,
            "an offline-slot command listed {path}, so Windows carried its traffic to an \
             address outside loopback: {line}"
        );
        // Recorded for the run's report: which refusal, and how long it took,
        // is what says whether a packet could have left.
        println!("measurement: {path} as the offline slot user: exit {code}, {line}");
    }
}

// ---- process, job, console, desktop ------------------------------------------

/// A sandbox must not open another sandbox's process for write.
///
/// Both commands run with the network open, so under the sandbox user they
/// take the online slot of their own folder and run as different users, and
/// the open is refused. Where the setup made no slot users there is one
/// online user: the second folder is then refused a user of its own while the
/// first holds it, which is the same confinement reached another way
/// (decision 0097), and never a run as the other folder's user.
#[test]
fn one_sandbox_cannot_open_another_sandboxs_process_for_write() {
    let e = env();
    let (victim_pid, victim) = spawn_sleeper(&e.other);
    let result = try_probe(
        &e.ws,
        Kind::Online,
        Act::OpenProcessForWrite,
        Path::new(&victim_pid.to_string()),
    );
    // Kill the victim before asserting so a failure does not leak it.
    drop(victim);
    let backend = platform_isolation().expect("a backend").backend;
    match result {
        Ok((DENIED, _)) => {}
        Ok((0, line)) => panic!(
            "a command in one worktree opened the process of a command in another for write, \
             on {backend:?}. As a sandbox user that means two folders ran as one user; under \
             the token sandbox it means cross-sandbox write denial does not hold and the \
             claim must be dropped from the backend's gaps text: {line}"
        ),
        Ok((other, line)) => panic!(
            "the probe exited {other}, neither opened nor refused, so this run proves \
             nothing about cross-sandbox writes: {line}"
        ),
        Err(err) => {
            let refused_a_user = err.to_string().contains("never run as one sandbox user");
            assert!(
                refused_a_user,
                "the attacking command did not run, and not because it was refused a sandbox \
                 user of its own: {err}"
            );
        }
    }
}

#[test]
fn the_sandbox_cannot_open_the_host_process_for_writing() {
    let e = env();
    for &kind in kinds() {
        let (code, line) = probe(
            &e.ws,
            kind,
            Act::OpenProcessForWrite,
            Path::new(&std::process::id().to_string()),
        );
        assert_eq!(code, DENIED, "{} {line}", kind.word());
    }
}

#[test]
fn a_child_cannot_break_away_from_the_job() {
    let e = env();
    for &kind in kinds() {
        let (code, line) = probe(&e.ws, kind, Act::Breakaway, Path::new(""));
        assert_eq!(code, DENIED, "{} {line}", kind.word());
    }
}

#[test]
fn descendants_do_not_outlive_the_command() {
    let e = env();
    let (code, line) = probe(&e.ws, Kind::Online, Act::SpawnSleeper, Path::new(""));
    assert_eq!(code, 0, "{line}");
    let pid: u32 = line.parse().unwrap();
    require_process_gone(pid, "the grandchild of the command");
}

/// Fail unless process `pid` has ended, waiting up to ten seconds for it.
///
/// A null handle is not proof on its own: this process cannot open a sandbox
/// user's process at all, so an access denied says the process is still
/// there. Only `ERROR_INVALID_PARAMETER`, the error for a process id that
/// names nothing, says it is gone, so an access denied is tried again until
/// the deadline: closing a job kills its processes in no fixed order, and the
/// one asked about here may still be on its way out.
///
/// A process id is reused once its process is gone, so a denied open at the
/// deadline can also be another user's process that took the number. That is
/// inherent to a check by process id, and it fails this test rather than
/// pass it.
fn require_process_gone(pid: u32, what: &str) {
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: plain call; the handle, if any, is owned below.
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if h.is_null() {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what} ({pid}) could not be opened to wait for it, and not because it is \
                 gone, so it is still there: {e}"
            );
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        // SAFETY: a handle just returned to us.
        let owned = unsafe {
            <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(h)
        };
        let left = deadline.saturating_duration_since(Instant::now());
        // SAFETY: a process handle we own, with synchronize access.
        let r = unsafe { WaitForSingleObject(owned.as_raw_handle(), left.as_millis() as u32) };
        assert_eq!(r, WAIT_OBJECT_0, "{what} ({pid}) is still running");
        return;
    }
}

#[test]
fn dropping_a_spawned_child_kills_it() {
    let e = env();
    let child = sandboxed_command(&policy(&e.ws), self_exe(), e.ws.root())
        .unwrap()
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_PROBE", Act::Sleep.name())
        .spawn()
        .unwrap();
    // `process_handle` is the runner on the sandbox user path, so the command
    // itself is checked by its process id as well.
    let process = child.process_handle().try_clone_to_owned().unwrap();
    let command_pid = child.id();
    drop(child);
    // SAFETY: a process handle we own.
    let r = unsafe {
        windows_sys::Win32::System::Threading::WaitForSingleObject(process.as_raw_handle(), 10_000)
    };
    assert_eq!(r, windows_sys::Win32::Foundation::WAIT_OBJECT_0);
    require_process_gone(command_pid, "the command");
}

#[test]
fn the_sandbox_cannot_attach_to_the_host_console() {
    let e = env();
    // With no console on the host, `AttachConsole` fails with an invalid
    // handle whatever the sandbox does, and this test would pass without
    // measuring the attachment it names.
    assert!(
        console_window() || !common::require_sandbox_user(),
        "this test binary has no console, so nothing here measures an attach to one. Run \
         the tests from a console window"
    );
    for &kind in kinds() {
        let (code, line) = probe(
            &e.ws,
            kind,
            Act::AttachConsole,
            Path::new(&std::process::id().to_string()),
        );
        assert_ne!(
            code,
            0,
            "AttachConsole to the host succeeded {}: {line}",
            kind.word()
        );
    }
}

/// A synthetic keystroke from the command reaches nothing of the owner's,
/// against what its backend does.
///
/// Under the token sandbox the command keeps the owner's window station, and
/// UIPI with the Low label on its desktop is what stops the injection: the
/// observable is that `SendInput` injects zero events.
///
/// Under the sandbox user the command is on a window station of its own, so
/// an injection into its own queue may well succeed and still reach nothing.
/// The station name is the measurement there, and the count is recorded.
#[test]
fn send_input_reaches_nothing_of_the_owners() {
    let e = env();
    for &kind in kinds() {
        let (code, line) = probe(&e.ws, kind, Act::SendInputHere, Path::new(""));
        assert_eq!(code, 0, "{} {line}", kind.word());
        if sandbox_user_mode() {
            assert!(
                line.contains(&format!(" on {COMMAND_STATION}")),
                "the command sent input on a window station kGraf did not make for it {}: \
                 {line}",
                kind.word()
            );
            println!("measurement: SendInput {}: {line}", kind.word());
        } else {
            assert!(
                line.starts_with("sent 0 on "),
                "SendInput injected events {}: {line}",
                kind.word()
            );
        }
    }
}

/// The prefix of the window station kGraf's process makes for the commands
/// of one sandbox user (`sandbox_user::windows::launch`, `Role::Command`).
const COMMAND_STATION: &str = "kgraf-cmd-";

/// The command has no console, and its user interface is its own, in the way
/// its backend makes it private. The token sandbox keeps the owner's window
/// station and gives the command a desktop of its own, named
/// `kGrafSandbox-<hex>`. The sandbox user cannot share the owner's station at
/// all, so kGraf's process makes a station for the commands of each sandbox
/// user, named `kgraf-cmd-<hex>`, with one desktop called `Default` on it:
/// there the station name is what says the command is isolated, and a test
/// that read the desktop name alone would have failed on a working machine.
#[test]
fn the_command_runs_on_its_own_desktop_without_a_console() {
    let e = env();
    // Station and desktop of one process.
    let (_, ui) = probe(&e.ws, Kind::Online, Act::Ui, Path::new(""));
    if sandbox_user_mode() {
        assert!(
            ui.starts_with(COMMAND_STATION) && ui.ends_with(r"\Default"),
            "the command runs on {ui:?}, not on a window station kGraf made for it"
        );
    } else {
        // The owner's station, and a desktop of the command's own on it.
        assert!(ui.contains(r"\kGrafSandbox-"), "{ui}");
    }
    let (_, console) = probe(&e.ws, Kind::Online, Act::Console, Path::new(""));
    assert_eq!(console, "false");
}

/// How the user interface objects are shared, per backend. Under the token
/// sandbox two spawns in one workspace reuse one desktop and two workspaces
/// do not. Under the sandbox user kGraf's process makes one station for the
/// commands of each sandbox user: two commands in one workspace run as one
/// slot user and share it, and two workspaces hold two slots, two users and
/// two stations. A hosted Windows self-test measured the first half: two
/// commands of one user ran on `kgraf-cmd-<hex>\Default` stations this
/// process made.
#[test]
fn user_interface_objects_are_shared_the_way_the_backend_shares_them() {
    let e = env();
    let (_, a) = probe(&e.ws, Kind::Online, Act::Ui, Path::new(""));
    let (_, b) = probe(&e.ws, Kind::Online, Act::Ui, Path::new(""));
    let (_, c) = probe(&e.other, Kind::Online, Act::Ui, Path::new(""));
    if sandbox_user_mode() {
        for ui in [&a, &b, &c] {
            assert!(
                ui.starts_with(COMMAND_STATION),
                "a command ran on {ui:?}, not on a window station kGraf made for it"
            );
        }
        assert_eq!(
            a, b,
            "two commands of one sandbox user ran on two window stations"
        );
        assert_ne!(
            a, c,
            "two workspaces, two slot users, shared a window station"
        );
    } else {
        assert_eq!(a, b, "same workspace should reuse one desktop");
        assert_ne!(a, c, "different workspaces should differ");
    }
}

// ---- pipes ----------------------------------------------------------------------

/// Run `f` on a thread and wait at most `limit` for it: a pipe read or write
/// that never completes fails the test instead of hanging the binary.
fn within<T: Send + 'static>(
    limit: Duration,
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("{what} did not complete within {limit:?}"))
}

/// Output read through the pipe in many small `read` calls arrives whole and
/// ends. std's `ChildStdout::read` uses overlapped reads; on a pipe end made
/// for synchronous I/O (`CreatePipe`) such a read could wait forever after
/// the first, so a check saw "no output" and a reader never ended.
#[test]
fn piped_output_read_in_small_pieces_arrives_whole_and_ends() {
    use std::io::Read;
    let e = env();
    let mut child = sandboxed_command(&policy(&e.ws), "cmd", e.ws.root())
        .unwrap()
        .args(["/d", "/c", "for /l %i in (1,1,500) do @echo line-%i"])
        .stdin(Redirect::Null)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Null)
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let text = within(Duration::from_secs(60), "reading stdout", move || {
        let mut all = Vec::new();
        let mut piece = [0u8; 7];
        loop {
            match stdout.read(&mut piece) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&piece[..n]),
                Err(e) => panic!("read failed: {e}"),
            }
        }
        String::from_utf8_lossy(&all).into_owned()
    });
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert_eq!(lines.len(), 500, "{text}");
    assert_eq!(lines.last(), Some(&"line-500"));
    assert!(child.wait().unwrap().success());
}

/// A write to a piped stdin returns, and the command reads it. `cmd` with no
/// `/c` runs the commands it reads from stdin.
#[test]
fn a_write_to_piped_stdin_returns_and_is_read() {
    let e = env();
    let mut child = sandboxed_command(&policy(&e.ws), "cmd", e.ws.root())
        .unwrap()
        .args(["/d", "/q"])
        .stdin(Redirect::Piped)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    within(Duration::from_secs(60), "writing stdin", move || {
        stdin.write_all(b"echo got-%OS%\r\nexit\r\n").unwrap();
    });
    let out = wait_within(child, Duration::from_secs(60));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("got-Windows_NT"),
        "{out:?}"
    );
}

// ---- the toolchain runs -------------------------------------------------------

#[test]
fn a_real_cargo_build_runs_inside_the_sandbox() {
    let e = env();
    let root = e.ws.root();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"cargo built\"); }\n",
    )
    .unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = sandboxed_command(&policy(&e.ws), &cargo, root).unwrap();
    // Not quiet: cargo's own lines say which step stopped, where a hosted Windows self-test had
    // only the exit status 0xC0000409.
    cmd.args(["run", "--offline"])
        .env_remove("CARGO_TARGET_DIR");
    let out = output_within(&mut cmd, Duration::from_secs(300));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        "cargo built"
    );
}

/// A command that runs as the sandbox user opens the jobserver kGraf made
/// for it, by the name `CARGO_MAKEFLAGS` gives, with the rights cargo and
/// rustc ask for (`sandbox_user::jobserver`). A hosted Windows self-test
/// refused that open ("Access is denied"), and refused a named semaphore of
/// the command's own in the session too: `kernelbase.dll` opens the
/// session's object directory itself before it looks up any name, and that
/// directory admits the session's logon SID, which the command holds
/// deny-only (`sandbox_user::named_objects`). Until a fix lets the command
/// open that directory, this test fails on the jobserver open, and cargo and
/// rustc cannot run as the sandbox user.
///
/// Each part of that cause is printed as a `measurement:` line, so a run
/// shows what a fix must change: the owner's reading of the session's
/// directory and of its `Restricted` subdirectory (their owner, access list,
/// label), and as each sandbox user, a named semaphore in the session and in
/// the global namespace, and the directory opened with the rights
/// `kernelbase.dll` asks for first and with traversal alone.
#[test]
fn the_sandbox_user_opens_its_jobserver_and_named_objects_are_measured() {
    let e = env();
    let directory = session_directory();
    println!(
        "measurement: the session's object directory {directory} as the owner reads it: {}",
        directory_sddl(&directory)
    );
    let restricted = format!(r"{directory}\Restricted");
    println!(
        "measurement: the object directory {restricted} as the owner reads it: {}",
        directory_sddl(&restricted)
    );
    for &kind in kinds() {
        let (code, line) = probe(&e.ws, kind, Act::NamedSemaphore, Path::new(""));
        println!(
            "measurement: a named semaphore in the session {}: exit {code}, {line}",
            kind.word()
        );
        let (code, line) = probe(&e.ws, kind, Act::NamedSemaphoreGlobal, Path::new(""));
        println!(
            "measurement: a named semaphore in the global namespace (Global\\) {}: exit \
             {code}, {line}",
            kind.word()
        );
        for (what, access) in [
            (
                "with the rights kernelbase asks for first",
                DIRECTORY_AS_KERNELBASE,
            ),
            ("for traversal alone", DIRECTORY_TRAVERSE),
        ] {
            let (code, line) = probe(
                &e.ws,
                kind,
                Act::OpenSessionDirectory,
                Path::new(&format!("{access:x}")),
            );
            println!(
                "measurement: the session's object directory opened {what} {}: exit {code}, \
                 {line}",
                kind.word()
            );
        }
        if !sandbox_user_mode() {
            continue;
        }
        let (code, line) = probe(&e.ws, kind, Act::OpenJobserver, Path::new(""));
        assert_eq!(
            code,
            0,
            "the command could not open the jobserver kGraf made for it {}: {line}. While \
             the session's object directory refuses the command (the measurement lines \
             above), no named object can be opened, and cargo and rustc cannot run as the \
             sandbox user",
            kind.word()
        );
        assert!(line.starts_with("opened kgraf-jobserver-"), "{line}");
    }
}

/// Whether a sandbox user can read the attributes of each folder above its
/// tree. Git reads them before it writes any ref (`git init`, `git commit`,
/// a branch): it stats each folder of the repository's absolute path from
/// the drive root down and makes the ones it cannot stat
/// (`safe_create_leading_directories`). The traverse privilege lets a
/// command open a path below a folder it cannot list, but not read that
/// folder's own attributes, and a folder in the owner's profile admits only
/// the owner, SYSTEM and administrators. A hosted Windows self-test met it as
/// "cannot lock ref 'HEAD': unable to create directory for
/// .../nested/.git/HEAD" in a check that ran `git init` in a tree in the
/// owner's profile. A measurement, so a run shows which folder refuses.
#[test]
fn the_attributes_of_each_folder_above_the_tree_are_measured() {
    let e = env();
    for &kind in kinds() {
        let (code, line) = probe(&e.ws, kind, Act::StatAncestors, e.ws.root());
        println!(
            "measurement: the attributes of each folder above the tree {}: exit {code}, {line}",
            kind.word()
        );
    }
}

#[test]
fn secrets_are_scrubbed_and_git_credential_helpers_are_reset() {
    let e = env();
    // SAFETY: Windows environment access is serialized by std's lock; the name
    // is unique to this test.
    unsafe { std::env::set_var("KGRAF_WIN_FAKE_TOKEN", "leak") };
    let (_, line) = probe(
        &e.ws,
        Kind::Online,
        Act::Env,
        Path::new("KGRAF_WIN_FAKE_TOKEN"),
    );
    assert_eq!(line, "<unset>");

    common::git(
        e.ws.root(),
        &[
            "config",
            "credential.helper",
            "!f() { echo password=leak; }; f",
        ],
    );
    let mut child = sandboxed_command(&policy(&e.ws), "git", e.ws.root())
        .unwrap()
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Redirect::Piped)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"protocol=https\nhost=example.invalid\n\n")
        .unwrap();
    let out = wait_within(child, Duration::from_secs(60));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // git must have run and asked for the credential. With the helper list
    // reset, nothing answers and no prompt is allowed, so git ends on the
    // disabled prompt and prints no credential at all: an empty stdout is
    // the expected shape here, and on its own it would also be what a git
    // that never started leaves behind.
    // That git ran at all is proved by a git that prints its version in the
    // same sandbox, not by a non-zero exit with something on stderr: the
    // runner also writes there when it cannot start a command, and the leak
    // check below passes on empty output.
    let mut version = sandboxed_command(&policy(&e.ws), "git", e.ws.root()).unwrap();
    version.arg("--version");
    let ran = output_within(&mut version, Duration::from_secs(60));
    assert!(
        String::from_utf8_lossy(&ran.stdout).contains("git version"),
        "git did not print its version in this sandbox within the time allowed, so the \
         credential check below proves nothing: {ran:?}"
    );
    assert!(
        stdout.contains("protocol=https") || (!out.status.success() && !stderr.trim().is_empty()),
        "git credential fill neither printed a credential nor failed with a message: {out:?}"
    );
    assert!(!stdout.contains("leak"), "{out:?}");
}

/// A host that sets GIT_CONFIG_COUNT, here with an `extraheader` entry that
/// carries a (fake) token, does not hand its entries to the command: git
/// sees only the sandbox's own, the credential.helper reset among them, and
/// still runs.
#[test]
fn git_config_entries_from_the_host_do_not_reach_the_command() {
    const HOST: [(&str, &str); 5] = [
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "http.https://example.com/.extraheader"),
        (
            "GIT_CONFIG_VALUE_0",
            "AUTHORIZATION: bearer fake-test-value",
        ),
        ("GIT_CONFIG_KEY_1", "user.name"),
        ("GIT_CONFIG_VALUE_1", "Preset"),
    ];
    let e = env();
    // SAFETY: single-threaded test run.
    unsafe {
        for (name, value) in HOST {
            std::env::set_var(name, value);
        }
    }
    let mut cmd = sandboxed_command(&policy(&e.ws), "git", e.ws.root()).unwrap();
    cmd.args(["config", "--list", "--show-scope"]);
    let out = output_within(&mut cmd, Duration::from_secs(60));
    unsafe {
        for (name, _) in HOST {
            std::env::remove_var(name);
        }
    }
    assert!(out.status.success(), "git failed: {:?}", out.status);
    let listing = String::from_utf8_lossy(&out.stdout);
    let command: Vec<&str> = listing
        .lines()
        .filter_map(|l| l.strip_prefix("command\t"))
        .collect();
    assert!(
        !command.iter().any(|l| l.contains("extraheader")
            || l.contains("fake-test-value")
            || l.contains("Preset")),
        "a host entry reached the command"
    );
    assert!(command.contains(&"credential.helper="), "{command:?}");
    assert!(command.contains(&"core.autocrlf=false"), "{command:?}");
}

#[test]
fn inheriting_a_console_stdin_is_refused_or_passes_a_non_console() {
    // Under `cargo test` stdin is usually a console or a pipe. A console must
    // be refused (keystroke injection); anything else must run.
    let e = env();
    let result = sandboxed_command(&policy(&e.ws), "cmd.exe", e.ws.root())
        .unwrap()
        .args(["/d", "/c", "exit", "0"])
        .stdin(Redirect::Inherit)
        .status();
    match result {
        Err(SandboxError::PolicyNotEnforceable { what, .. }) => {
            assert!(what.contains("console input"), "{what}")
        }
        Ok(status) => assert!(status.success()),
        Err(other) => panic!("{other}"),
    }
}
