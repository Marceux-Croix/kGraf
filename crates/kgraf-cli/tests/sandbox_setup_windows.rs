//! The Windows sandbox user setup end to end (decision 0097): `kgraf sandbox
//! setup`, `status`, a second `setup`, `remove`, `status`, with a test tag so
//! the users and rules have names of their own.
//!
//! It creates local accounts and firewall rules, so it runs only when the test
//! process is already elevated (as on a hosted Windows runner) and
//! `KGRAF_SANDBOX_SETUP_TEST=1` is set; otherwise it prints why and returns.
//! An elevated process starts the elevated step without a UAC prompt.

#![cfg(windows)]

use std::process::{Command, Output};

/// High mandatory integrity level: the process runs elevated.
const HIGH_INTEGRITY: &str = "S-1-16-12288";

fn elevated() -> bool {
    Command::new("whoami")
        .arg("/groups")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(HIGH_INTEGRITY))
}

fn kgraf(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .args(args)
        .output()
        .expect("run kgraf")
}

fn text(o: &Output) -> String {
    format!(
        "exit {:?}\n{}{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn setup_status_again_remove_leave_nothing_behind() {
    if std::env::var("KGRAF_SANDBOX_SETUP_TEST").as_deref() != Ok("1") {
        eprintln!("skipped: set KGRAF_SANDBOX_SETUP_TEST=1 to create test sandbox users");
        return;
    }
    if !elevated() {
        eprintln!("skipped: the setup test needs an elevated test process");
        return;
    }
    let tag = format!("t{:x}", std::process::id() % 0xffff);
    let t = ["--tag", tag.as_str()];

    let before = kgraf(&[&["sandbox", "status"][..], &t].concat());
    assert!(
        !before.status.success(),
        "nothing set up yet: {}",
        text(&before)
    );

    let setup = kgraf(&[&["sandbox", "setup"][..], &t].concat());
    let removed_anyway = || kgraf(&[&["sandbox", "remove"][..], &t].concat());
    if !setup.status.success() {
        let r = removed_anyway();
        panic!("setup failed:\n{}\nremove:\n{}", text(&setup), text(&r));
    }
    let out = text(&setup);
    assert!(out.contains("created"), "{out}");

    let status = kgraf(&[&["sandbox", "status"][..], &t].concat());
    let st = text(&status);
    // A DNS filter Windows refused at setup fails status with a FAILED line
    // and says so; that gap is reported, not hidden, and is not what this
    // test checks. Any other failing line fails the test.
    if !status.status.success() && !only_refused_dns_filters(&st) {
        let r = removed_anyway();
        panic!("status after setup:\n{st}\nremove:\n{}", text(&r));
    }
    if !status.status.success() {
        eprintln!("DNS is not confined on this machine (status says why):\n{st}");
    }
    assert!(st.contains("set up"), "{st}");

    let again = kgraf(&[&["sandbox", "setup"][..], &t].concat());
    let ag = text(&again);
    assert!(again.status.success(), "{ag}");
    assert!(ag.contains("updated"), "a second setup refreshes: {ag}");

    let remove = removed_anyway();
    assert!(remove.status.success(), "{}", text(&remove));

    let after = kgraf(&[&["sandbox", "status"][..], &t].concat());
    let af = text(&after);
    assert!(!after.status.success(), "everything removed: {af}");
    // A failing status says only that something is missing. Nothing setup
    // made may still be present: a removal that dropped record lines for
    // users it did not read once left every slot user behind while this
    // check passed.
    let left = left_behind(&af);
    assert!(left.is_empty(), "left behind after remove: {left:?}\n{af}");
}

/// Whether a failing status fails only for DNS filters Windows refused at
/// setup: its closing sentence says so, and every line that is not `present`
/// or `unknown` is a `FAILED` DNS filter line.
fn only_refused_dns_filters(status: &str) -> bool {
    status.contains("running setup again does not change that.")
        && status
            .lines()
            .map(str::trim)
            .filter(|l| {
                ["MISSING", "WRONG", "FAILED"]
                    .iter()
                    .any(|w| l.starts_with(w))
            })
            .all(|l| l.starts_with("FAILED") && l.contains("DNS filter"))
}

/// The `present` lines of a status report that name something setup makes,
/// not a fact of the machine's firewall.
fn left_behind(status: &str) -> Vec<&str> {
    const MACHINE_FACTS: [&str; 2] = [
        "Windows Firewall on for every profile",
        "local firewall rules take effect",
    ];
    status
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("present"))
        .map(str::trim)
        .filter(|item| !MACHINE_FACTS.contains(item))
        .collect()
}
