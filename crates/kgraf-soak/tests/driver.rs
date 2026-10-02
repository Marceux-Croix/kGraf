//! The driver end to end: it runs real orchestrations against its own stub server, and what it prints
//! is what the canary's judge reads.
//!
//! Replicate by hand: `kgraf-soak --repo DIR --work DIR --runs 2`, then read the JSON lines.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::Command;

fn run_driver(runs: &str) -> (std::process::ExitStatus, Vec<serde_json::Value>, String) {
    let dir = tempfile::tempdir().expect("dir");
    let out = Command::new(env!("CARGO_BIN_EXE_kgraf-soak"))
        .arg("--repo")
        .arg(dir.path().join("repo"))
        .arg("--work")
        .arg(dir.path().join("work"))
        .args(["--runs", runs])
        .output()
        .expect("run the driver");
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect();
    (
        out.status,
        lines,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn each_run_completes_and_leaves_the_host_empty() {
    let (status, lines, stderr) = run_driver("2");
    assert!(status.success(), "{stderr}");
    assert_eq!(lines.len(), 3, "a baseline line and one per run");
    assert_eq!(lines[0]["run"], 0);
    assert_eq!(lines[0]["status"], "baseline");
    for (i, line) in lines.iter().enumerate().skip(1) {
        assert_eq!(line["run"], i as u64);
        assert_eq!(line["status"], "Completed", "{line}");
        assert_eq!(line["verified"], 2, "two contracts, both verified: {line}");
        for table in [
            "ended",
            "refetchers",
            "start_consents",
            "dependency_questions",
            "fetch_grants",
            "notices",
        ] {
            assert_eq!(line["retained"][table], 0, "run {i}: {table} in {line}");
        }
        // The stub server is a child process the driver does not count; nothing else is.
        assert_eq!(line["children"], 0, "{line}");
        assert_eq!(line["git_worktrees"], 0, "{line}");
        assert!(line["mem_bytes"].as_u64().unwrap_or(0) > 0, "{line}");
        assert!(line["fds"].as_u64().unwrap_or(0) >= 3, "{line}");
        assert!(line["threads"].as_u64().unwrap_or(0) >= 1, "{line}");
    }
}

#[test]
fn a_missing_argument_is_refused_with_a_reason() {
    let out = Command::new(env!("CARGO_BIN_EXE_kgraf-soak"))
        .args(["--runs", "1"])
        .output()
        .expect("run the driver");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--repo DIR is required"));
}
