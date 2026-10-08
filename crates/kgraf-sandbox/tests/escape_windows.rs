//! Escape attempts against the real Windows backend (acceptance: "verified by an
//! escape attempt test on each supported platform").
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for x86_64-pc-windows-msvc only. Run it
//! on Windows 10 or later, as a normal (non-elevated) user, with git on PATH:
//! `cargo test -p kgraf-sandbox --test escape_windows -- --test-threads=1`.
//! Single-threaded because several tests set process environment variables.
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

use common::fixture;
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

/// The in-sandbox half. A no-op unless `KGRAF_PROBE` is set.
#[test]
fn probe_entry() {
    let Ok(action) = std::env::var("KGRAF_PROBE") else {
        return;
    };
    let arg = std::env::var("KGRAF_PROBE_ARG").unwrap_or_default();
    let result = match action.as_str() {
        "write" => std::fs::write(&arg, b"escaped").map(|()| "written".into()),
        "delete" => std::fs::remove_file(&arg).map(|()| "deleted".into()),
        "rename" => std::fs::rename(&arg, format!("{arg}.moved")).map(|()| "renamed".into()),
        "read" => std::fs::read_to_string(&arg),
        // WRITE_DAC and WRITE_OWNER are not in FILE_GENERIC_WRITE. If a
        // write-restricted token did not treat them as writes, the owner's
        // implicit WRITE_DAC would let the sandbox grant itself access to any
        // file the user owns.
        "open_write_dac" => open_with(&arg, 0x0004_0000),
        "open_write_owner" => open_with(&arg, 0x0008_0000),
        "env" => Ok(std::env::var(&arg).unwrap_or_else(|_| "<unset>".into())),
        "sleep" => {
            std::thread::sleep(std::time::Duration::from_secs(120));
            Ok("woke".into())
        }
        "spawn_sleeper" => spawn_self("sleep", "", 0).map(|child| child.id().to_string()),
        "breakaway" => {
            // CREATE_BREAKAWAY_FROM_JOB: the job has no BREAKAWAY_OK flag.
            spawn_self("sleep", "", 0x0100_0000).map(|c| format!("escaped pid {}", c.id()))
        }
        "open_process_for_write" => open_process(arg.parse().unwrap_or(0), true),
        "open_process_vm_read" => open_process(arg.parse().unwrap_or(0), false),
        "hardlink_write" => hardlink_write(&arg, &arg2()),
        "regwrite_run_key" => reg_write_run_key(&arg),
        "open_pipe" => open_with(&arg, 0xC000_0000), // GENERIC_READ | GENERIC_WRITE
        "attach_console" => attach_console(arg.parse().unwrap_or(0)),
        "send_input" => send_input(),
        "junction_write" => junction_write(&arg, &arg2()),
        "user_sid" => own_user_sid(),
        "connect" => connect_to(&arg),
        "desktop" => desktop_name(),
        "console" => Ok(console_window().to_string()),
        other => Err(std::io::Error::other(format!("unknown probe {other}"))),
    };
    exit_with(result)
}

fn spawn_self(action: &str, arg: &str, flags: u32) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_PROBE", action)
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

fn reg_write_run_key(value_name: &str) -> std::io::Result<String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegOpenKeyExW, RegSetValueExW,
    };
    let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut key = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey; output written on success.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
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
    Env {
        f,
        _mgr: mgr,
        ws,
        other,
        outside,
    }
}

fn policy(ws: &Workspace) -> SandboxPolicy {
    SandboxPolicy::for_workspace(ws, Network::Allow).accept_partial_isolation(true)
}

fn self_exe() -> PathBuf {
    std::env::current_exe().unwrap()
}

fn probe_output(policy: &SandboxPolicy, cwd: &Path, action: &str, arg: &str, arg2: &str) -> Output {
    sandboxed_command(policy, self_exe(), cwd)
        .unwrap()
        .args([
            "--exact",
            "probe_entry",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env("KGRAF_PROBE", action)
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
    cmd.stdin(Redirect::Null)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped);
    wait_within(cmd.spawn().unwrap(), limit)
}

/// [`output_within`] for a child that is already running.
fn wait_within(mut child: kgraf_sandbox::SandboxedChild, limit: Duration) -> Output {
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
    let quiet = child.kill_tree_and_wait(Duration::from_secs(10)).unwrap();
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

fn probe(ws: &Workspace, action: &str, arg: &Path) -> (i32, String) {
    parse(&probe_output(
        &policy(ws),
        ws.root(),
        action,
        &arg.to_string_lossy(),
        "",
    ))
}

fn probe2(ws: &Workspace, action: &str, arg: &Path, arg2: &Path) -> (i32, String) {
    parse(&probe_output(
        &policy(ws),
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

fn assert_denied(ws: &Workspace, action: &str, target: &Path) {
    let (code, line) = probe(ws, action, target);
    assert_eq!(code, DENIED, "{action} {}: {line}", target.display());
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
        .env("KGRAF_PROBE", "sleep")
        .spawn()
        .unwrap();
    (child.id(), child)
}

// ---- policy plumbing ----------------------------------------------------------

#[test]
fn the_strength_indicator_reports_partial_isolation() {
    let s = platform_isolation().unwrap();
    assert!(!s.network_deniable && !s.writes_confined);
    assert_eq!(s.reads, ReadConfinement::Unconfined);
    assert!(
        s.gaps.contains("network egress is not blocked"),
        "{}",
        s.gaps
    );
    assert!(s.gaps.contains("PROCESS_VM_READ"), "{}", s.gaps);
}

#[test]
fn partial_isolation_is_refused_without_opt_in() {
    let e = env();
    let err = sandboxed_command(
        &SandboxPolicy::for_workspace(&e.ws, Network::Allow),
        "cmd.exe",
        e.ws.root(),
    )
    .unwrap_err();
    assert!(
        matches!(err, SandboxError::PartialIsolation { .. }),
        "{err}"
    );
}

#[test]
fn network_deny_and_socket_allowlists_are_refused() {
    let e = env();
    let deny = SandboxPolicy::for_workspace(&e.ws, Network::Deny).accept_partial_isolation(true);
    let err = sandboxed_command(&deny, "cmd.exe", e.ws.root()).unwrap_err();
    assert!(
        matches!(err, SandboxError::PolicyNotEnforceable { .. }),
        "{err}"
    );
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
    for target in [e.ws.root().join("new.txt"), e.ws.scratch().join("t.txt")] {
        let (code, line) = probe(&e.ws, "write", &target);
        assert_eq!(code, 0, "{line}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "escaped");
    }
}

#[test]
fn write_outside_the_workspace_is_denied() {
    let e = env();
    let target = e.outside.path().join("escaped.txt");
    assert_denied(&e.ws, "write", &target);
    assert!(!target.exists());
}

#[test]
fn write_into_the_user_profile_is_denied() {
    let e = env();
    let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
    let target = profile.join(format!("kgraf-escape-{}.txt", std::process::id()));
    assert_denied(&e.ws, "write", &target);
    assert!(!target.exists());
}

#[test]
fn writes_to_a_sibling_workspace_and_the_main_repo_are_denied() {
    let e = env();
    let sibling = e.other.root().join("from-w.txt");
    assert_denied(&e.ws, "write", &sibling);
    assert!(!sibling.exists());
    let readme = e.f.repo.join("README");
    assert_denied(&e.ws, "write", &readme);
    assert_eq!(std::fs::read_to_string(readme).unwrap(), "hello\n");
}

#[test]
fn an_existing_outside_file_cannot_be_modified_or_deleted() {
    let e = env();
    let target = e.outside.path().join("keep.txt");
    std::fs::write(&target, "original").unwrap();
    assert_denied(&e.ws, "write", &target);
    assert_denied(&e.ws, "delete", &target);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
}

#[test]
fn the_owner_cannot_rewrite_the_dacl_of_an_outside_file() {
    let e = env();
    let target = e.outside.path().join("owned.txt");
    std::fs::write(&target, "original").unwrap();
    assert_denied(&e.ws, "open_write_dac", &target);
    assert_denied(&e.ws, "open_write_owner", &target);
    assert_denied(&e.ws, "open_write_dac", e.outside.path());
}

/// An Everyone-full-control directory is DACL-writable, but the Low integrity
/// level blocks a write up to it while it stays at Medium. If this ever
/// succeeds, the Low label did not take and STRENGTH must change.
#[test]
fn write_into_an_everyone_writable_directory_is_denied() {
    let e = env();
    let dir = e.outside.path().join("world");
    std::fs::create_dir_all(&dir).unwrap();
    let status = std::process::Command::new("icacls.exe")
        .args([
            dir.to_string_lossy().as_ref(),
            "/grant",
            "*S-1-1-0:(OI)(CI)F",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "icacls grant failed");
    let target = dir.join("planted.txt");
    assert_denied(&e.ws, "write", &target);
    assert!(!target.exists());
}

#[test]
fn a_hard_link_to_an_outside_file_cannot_escape() {
    let e = env();
    let outside = e.outside.path().join("secret.txt");
    std::fs::write(&outside, "original").unwrap();
    let link = e.ws.root().join("link.txt");
    let (code, line) = probe2(&e.ws, "hardlink_write", &link, &outside);
    assert_ne!(code, 0, "hard-link write succeeded: {line}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
}

#[test]
fn a_junction_inside_the_root_cannot_escape() {
    let e = env();
    let outside = e.outside.path().join("target-dir");
    std::fs::create_dir_all(&outside).unwrap();
    let link = e.ws.root().join("jn");
    let (code, line) = probe2(&e.ws, "junction_write", &link, &outside);
    assert_ne!(code, 0, "junction write succeeded: {line}");
    assert!(!outside.join("planted.txt").exists(), "{line}");
}

/// A planted `scratch\.git` junction must be refused at prepare, so the
/// host never rewrites DACLs across the junction target (the user profile).
#[test]
fn a_planted_git_junction_is_refused_at_prepare() {
    let e = env();
    let victim = e.outside.path().join("victim-profile");
    std::fs::create_dir_all(&victim).unwrap();
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
    assert!(matches!(err, SandboxError::PlatformSetup { .. }), "{err}");
    // The victim's DACL is untouched: it still holds no capability deny ACE we
    // would have added. A coarse check: the directory is still readable/usable.
    assert!(victim.is_dir());
}

/// Opening the OpenSSH agent pipe for read/write must not succeed: if the
/// agent runs, its Medium pipe is blocked to a Low process; if it does not,
/// the open fails with not-found. Either way the sandbox gains no agent access.
#[test]
fn the_ssh_agent_pipe_cannot_be_opened() {
    let e = env();
    let (code, line) = probe(&e.ws, "open_pipe", Path::new(r"\\.\pipe\openssh-ssh-agent"));
    assert_ne!(code, 0, "opened the ssh-agent pipe: {line}");
}

#[test]
fn the_registry_run_key_cannot_be_written() {
    let e = env();
    let value = format!("kgraf-escape-{}", std::process::id());
    let (code, line) = probe(&e.ws, "regwrite_run_key", Path::new(&value));
    assert_ne!(code, 0, "HKCU Run key written: {line}");
}

// ---- reads (documented gaps) --------------------------------------------------

#[test]
fn reads_outside_the_workspace_are_not_confined() {
    let e = env();
    let target = e.outside.path().join("readable.txt");
    std::fs::write(&target, "visible").unwrap();
    let (code, line) = probe(&e.ws, "read", &target);
    assert_eq!((code, line.as_str()), (0, "visible"));
}

/// Whether a command can open the harness process for `PROCESS_VM_READ`,
/// against what the backend in use claims.
///
/// Under the token sandbox this is a documented gap: the command runs as the
/// same Windows user, so the harness process's own access list admits it,
/// Low integrity blocks writes up and not reads, and the harness holds
/// provider keys while egress is open. Under the sandbox user the command
/// runs as another user, which that list does not admit, so the open must
/// fail; that is one of the reasons for decision 0097.
///
/// A refusal under the token sandbox and a probe that failed for some other
/// reason are different findings, and this says which it was: the first means
/// the strength text must drop the claim, the second means the run proved
/// nothing. Run 7 reported this test failing without saying which.
#[test]
fn the_harness_process_vm_read_matches_what_the_backend_claims() {
    let e = env();
    let harness = std::process::id().to_string();
    let (code, line) = probe(&e.ws, "open_process_vm_read", Path::new(&harness));
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
            "VM_READ of the harness works, and the strength text no longer claims that \
             gap: put it back, or fix the backend: {line}"
        ),
        (_, DENIED) => panic!(
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

/// Whether this machine runs commands as a sandbox user.
fn sandbox_user_mode() -> bool {
    platform_isolation().expect("a backend").backend == Backend::WindowsSandboxUser
}

/// A command runs as a sandbox user, never as the owner. That is what keeps
/// it out of the owner's own files: the access lists of the owner's profile
/// admit the owner, SYSTEM and administrators, and none of those is the
/// command.
#[test]
fn a_command_runs_as_a_sandbox_user_and_not_as_the_owner() {
    let e = env();
    let (code, line) = probe(&e.ws, "user_sid", Path::new(""));
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
    let slots = users
        .iter()
        .filter(|(u, _)| matches!(u.role, Role::Slot(_)))
        .count();
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
    if slots == 0 {
        let err = second.expect_err("a second worktree took the only offline user");
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
    let run = |ws: &Workspace, slot: &WorkerSlot, action: &str, arg: &str| {
        let policy = SandboxPolicy::for_workspace(ws, Network::Deny)
            .accept_partial_isolation(true)
            .with_worker_slot(slot.clone());
        parse(&probe_output(&policy, ws.root(), action, arg, ""))
    };
    for (ws, slot) in [(&e.ws, &a), (&e.other, &b)] {
        let (code, line) = run(ws, slot, "user_sid", "");
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
    let (code, line) = run(&e.ws, &a, "write", &target.to_string_lossy());
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
    let Ok(Some(slot2)) = worker_slot(&t2) else {
        // One offline user: the test above covers the refusal.
        return;
    };
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
        "write",
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
    let (code, line) = probe(&e.ws, "read", &target);
    let _ = std::fs::remove_file(&target);
    if sandbox_user_mode() {
        assert_eq!(
            code, DENIED,
            "a sandbox user read a file in the owner's profile: {line}"
        );
    } else {
        assert_eq!(
            (code, line.as_str()),
            (0, "owner's own file"),
            "the token sandbox does not confine reads; the strength text says so"
        );
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

/// A command whose policy denies the network cannot reach a listener on this
/// machine. The firewall rules of the offline and slot users block loopback
/// outside kGraf's own proxy ports, so a listener on any other port is
/// unreachable.
#[test]
fn a_command_without_the_network_cannot_reach_a_local_listener() {
    if !sandbox_user_mode() {
        // The token sandbox cannot deny the network at all, and refuses the
        // policy instead; `STRENGTH.network_deniable` says so.
        return;
    }
    let e = env();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind a local listener");
    let port = listener.local_addr().expect("address").port();
    assert!(
        !kgraf_sandbox::sandbox_user::PROXY_PORTS.contains(&port),
        "the listener took one of kGraf's proxy ports, which stay reachable; run again"
    );
    std::thread::spawn(move || {
        for client in listener.incoming() {
            drop(client);
        }
    });
    let policy = SandboxPolicy::for_workspace(&e.ws, Network::Deny).accept_partial_isolation(true);
    let out = probe_output(
        &policy,
        e.ws.root(),
        "connect",
        &format!("127.0.0.1:{port}"),
        "",
    );
    let (code, line) = parse(&out);
    assert_ne!(
        code, 0,
        "a command with the network denied reached a local listener: {line}"
    );
}

// ---- process, job, console, desktop ------------------------------------------

/// A second sandbox must not open this sandbox's process for write.
#[test]
fn one_sandbox_cannot_open_another_sandboxs_process_for_write() {
    let e = env();
    let (victim_pid, victim) = spawn_sleeper(&e.other);
    let attacker = probe(
        &e.ws,
        "open_process_for_write",
        Path::new(&victim_pid.to_string()),
    );
    // Kill the victim before asserting so a failure does not leak it.
    drop(victim);
    assert_eq!(attacker.0, DENIED, "cross-sandbox VM_WRITE: {}", attacker.1);
}

#[test]
fn the_sandbox_cannot_open_the_host_process_for_writing() {
    let e = env();
    let (code, line) = probe(
        &e.ws,
        "open_process_for_write",
        Path::new(&std::process::id().to_string()),
    );
    assert_eq!(code, DENIED, "{line}");
}

#[test]
fn a_child_cannot_break_away_from_the_job() {
    let e = env();
    let (code, line) = probe(&e.ws, "breakaway", Path::new(""));
    assert_eq!(code, DENIED, "{line}");
}

#[test]
fn descendants_do_not_outlive_the_command() {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let e = env();
    let (code, line) = probe(&e.ws, "spawn_sleeper", Path::new(""));
    assert_eq!(code, 0, "{line}");
    let pid: u32 = line.parse().unwrap();
    // SAFETY: plain calls; a null handle means the process is already gone.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            let owned = <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(h);
            assert_eq!(
                WaitForSingleObject(owned.as_raw_handle(), 10_000),
                WAIT_OBJECT_0,
                "grandchild {pid} still running"
            );
        }
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
        .env("KGRAF_PROBE", "sleep")
        .spawn()
        .unwrap();
    let process = child.process_handle().try_clone_to_owned().unwrap();
    drop(child);
    // SAFETY: a process handle we own.
    let r = unsafe {
        windows_sys::Win32::System::Threading::WaitForSingleObject(process.as_raw_handle(), 10_000)
    };
    assert_eq!(r, windows_sys::Win32::Foundation::WAIT_OBJECT_0);
}

#[test]
fn the_sandbox_cannot_attach_to_the_host_console() {
    let e = env();
    let (code, line) = probe(
        &e.ws,
        "attach_console",
        Path::new(&std::process::id().to_string()),
    );
    assert_ne!(code, 0, "AttachConsole to the host succeeded: {line}");
}

/// UIPI plus a separate, Low-labeled desktop: a synthetic keystroke must reach
/// nothing. The observable effect is that SendInput injects zero events.
#[test]
fn send_input_to_the_user_desktop_is_blocked() {
    let e = env();
    let (code, line) = probe(&e.ws, "send_input", Path::new(""));
    assert_eq!(code, 0, "{line}");
    assert_eq!(line, "sent 0", "SendInput injected events: {line}");
}

#[test]
fn the_command_runs_on_a_private_desktop_without_a_console() {
    let e = env();
    let (_, desktop) = probe(&e.ws, "desktop", Path::new(""));
    assert!(desktop.starts_with("kGrafSandbox-"), "{desktop}");
    let (_, console) = probe(&e.ws, "console", Path::new(""));
    assert_eq!(console, "false");
}

/// Two spawns in one workspace reuse one desktop; two workspaces do not.
#[test]
fn the_private_desktop_is_reused_per_workspace() {
    let e = env();
    let (_, d1) = probe(&e.ws, "desktop", Path::new(""));
    let (_, d2) = probe(&e.ws, "desktop", Path::new(""));
    assert_eq!(d1, d2, "same workspace should reuse one desktop");
    let (_, d3) = probe(&e.other, "desktop", Path::new(""));
    assert_ne!(d1, d3, "different workspaces should differ");
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
    cmd.args(["run", "--offline", "-q"])
        .env_remove("CARGO_TARGET_DIR");
    let out = output_within(&mut cmd, Duration::from_secs(300));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        "cargo built"
    );
}

#[test]
fn secrets_are_scrubbed_and_git_credential_helpers_are_reset() {
    let e = env();
    // SAFETY: Windows environment access is serialized by std's lock; the name
    // is unique to this test.
    unsafe { std::env::set_var("KGRAF_WIN_FAKE_TOKEN", "leak") };
    let (_, line) = probe(&e.ws, "env", Path::new("KGRAF_WIN_FAKE_TOKEN"));
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
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("leak"),
        "{out:?}"
    );
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
