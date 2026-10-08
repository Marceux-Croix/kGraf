//! Programs start inside the Windows sandbox, and a cleared environment
//! passes only what a program needs.
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for the Windows targets only. Run it on
//! Windows 10 or later, as a normal (non-elevated) user, with Git for Windows
//! installed:
//! `cargo test -p kgraf-sandbox --test startup_windows -- --test-threads=1`.
//! Single-threaded because the environment tests set process variables.
//!
//! What it shows:
//!
//! - A program that loads `user32.dll` at start (`whoami.exe`, `where.exe`,
//!   PowerShell) runs: the private desktop admits the command's token
//!   through the desktop's own SID. Before that SID, each of them exited
//!   with STATUS_DLL_INIT_FAILED (0xC0000142) and no output, while
//!   `cmd.exe`, which does not load `user32.dll`, ran.
//! - `sh -c 'echo ok'` and `bash -c 'exit 3'` through Git Bash. These are
//!   expected to FAIL until decision 0011 settles how MSYS2 programs run:
//!   the MSYS2 runtime opens its own signal pipe for write with an ACL that
//!   names only the user, Administrators and SYSTEM, which the
//!   write-restricted token cannot pass. The failure then shows the
//!   runtime's own words on stderr ("couldn't create signal pipe, Win32
//!   error 5"). The cause is pinned by the `msys_pipe` unit tests in
//!   `src/sandbox/windows.rs`.
//! - After `env_clear`, a credential-shaped and a plain variable of the
//!   parent are gone, the base variables (`SystemRoot`, `PATH`) are there,
//!   and the sandbox's git and TLS settings and its `TMP` survive. Without
//!   `env_clear`, the scrub still removes the credential-shaped name.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Output;

use kgraf_sandbox::{Network, SandboxPolicy, sandboxed_command};

/// A writable folder and a scratch folder (the policy's tmp folder).
struct Dirs {
    _tmp: tempfile::TempDir,
    folder: PathBuf,
    scratch: PathBuf,
}

fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("box");
    let scratch = tmp.path().join("tmp");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    Dirs {
        _tmp: tmp,
        folder,
        scratch,
    }
}

fn policy(d: &Dirs) -> SandboxPolicy {
    SandboxPolicy::new(vec![d.folder.clone(), d.scratch.clone()], Network::Allow)
        .accept_partial_isolation(true)
        .with_tmp_dir(d.scratch.clone())
}

/// Exit code and both streams, for failure messages.
fn said(out: &Output) -> String {
    format!(
        "exit {:?} (0xC0000142 = {}), stdout: {}, stderr: {}",
        out.status.code(),
        0xC000_0142_u32 as i32,
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

fn run(d: &Dirs, program: &str, args: &[&str], clear: bool) -> Output {
    let mut cmd = sandboxed_command(&policy(d), program, &d.folder).unwrap();
    if clear {
        cmd.env_clear();
    }
    cmd.args(args)
        .output()
        .unwrap_or_else(|e| panic!("`{program}` did not run: {e}"))
}

/// `NAME=value` lines of `cmd /d /c set`, names compared without case.
fn value_of<'a>(listing: &'a str, name: &str) -> Option<&'a str> {
    listing.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        k.eq_ignore_ascii_case(name).then_some(v.trim_end())
    })
}

#[test]
fn programs_that_load_user32_start_in_the_sandbox() {
    let d = dirs();
    let whoami = run(&d, "whoami.exe", &[], false);
    assert!(whoami.status.success(), "whoami: {}", said(&whoami));
    assert!(!whoami.stdout.is_empty(), "whoami: {}", said(&whoami));
    let where_cmd = run(&d, "where.exe", &["cmd"], false);
    assert!(where_cmd.status.success(), "where: {}", said(&where_cmd));
}

#[test]
fn powershell_starts_and_returns_its_exit_status() {
    let d = dirs();
    let out = run(
        &d,
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", "exit 5"],
        false,
    );
    assert_eq!(out.status.code(), Some(5), "{}", said(&out));
}

/// The control: `cmd.exe` does not load `user32.dll` and ran before the
/// desktop fix too.
#[test]
fn cmd_runs_and_returns_its_exit_status() {
    let d = dirs();
    let out = run(&d, "cmd", &["/d", "/c", "echo ok& exit /b 4"], false);
    assert_eq!(out.status.code(), Some(4), "{}", said(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
}

/// Expected to fail until decision 0011 settles how MSYS2 programs run (see
/// the module docs).
#[test]
fn sh_dash_c_echo_ok_prints_ok() {
    let d = dirs();
    let out = run(&d, "sh", &["-c", "echo ok"], false);
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "ok",
        "{}",
        said(&out)
    );
}

/// Expected to fail until decision 0011 settles how MSYS2 programs run (see
/// the module docs).
#[test]
fn bash_dash_c_exit_3_returns_3() {
    let d = dirs();
    let out = run(&d, "bash", &["-c", "exit 3"], false);
    assert_eq!(out.status.code(), Some(3), "{}", said(&out));
}

#[test]
fn a_cleared_environment_keeps_the_base_and_the_sandbox_settings_only() {
    // SAFETY: this file runs single-threaded (see the module docs), so no
    // other thread reads the environment while it changes.
    unsafe {
        std::env::set_var("KGRAF_STARTUP_TEST_API_KEY", "sk-should-not-leak");
        std::env::set_var("KGRAF_STARTUP_TEST_PLAIN", "visible");
    }
    let d = dirs();
    let out = run(&d, "cmd", &["/d", "/c", "set"], true);
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("KGRAF_STARTUP_TEST_API_KEY");
        std::env::remove_var("KGRAF_STARTUP_TEST_PLAIN");
    }
    assert!(out.status.success(), "{}", said(&out));
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        value_of(&listing, "KGRAF_STARTUP_TEST_API_KEY"),
        None,
        "{listing}"
    );
    assert_eq!(
        value_of(&listing, "KGRAF_STARTUP_TEST_PLAIN"),
        None,
        "{listing}"
    );
    assert!(value_of(&listing, "SystemRoot").is_some(), "{listing}");
    assert!(value_of(&listing, "PATH").is_some(), "{listing}");
    assert_eq!(
        value_of(&listing, "CURL_SSL_BACKEND"),
        Some("openssl"),
        "{listing}"
    );
    assert_eq!(
        value_of(&listing, "GIT_CONFIG_KEY_0"),
        Some("credential.helper"),
        "{listing}"
    );
    assert_eq!(
        value_of(&listing, "GIT_CONFIG_COUNT"),
        Some("5"),
        "{listing}"
    );
    let tmp = value_of(&listing, "TMP").map(Path::new);
    assert_eq!(tmp, Some(d.scratch.as_path()), "{listing}");
    for line in listing.lines() {
        let Some((name, _)) = line.split_once('=') else {
            continue;
        };
        let known = kgraf_sandbox::WINDOWS_BASE_VARS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(name))
            || [
                "TMP",
                "TEMP",
                "GIT_CONFIG_COUNT",
                "CURL_SSL_BACKEND",
                "PROMPT",
            ]
            .iter()
            .any(|b| b.eq_ignore_ascii_case(name))
            || name.starts_with("GIT_CONFIG_KEY_")
            || name.starts_with("GIT_CONFIG_VALUE_");
        assert!(known, "unexpected variable {name} in {listing}");
    }
}

/// Without a clear the command inherits the environment, minus the names
/// the scrub judges to hold a credential.
#[test]
fn without_a_clear_the_scrub_still_removes_a_credential_name() {
    // SAFETY: this file runs single-threaded (see the module docs).
    unsafe {
        std::env::set_var("KGRAF_STARTUP_TEST_API_KEY", "sk-should-not-leak");
        std::env::set_var("KGRAF_STARTUP_TEST_PLAIN", "visible");
    }
    let d = dirs();
    let out = run(&d, "cmd", &["/d", "/c", "set"], false);
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("KGRAF_STARTUP_TEST_API_KEY");
        std::env::remove_var("KGRAF_STARTUP_TEST_PLAIN");
    }
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        value_of(&listing, "KGRAF_STARTUP_TEST_API_KEY"),
        None,
        "{listing}"
    );
    assert_eq!(
        value_of(&listing, "KGRAF_STARTUP_TEST_PLAIN"),
        Some("visible"),
        "{listing}"
    );
}
