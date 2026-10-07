//! `sh` and `bash` through Git Bash on the real Windows backend.
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for the Windows targets only. Run it on
//! Windows 10 or later, as a normal (non-elevated) user, with Git for Windows
//! installed:
//! `cargo test -p kgraf-sandbox --test shell_windows -- --test-threads=1`.
//! Single-threaded because one test sets a process environment variable.
//!
//! What it shows: a POSIX command line with quotes, spaces, backslashes, a
//! glob and a newline reaches the shell byte for byte (the arguments travel
//! in the environment, never through a command line MSYS2 would re-parse);
//! the exit status comes back; the `PATH` Git Bash sets up finds its own
//! tools; and a Git Bash that cannot be found stops the command with a
//! message that says how to fix it.

#![cfg(windows)]

mod common;

use common::fixture;
use kgraf_sandbox::{
    Network, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec, sandboxed_command,
};

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
    SandboxPolicy::for_workspace(ws, Network::Allow).accept_partial_isolation(true)
}

fn run(ws: &Workspace, program: &str, args: &[&str]) -> std::process::Output {
    sandboxed_command(&policy(ws), program, ws.root())
        .unwrap()
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("`{program}` did not run: {e}"))
}

#[test]
fn sh_dash_c_gets_every_argument_byte_for_byte() {
    let e = env();
    std::fs::write(e.ws.root().join("a.rs"), "").unwrap();
    std::fs::write(e.ws.root().join("b.rs"), "").unwrap();
    let script = r#"printf '<%s>' "$0"; for a; do printf '[%s]' "$a"; done; echo; echo *.rs"#;
    let out = run(
        &e.ws,
        "sh",
        &[
            "-c",
            script,
            "zero",
            "a b",
            "it's",
            "*.rs",
            r"C:\dir\",
            r#""q""#,
            "{x,y}",
            "@file",
            "two\nlines",
            "",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        stdout.replace("\r\n", "\n"),
        "<zero>[a b][it's][*.rs][C:\\dir\\][\"q\"][{x,y}][@file][two\nlines][]\na.rs b.rs\n",
        "{out:?}"
    );
}

#[test]
fn bash_returns_the_exit_status_and_finds_its_own_tools() {
    let e = env();
    let out = run(
        &e.ws,
        "bash",
        &["-c", "ls >/dev/null && grep --version >/dev/null && exit 7"],
    );
    assert_eq!(out.status.code(), Some(7), "{out:?}");
}

#[test]
fn the_carrier_variable_is_gone_inside_the_command() {
    let e = env();
    let out = run(
        &e.ws,
        "sh",
        &["-c", r#"printf '%s' "${KGRAF_SHELL_COMMAND-unset}""#],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "unset", "{out:?}");
}

/// A named Git Bash that is not there stops the command, and the error says
/// what to set; nothing else is tried.
#[test]
fn a_missing_git_bash_stops_the_command_and_says_how_to_fix_it() {
    let e = env();
    // SAFETY: this file runs single-threaded (see the module docs), so no
    // other thread reads the environment while it changes.
    unsafe { std::env::set_var("KGRAF_GIT_BASH_PATH", r"C:\no\such\bash.exe") };
    let result = sandboxed_command(&policy(&e.ws), "sh", e.ws.root())
        .unwrap()
        .args(["-c", "echo ran"])
        .output();
    // SAFETY: as above.
    unsafe { std::env::remove_var("KGRAF_GIT_BASH_PATH") };
    let err = result.expect_err("a missing Git Bash must not run anything");
    let text = err.to_string();
    assert!(
        text.contains("KGRAF_GIT_BASH_PATH") && text.contains("not a file"),
        "{text}"
    );
}
