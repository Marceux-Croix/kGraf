//! What a command can do to its own runner on Windows (decision 0097).
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for x86_64-pc-windows-msvc only. Run it
//! on Windows 10 or later, as a normal (non-elevated) user, after
//! `kgraf sandbox setup` (or with `KGRAF_SANDBOX_TAG` naming a tagged setup):
//! `cargo test -p kgraf-sandbox --test runner_windows -- --test-threads=1`.
//! Without the sandbox user backend every test prints why it skipped and
//! passes, because there is no runner to test. Set
//! `KGRAF_REQUIRE_SANDBOX_USER=1` where the run is about the sandbox user,
//! and a machine without it fails these tests instead of skipping them.
//!
//! The runner and the command run as one sandbox user, and the runner holds
//! the owner's logon SID, which the command must not get. So the command must
//! not be able to open the runner's process, any of its threads, or its token
//! for anything but waiting and reading, and the runner must have no console
//! host, which would run with the runner's token and admit that user fully.
//!
//! The probe is this test binary itself: [`probe_entry`] does nothing in a
//! normal run, and when `KGRAF_RUNNER_PROBE` is set it performs one check,
//! prints `PROBE=<result>` and exits: 0 when the check holds, 1 when it does
//! not, 3 when the check could not be made.

#![cfg(windows)]

mod common;

use std::io::{Read, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Output;

use common::fixture;
use kgraf_sandbox::{
    Network, Redirect, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec,
    platform_isolation, sandboxed_command,
};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};

const HOLDS: i32 = 0;
const BROKEN: i32 = 1;
const OTHER_ERR: i32 = 3;

// ---- probe side -------------------------------------------------------------

/// One process of a snapshot: ID, parent ID, image name.
struct ProcessEntry {
    pid: u32,
    parent: u32,
    image: String,
}

fn owned(h: HANDLE) -> std::io::Result<OwnedHandle> {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh handle the caller now owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

fn processes() -> std::io::Result<Vec<ProcessEntry>> {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: a snapshot of every process; owned below.
    let snap = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    // SAFETY: zeroed is a valid initial PROCESSENTRY32W; `dwSize` set below.
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    // SAFETY: a valid snapshot and entry.
    let mut more = unsafe { Process32FirstW(snap.as_raw_handle(), &mut e) } != 0;
    while more {
        let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(0);
        out.push(ProcessEntry {
            pid: e.th32ProcessID,
            parent: e.th32ParentProcessID,
            image: String::from_utf16_lossy(&e.szExeFile[..len]),
        });
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap.as_raw_handle(), &mut e) } != 0;
    }
    Ok(out)
}

/// The process that started this one, which must be the runner.
fn runner_pid() -> std::io::Result<u32> {
    let me = std::process::id();
    let all = processes()?;
    let parent = all
        .iter()
        .find(|p| p.pid == me)
        .map(|p| p.parent)
        .ok_or_else(|| std::io::Error::other("this process is not in the process list"))?;
    let image = all
        .iter()
        .find(|p| p.pid == parent)
        .map(|p| p.image.to_ascii_lowercase())
        .unwrap_or_default();
    if image != "kgraf-sandbox-runner.exe" {
        return Err(std::io::Error::other(format!(
            "the parent process {parent} is {image:?}, not the runner"
        )));
    }
    Ok(parent)
}

fn threads_of(pid: u32) -> std::io::Result<Vec<u32>> {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: a snapshot of every thread; owned below.
    let snap = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    // SAFETY: zeroed is a valid initial THREADENTRY32; `dwSize` set below.
    let mut e: THREADENTRY32 = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut out = Vec::new();
    // SAFETY: a valid snapshot and entry.
    let mut more = unsafe { Thread32First(snap.as_raw_handle(), &mut e) } != 0;
    while more {
        if e.th32OwnerProcessID == pid {
            out.push(e.th32ThreadID);
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snap.as_raw_handle(), &mut e) } != 0;
    }
    Ok(out)
}

/// `Ok(None)` when every `(name, access)` is refused with access denied,
/// `Ok(Some(name))` for the first one that opened, an error for any other
/// failure. `open` returns the raw handle of one attempt.
fn all_refused(
    attempts: &[(&'static str, u32)],
    open: impl Fn(u32) -> HANDLE,
) -> std::io::Result<Option<&'static str>> {
    for (name, access) in attempts {
        let h = open(*access);
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            drop(owned(h)?);
            return Ok(Some(name));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
            return Err(std::io::Error::other(format!("{name}: {e}")));
        }
    }
    Ok(None)
}

/// Every thread of the runner refuses the rights that would let this
/// process run code on it or change it.
fn check_runner_threads() -> std::io::Result<(bool, String)> {
    use windows_sys::Win32::Storage::FileSystem::{WRITE_DAC, WRITE_OWNER};
    use windows_sys::Win32::System::Threading::{
        OpenThread, THREAD_DIRECT_IMPERSONATION, THREAD_GET_CONTEXT, THREAD_IMPERSONATE,
        THREAD_SET_CONTEXT, THREAD_SET_INFORMATION, THREAD_SET_THREAD_TOKEN, THREAD_SUSPEND_RESUME,
        THREAD_TERMINATE,
    };
    let runner = runner_pid()?;
    let threads = threads_of(runner)?;
    if threads.is_empty() {
        return Err(std::io::Error::other(
            "the runner has no threads in the list",
        ));
    }
    let attempts = [
        ("THREAD_SET_CONTEXT", THREAD_SET_CONTEXT),
        ("THREAD_GET_CONTEXT", THREAD_GET_CONTEXT),
        ("THREAD_SUSPEND_RESUME", THREAD_SUSPEND_RESUME),
        ("THREAD_TERMINATE", THREAD_TERMINATE),
        ("THREAD_SET_INFORMATION", THREAD_SET_INFORMATION),
        ("THREAD_SET_THREAD_TOKEN", THREAD_SET_THREAD_TOKEN),
        ("THREAD_IMPERSONATE", THREAD_IMPERSONATE),
        ("THREAD_DIRECT_IMPERSONATION", THREAD_DIRECT_IMPERSONATION),
        ("WRITE_DAC", WRITE_DAC),
        ("WRITE_OWNER", WRITE_OWNER),
    ];
    for tid in &threads {
        // SAFETY: plain call; a returned handle is closed by `all_refused`.
        match all_refused(&attempts, |access| unsafe { OpenThread(access, 0, *tid) })? {
            None => {}
            Some(name) => return Ok((false, format!("opened thread {tid} for {name}"))),
        }
    }
    Ok((true, format!("refused on {} threads", threads.len())))
}

/// The runner's process and token refuse every right but waiting and
/// reading.
fn check_runner_process() -> std::io::Result<(bool, String)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::{
        TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_GROUPS, TOKEN_ADJUST_PRIVILEGES, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    };
    use windows_sys::Win32::Storage::FileSystem::{WRITE_DAC, WRITE_OWNER};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_CREATE_THREAD, PROCESS_DUP_HANDLE,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME,
        PROCESS_TERMINATE, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
    };
    let runner = runner_pid()?;
    let attempts = [
        ("PROCESS_VM_WRITE", PROCESS_VM_WRITE),
        ("PROCESS_VM_READ", PROCESS_VM_READ),
        ("PROCESS_VM_OPERATION", PROCESS_VM_OPERATION),
        ("PROCESS_CREATE_THREAD", PROCESS_CREATE_THREAD),
        ("PROCESS_DUP_HANDLE", PROCESS_DUP_HANDLE),
        ("PROCESS_SET_INFORMATION", PROCESS_SET_INFORMATION),
        ("PROCESS_SUSPEND_RESUME", PROCESS_SUSPEND_RESUME),
        ("PROCESS_TERMINATE", PROCESS_TERMINATE),
        ("WRITE_DAC", WRITE_DAC),
        ("WRITE_OWNER", WRITE_OWNER),
    ];
    // SAFETY: plain call; a returned handle is closed by `all_refused`.
    if let Some(name) = all_refused(&attempts, |access| unsafe {
        OpenProcess(access, 0, runner)
    })? {
        return Ok((false, format!("opened the runner for {name}")));
    }
    // Waiting and reading basic information stay allowed, and are the way
    // to the token.
    // SAFETY: plain call; owned below.
    let process = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, runner) })?;
    let token_attempts = [
        ("TOKEN_DUPLICATE", TOKEN_DUPLICATE),
        ("TOKEN_IMPERSONATE", TOKEN_IMPERSONATE),
        ("TOKEN_ASSIGN_PRIMARY", TOKEN_ASSIGN_PRIMARY),
        ("TOKEN_ADJUST_DEFAULT", TOKEN_ADJUST_DEFAULT),
        ("TOKEN_ADJUST_GROUPS", TOKEN_ADJUST_GROUPS),
        ("TOKEN_ADJUST_PRIVILEGES", TOKEN_ADJUST_PRIVILEGES),
        ("WRITE_DAC", WRITE_DAC),
    ];
    let opened = all_refused(&token_attempts, |access| {
        let mut raw: HANDLE = std::ptr::null_mut();
        // SAFETY: a valid process handle; `raw` is written on success.
        if unsafe { OpenProcessToken(process.as_raw_handle(), access, &mut raw) } == 0 {
            return std::ptr::null_mut();
        }
        raw
    })?;
    if let Some(name) = opened {
        return Ok((false, format!("opened the runner's token for {name}")));
    }
    Ok((true, "refused".into()))
}

/// No console host belongs to the runner: the runner is a windows-subsystem
/// program, so Windows makes none.
fn check_runner_console() -> std::io::Result<(bool, String)> {
    let runner = runner_pid()?;
    let hosts: Vec<u32> = processes()?
        .into_iter()
        .filter(|p| p.parent == runner && p.image.eq_ignore_ascii_case("conhost.exe"))
        .map(|p| p.pid)
        .collect();
    if hosts.is_empty() {
        Ok((true, "no console host".into()))
    } else {
        Ok((
            false,
            format!("console host {hosts:?} belongs to the runner"),
        ))
    }
}

/// The command can still open its own token for reading, as many programs
/// do at start (the MSYS2 runtime among them). The runner changes its own
/// default access list after it makes the command's token, so this holds.
fn check_own_token() -> std::io::Result<(bool, String)> {
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut raw: HANDLE = std::ptr::null_mut();
    // SAFETY: opens this process's own token for query.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        let e = std::io::Error::last_os_error();
        return Ok((false, format!("own token refused: {e}")));
    }
    drop(owned(raw)?);
    Ok((true, "own token opened".into()))
}

/// Copy standard input to standard output: the runner passes both through
/// although it has no console.
fn check_stdio() -> std::io::Result<(bool, String)> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok((true, format!("echo {}", text.trim())))
}

/// The in-sandbox half. A no-op unless `KGRAF_RUNNER_PROBE` is set.
#[test]
fn probe_entry() {
    let Ok(action) = std::env::var("KGRAF_RUNNER_PROBE") else {
        return;
    };
    let result = match action.as_str() {
        "threads" => check_runner_threads(),
        "process" => check_runner_process(),
        "console" => check_runner_console(),
        "own_token" => check_own_token(),
        "stdio" => check_stdio(),
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

/// Whether commands run as the sandbox user here; prints why a test skips
/// when they do not. With `KGRAF_REQUIRE_SANDBOX_USER=1` a machine that does
/// not run them that way fails instead (`common::sandbox_user_mode`).
fn sandbox_user_mode(test: &str) -> bool {
    let on = common::sandbox_user_mode();
    if !on {
        let backend = platform_isolation().expect("a backend").backend;
        println!("{test}: skipped, the backend is {backend:?}, not the sandbox user");
    }
    on
}

fn self_exe() -> PathBuf {
    std::env::current_exe().unwrap()
}

fn policy(ws: &Workspace) -> SandboxPolicy {
    SandboxPolicy::for_workspace(ws, Network::Deny).accept_partial_isolation(true)
}

fn run_probe(ws: &Workspace, cwd: &Path, action: &str, input: Option<&str>) -> Output {
    let mut cmd = sandboxed_command(&policy(ws), self_exe(), cwd).unwrap();
    cmd.args([
        "--exact",
        "probe_entry",
        "--nocapture",
        "--test-threads=1",
        "-q",
    ])
    .env("KGRAF_RUNNER_PROBE", action)
    .stdout(Redirect::Piped)
    .stderr(Redirect::Piped);
    match input {
        Some(_) => cmd.stdin(Redirect::Piped),
        None => cmd.stdin(Redirect::Null),
    };
    let mut child = cmd.spawn().unwrap();
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn probe(action: &str, input: Option<&str>) -> (i32, String, Output) {
    let e = env();
    let out = run_probe(&e.ws, e.ws.root(), action, input);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("PROBE="))
        .unwrap_or_else(|| panic!("no probe line: {out:?}"))
        .to_string();
    (out.status.code().unwrap_or(-1), line, out)
}

fn assert_holds(action: &str, input: Option<&str>) -> String {
    let (code, line, out) = probe(action, input);
    assert_eq!(
        code,
        HOLDS,
        "{action}: {line}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    line
}

#[test]
fn a_command_cannot_open_any_runner_thread_to_run_code_on_it() {
    if sandbox_user_mode("threads") {
        assert_holds("threads", None);
    }
}

#[test]
fn a_command_cannot_open_the_runner_process_or_token_beyond_reading() {
    if sandbox_user_mode("process") {
        assert_holds("process", None);
    }
}

#[test]
fn the_runner_has_no_console_host() {
    if sandbox_user_mode("console") {
        assert_holds("console", None);
    }
}

#[test]
fn a_command_can_still_open_its_own_token() {
    if sandbox_user_mode("own_token") {
        assert_holds("own_token", None);
    }
}

/// kGraf writes the lists of the runner's process and main thread while the
/// runner is suspended and reads them back. An `Err` here is the measured
/// Win32 error of that write on this machine: the launch then fell back to
/// the runner protecting itself, a gap the backend's gaps text states.
#[test]
fn the_runners_lists_are_written_before_it_runs_and_read_back() {
    if !sandbox_user_mode("pre_resume") {
        return;
    }
    let e = env();
    let mut cmd = sandboxed_command(&policy(&e.ws), "cmd.exe", e.ws.root()).unwrap();
    cmd.args(["/d", "/c", "exit", "0"])
        .stdin(Redirect::Null)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped);
    let child = cmd.spawn().unwrap();
    let outcome = child
        .runner_lists_before_resume()
        .map(|r| r.map_err(str::to_owned));
    let out = child.wait_with_output().unwrap();
    println!("runner lists before resume: {outcome:?}");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(outcome, Some(Ok(())));
}

#[test]
fn standard_input_and_output_pass_through_a_runner_without_a_console() {
    if sandbox_user_mode("stdio") {
        let line = assert_holds("stdio", Some("through the runner"));
        assert_eq!(line, "echo through the runner");
    }
}
