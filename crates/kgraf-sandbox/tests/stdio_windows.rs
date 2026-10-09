//! A sandboxed command's standard handles on Windows: no other spawn of this
//! process takes a copy of them, and its standard input is never the owner's
//! console. Runs on whichever Windows backend is active (the token sandbox,
//! or the sandbox user after `kgraf sandbox setup`).
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for x86_64-pc-windows-msvc only. Run it
//! on Windows 10 or later, as a normal (non-elevated) user:
//! `cargo test -p kgraf-sandbox --test stdio_windows -- --test-threads=1`.
//!
//! The probe is this test binary itself: [`probe_entry`] does nothing in a
//! normal run, and when `KGRAF_STDIO_PROBE` is set it performs one check,
//! prints `PROBE=<result>` and exits: 0 when the check holds, 1 when it does
//! not, 3 when the check could not be made.

#![cfg(windows)]

mod common;

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::fixture;
use kgraf_sandbox::{
    Network, Redirect, SandboxError, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec,
    sandboxed_command,
};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileType, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    AllocConsole, FreeConsole, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetStdHandle,
};

const HOLDS: i32 = 0;
const BROKEN: i32 = 1;
const OTHER_ERR: i32 = 3;

// ---- probe side -------------------------------------------------------------

/// The command's standard input is not a console: no handle, or one that
/// `GetConsoleMode` refuses (the NUL device and pipes do).
fn check_stdin_not_console() -> std::io::Result<(bool, String)> {
    // SAFETY: plain call; the handle is borrowed.
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Ok((true, "no standard input".into()));
    }
    let mut mode = 0;
    // SAFETY: probes a valid handle.
    let console = unsafe { GetConsoleMode(h, &mut mode) } != 0;
    // SAFETY: as above.
    let kind = unsafe { GetFileType(h) };
    Ok((!console, format!("console={console} file_type={kind}")))
}

/// Runs in a process of its own (the host side starts it): give that
/// process a console of its own as standard input, then ask the sandbox to
/// pass it to a command. The sandbox must refuse.
fn check_console_stdin_refused() -> std::io::Result<(bool, String)> {
    // SAFETY: changes only this probe process's console.
    unsafe {
        FreeConsole();
        if AllocConsole() == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let name: Vec<u16> = "CONIN$\0".encode_utf16().collect();
    // SAFETY: a NUL-terminated name; the handle is kept for the process's
    // life as its standard input.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: sets this process's standard input to the console just opened.
    if unsafe { SetStdHandle(STD_INPUT_HANDLE, h) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let dir = PathBuf::from(std::env::var_os("KGRAF_STDIO_PROBE_ROOT").unwrap_or_default());
    let ws_root = dir.clone();
    let policy = SandboxPolicy::new(vec![ws_root], Network::Deny).accept_partial_isolation(true);
    let result = sandboxed_command(&policy, "cmd.exe", &dir)
        .map_err(std::io::Error::other)?
        .args(["/d", "/c", "exit", "0"])
        .stdin(Redirect::Inherit)
        .status();
    match result {
        Err(SandboxError::PolicyNotEnforceable { what, .. }) if what.contains("console input") => {
            Ok((true, format!("refused: {what}")))
        }
        Err(other) => Ok((false, format!("another error: {other}"))),
        Ok(status) => Ok((false, format!("the command ran with the console: {status}"))),
    }
}

/// The in-sandbox half. A no-op unless `KGRAF_STDIO_PROBE` is set.
#[test]
fn probe_entry() {
    let Ok(action) = std::env::var("KGRAF_STDIO_PROBE") else {
        return;
    };
    let result = match action.as_str() {
        "stdin_not_console" => check_stdin_not_console(),
        "console_stdin_refused" => check_console_stdin_refused(),
        other => Err(std::io::Error::other(format!("unknown probe {other}"))),
    };
    let (code, line) = match result {
        Ok((true, text)) => (HOLDS, text),
        Ok((false, text)) => (BROKEN, text),
        Err(e) => (OTHER_ERR, format!("error {e}")),
    };
    println!("PROBE={line}");
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

// ---- host side ----------------------------------------------------------------

struct Env {
    _f: common::Fixture,
    _mgr: WorkspaceManager,
    ws: Workspace,
}

fn env() -> Env {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr
        .create(&WorkspaceSpec {
            name: "w",
            base: "HEAD",
            branch: None,
        })
        .unwrap();
    Env {
        _f: f,
        _mgr: mgr,
        ws,
    }
}

fn policy(ws: &Workspace) -> SandboxPolicy {
    SandboxPolicy::for_workspace(ws, Network::Deny).accept_partial_isolation(true)
}

fn self_exe() -> PathBuf {
    std::env::current_exe().unwrap()
}

fn probe_args() -> [&'static str; 5] {
    [
        "--exact",
        "probe_entry",
        "--nocapture",
        "--test-threads=1",
        "-q",
    ]
}

fn probe_line(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|l| l.strip_prefix("PROBE="))
        .unwrap_or("<no probe line>")
        .to_string()
}

/// Unrelated children started through `kgraf_core::child_run::run` (as
/// kGraf's git runs are) on four threads, each living about five seconds,
/// until `stop` is set. Before the fix every such spawn copied every
/// sandboxed command's pipe ends that existed at that moment, and the
/// command's reader waited for that child.
fn unrelated_spawns(stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<u32>> {
    (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    let mut cmd = Command::new("cmd.exe");
                    cmd.args(["/d", "/c", "ping -n 6 127.0.0.1 >NUL"]);
                    let _ = kgraf_core::child_run::run(
                        cmd,
                        "unrelated ping",
                        None,
                        kgraf_core::child_run::Limits::new(Duration::from_secs(60)),
                    );
                    n += 1;
                }
                n
            })
        })
        .collect()
}

#[test]
fn a_concurrent_unrelated_spawn_does_not_hold_a_sandboxed_commands_output() {
    let e = env();
    let stop = Arc::new(AtomicBool::new(false));
    let spawners = unrelated_spawns(stop.clone());
    let held_before = kgraf_core::waits::pipes_held_count();
    let mut slowest = Duration::ZERO;
    for i in 0..20 {
        let started = Instant::now();
        let out = sandboxed_command(&policy(&e.ws), "cmd.exe", e.ws.root())
            .unwrap()
            .args(["/d", "/c", "echo", "out"])
            .stdin(Redirect::Null)
            .stdout(Redirect::Piped)
            .stderr(Redirect::Piped)
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        let took = started.elapsed();
        slowest = slowest.max(took);
        assert!(out.status.success(), "run {i}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("out"),
            "run {i}: {out:?}"
        );
    }
    stop.store(true, Ordering::Relaxed);
    let spawned: u32 = spawners.into_iter().map(|h| h.join().unwrap()).sum();
    // An unrelated child holding a pipe end keeps the reader waiting for the
    // whole drain grace, and the wait records it under the command's name.
    let report = kgraf_core::waits::report();
    let held: Vec<&str> = report
        .lines()
        .filter(|l| l.contains("pipes held") && l.contains("sandboxed command"))
        .collect();
    assert!(
        held.is_empty(),
        "a sandboxed command's pipe stayed open after its tree ended \
         (pipes held before the test: {held_before}): {held:?}"
    );
    println!("20 sandboxed commands beside {spawned} unrelated spawns; slowest {slowest:?}");
}

#[test]
fn a_commands_standard_input_is_never_a_console() {
    let e = env();
    let out = sandboxed_command(&policy(&e.ws), self_exe(), e.ws.root())
        .unwrap()
        .args(probe_args())
        .env("KGRAF_STDIO_PROBE", "stdin_not_console")
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped)
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();
    let line = probe_line(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(HOLDS),
        "{line}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_inherited_console_as_standard_input_is_refused() {
    // In a process of its own, which gives itself a console: this test
    // process's standard input may or may not be one.
    let e = env();
    let out = Command::new(self_exe())
        .args(probe_args())
        .env("KGRAF_STDIO_PROBE", "console_stdin_refused")
        .env("KGRAF_STDIO_PROBE_ROOT", e.ws.root())
        .output()
        .unwrap();
    let line = probe_line(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(HOLDS),
        "{line}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
