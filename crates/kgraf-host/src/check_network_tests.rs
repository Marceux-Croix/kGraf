//! A check or gate runs with the network open, and says so, on a backend
//! that cannot deny the network (Windows; decision 0011, amended 2026-10-06).
//! These tests stand in for that backend with a [`CheckNetwork`] value, so
//! they run on macOS and Linux; nothing here ran on Windows.
//!
//! How to run: `cargo test -p kgraf-host check_network`. The one test that
//! connects to a local socket runs on macOS only, where the sandbox denies
//! the network.

use super::*;
use kgraf_sandbox::{CheckNetwork, NETWORK_OPEN_NOTICE};

/// The notice line a check ends with when its network was open.
fn says_open(text: &str) -> bool {
    text.ends_with(&format!("\n[sandbox] {NETWORK_OPEN_NOTICE}"))
}

/// Under a backend that cannot deny the network, the check runs (it is not
/// refused) and its output ends with the notice, whether it passed, failed
/// or was killed at its timeout. Under one that can, nothing changes: no
/// notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_on_a_backend_that_cannot_deny_the_network_runs_and_says_so() {
    let r = repo();
    let c = named("b.txt", "w-net");
    let ws_path = prepared(&r, &c, "w-net").await;
    let live = r.workspaces.get(&ws_path).expect("live");
    let open = CheckNetwork::open_with_notice();
    let run = |net: CheckNetwork, command: &str, secs: u64| {
        crate::gates::run_confined_on(
            net,
            false,
            &live,
            &r.root,
            &[],
            command,
            Duration::from_secs(secs),
            2_000,
            None,
        )
        .expect("the check ran")
    };
    let (ok, text) = run(open, "echo from-the-check", 30);
    assert!(ok, "{text}");
    assert!(text.contains("from-the-check"), "{text}");
    assert!(says_open(&text), "a pass says it: {text}");
    let (ok, text) = run(open, "echo bad >&2; exit 3", 30);
    assert!(!ok, "{text}");
    assert!(says_open(&text), "a failure says it: {text}");
    let (ok, text) = run(open, "sleep 30", 1);
    assert!(!ok, "{text}");
    assert!(text.contains("[killed after"), "{text}");
    assert!(says_open(&text), "a kill says it: {text}");
    // A backend that can deny: the same check, no notice.
    let (ok, text) = run(CheckNetwork::deny(), "echo from-the-check", 30);
    assert!(ok, "{text}");
    assert!(!text.contains("network not blocked"), "{text}");
}

/// The notice is not a label on a setting: with the network open a check
/// reaches a local socket, and with it denied the same check does not.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_open_network_decision_changes_what_the_check_can_reach() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let r = repo();
    let c = named("b.txt", "w-net-reach");
    let ws_path = prepared(&r, &c, "w-net-reach").await;
    let live = r.workspaces.get(&ws_path).expect("live");
    let probe = format!("/bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'");
    let run = |net: CheckNetwork| {
        crate::gates::run_confined_on(
            net,
            false,
            &live,
            &r.root,
            &[],
            &probe,
            Duration::from_secs(30),
            2_000,
            None,
        )
        .expect("the check ran")
    };
    let (reached, text) = run(CheckNetwork::open_with_notice());
    assert!(reached, "the open network reaches a local socket: {text}");
    let (reached, text) = run(CheckNetwork::deny());
    assert!(!reached, "the denied network does not: {text}");
}

/// A check that started from no warm build folder says so when it fails, and
/// only then: a pass ends with no build note.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_check_that_built_cold_says_so_and_a_passing_one_does_not() {
    let r = repo();
    let c = named("b.txt", "w-cold");
    let ws_path = prepared(&r, &c, "w-cold").await;
    let live = r.workspaces.get(&ws_path).expect("live");
    let missing = r.root.join("no-such-warm-folder");
    let run = |command: &str| {
        crate::gates::run_confined_on(
            CheckNetwork::deny(),
            false,
            &live,
            &r.root,
            &[],
            command,
            Duration::from_secs(30),
            2_000,
            Some(&missing),
        )
        .expect("the check ran")
    };
    let (ok, text) = run("echo fine");
    assert!(ok, "{text}");
    assert!(!text.contains("[build]"), "{text}");
    let (ok, text) = run("exit 3");
    assert!(!ok, "{text}");
    assert!(text.contains("[build] the check built cold"), "{text}");
    assert!(text.contains("no-such-warm-folder"), "{text}");
}

/// A check that prints rustc's named-mutex abort and fails gets the sandbox
/// line that says why, unless the active launch path makes named objects work
/// (`Active::cargo_runs`): then the failure has another cause and the line
/// would name the wrong one. The check only prints the text rustc prints on
/// Windows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_check_gets_the_named_object_line_only_where_cargo_cannot_run() {
    let r = repo();
    let c = named("b.txt", "w-named-gate");
    let ws_path = prepared(&r, &c, "w-named-gate").await;
    let live = r.workspaces.get(&ws_path).expect("live");
    let command = "echo 'failed to create global mutex named `rustc_errors`' >&2; exit 101";
    let run = |cargo_runs: bool| {
        crate::gates::run_confined_on(
            CheckNetwork::deny(),
            cargo_runs,
            &live,
            &r.root,
            &[],
            command,
            Duration::from_secs(30),
            2_000,
            None,
        )
        .expect("the check ran")
    };
    let (ok, text) = run(false);
    assert!(!ok, "{text}");
    assert!(text.contains("[sandbox] rustc could not make"), "{text}");
    let (ok, text) = run(true);
    assert!(!ok, "{text}");
    assert!(!text.contains("rustc could not make"), "{text}");
    assert!(text.contains("rustc_errors"), "the output stays: {text}");
}
