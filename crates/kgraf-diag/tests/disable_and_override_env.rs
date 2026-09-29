//! `KGRAF_DIAG_PATH`/`KGRAF_DIAG_DISABLE`: every
//! test that runs a real kGraf binary must be able to keep it off the
//! owner's actual per-user diagnostics file. This also covers the follow-on
//! bug the fix surfaced: `scrub::install_key` read the real per-user path
//! unconditionally (`default_path`, not `resolve_path`), so a process with
//! diagnostics disabled still silently created a real `diagnostics.key`
//! there the first time it scrubbed any text.

use std::path::PathBuf;
use std::process::Command;

/// A fresh temp directory per test: `key_path` is always
/// `diagnostics.key` next to whatever `diagnostics.jsonl`-like name is
/// chosen, with no test-specific component in it, so two tests sharing one
/// directory (both use `std::env::temp_dir()`, and the test binary's `pid` is
/// identical for every `#[test]` running as its threads) would collide on
/// the same key path even with distinctly named diagnostics files.
fn scratch_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kgraf-diag-env-test-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_smoke(env: &[(&str, &str)], count: usize) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_diag-smoke"));
    cmd.env("KGRAF_DIAG_SMOKE_COUNT", count.to_string());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd.status().expect("diag-smoke must run");
    assert!(status.success());
}

#[test]
fn disabled_diagnostics_writes_no_file_at_the_redirected_path_or_its_key() {
    let dir = scratch_dir("disable");
    let path = dir.join("diagnostics.jsonl");
    let key_path = dir.join("diagnostics.key");

    run_smoke(
        &[
            ("KGRAF_DIAG_PATH", path.to_str().unwrap()),
            ("KGRAF_DIAG_DISABLE", "1"),
        ],
        5,
    );

    assert!(
        !path.exists(),
        "KGRAF_DIAG_PATH must not be used once KGRAF_DIAG_DISABLE is set"
    );
    assert!(
        !key_path.exists(),
        "a disabled run must not create a key file either"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn path_env_redirects_both_the_diagnostics_file_and_its_key() {
    let dir = scratch_dir("override");
    let path = dir.join("diagnostics.jsonl");
    let key_path = dir.join("diagnostics.key");

    run_smoke(&[("KGRAF_DIAG_PATH", path.to_str().unwrap())], 3);

    assert!(path.exists(), "the overridden path must be used");
    assert!(
        key_path.exists(),
        "the key must live alongside the overridden path, not the real default one"
    );
    let records = kgraf_diag::store::read_all(&path).unwrap();
    assert_eq!(records.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}
