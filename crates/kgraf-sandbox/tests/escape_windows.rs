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

use common::fixture;
use kgraf_sandbox::{
    Network, ReadConfinement, Redirect, SandboxError, SandboxPolicy, Workspace, WorkspaceManager,
    WorkspaceSpec, platform_isolation, sandboxed_command,
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

/// The documented VM_READ gap. A Low sandbox can open the Medium harness
/// process for PROCESS_VM_READ, and the harness holds provider keys. This test
/// asserts the gap so a future change that closes it prompts updating STRENGTH.
#[test]
fn the_harness_process_can_be_opened_for_vm_read() {
    let e = env();
    let harness = std::process::id().to_string();
    let (code, line) = probe(&e.ws, "open_process_vm_read", Path::new(&harness));
    assert_eq!(code, 0, "VM_READ of the harness is now blocked: {line}");
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
    let out = sandboxed_command(&policy(&e.ws), &cargo, root)
        .unwrap()
        .args(["run", "--offline", "-q"])
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
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
    let out = child.wait_with_output().unwrap();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("leak"),
        "{out:?}"
    );
}

/// A host that already sets GIT_CONFIG_COUNT keeps its own entries and the
/// credential.helper reset is appended, not dropped.
#[test]
fn git_config_count_from_the_host_is_extended_not_dropped() {
    let e = env();
    // SAFETY: single-threaded test run.
    unsafe {
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "user.name");
        std::env::set_var("GIT_CONFIG_VALUE_0", "Preset");
    }
    let out = sandboxed_command(&policy(&e.ws), "git", e.ws.root())
        .unwrap()
        .args(["config", "--get-all", "--show-scope", "user.name"])
        .output()
        .unwrap();
    unsafe {
        std::env::remove_var("GIT_CONFIG_COUNT");
        std::env::remove_var("GIT_CONFIG_KEY_0");
        std::env::remove_var("GIT_CONFIG_VALUE_0");
    }
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Preset"),
        "host GIT_CONFIG entry was dropped: {out:?}"
    );
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
