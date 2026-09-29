//! The regression this fixes: a
//! `static`'s `Drop` never runs at normal process exit, so a writer kept only
//! in one never got to send its buffered `Stop` or drain the channel before
//! the process ended. Of 20 short-lived runs that each recorded one error,
//! only 18 lines made it to disk. This spawns the real `diag-smoke` binary
//! (a real process boundary, not a thread inside this test binary) enough
//! times to reproduce that shape, and asserts every record survives.

use std::path::PathBuf;
use std::process::Command;

/// `label` disambiguates the two tests in this file, which otherwise share
/// `std::process::id()` (the test *binary's* pid, not a per-test one: both
/// `#[test]`s run as threads of the same process) and would collide on the
/// same path if run in parallel, which is the default. A whole directory per
/// label, not just a differently-named file directly in the shared system
/// temp root: `diag-smoke` also scrubs (`scrub::install_key`), whose key file
/// is always named `diagnostics.key` next to whatever path it is given, so
/// two tests (here, or in a sibling test binary running at the same time)
/// that put their own diagnostics file straight into the shared temp root
/// would collide on that shared key file even with distinct file names.
fn smoke_path(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kgraf-diag-exit-flush-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("diagnostics.jsonl")
}

fn run_smoke(path: &std::path::Path, count: usize) {
    let status = Command::new(env!("CARGO_BIN_EXE_diag-smoke"))
        .env("KGRAF_DIAG_PATH", path)
        .env("KGRAF_DIAG_SMOKE_COUNT", count.to_string())
        .status()
        .expect("diag-smoke must run");
    assert!(status.success(), "diag-smoke exited with {status:?}");
}

#[test]
fn twenty_short_lived_runs_each_recording_one_error_lose_nothing() {
    let path = smoke_path("many-runs");
    let _ = std::fs::remove_file(&path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    const RUNS: usize = 20;
    for _ in 0..RUNS {
        run_smoke(&path, 1);
    }

    let records = kgraf_diag::store::read_all(&path).unwrap();
    let error_count = records
        .iter()
        .filter(|r| matches!(r.event, kgraf_diag::DiagEvent::Error { .. }))
        .count();
    assert_eq!(
        error_count, RUNS,
        "expected all {RUNS} short-lived processes' single record each to survive; \
         got {error_count} of {records:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_single_process_recording_many_events_and_exiting_at_once_loses_nothing() {
    let path = smoke_path("one-run-many-events");
    let _ = std::fs::remove_file(&path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    const COUNT: usize = 500;
    run_smoke(&path, COUNT);

    let records = kgraf_diag::store::read_all(&path).unwrap();
    let error_count = records
        .iter()
        .filter(|r| matches!(r.event, kgraf_diag::DiagEvent::Error { .. }))
        .count();
    assert_eq!(
        error_count, COUNT,
        "expected every one of {COUNT} records recorded \
        right before the process exited to survive; got {error_count}"
    );
    let _ = std::fs::remove_file(&path);
}
