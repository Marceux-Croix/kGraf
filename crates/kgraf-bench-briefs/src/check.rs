//! Running a [`Check`](crate::brief::Check) against a checked-out attempt.
//!
//! `Ok(false)` is a legitimate, expected outcome: the brief was not
//! satisfied (a test failed, a script printed the wrong thing, a doc file is
//! missing a required fact). `Err` is reserved for the check itself being
//! unable to run (`cargo` or `sh` could not be spawned, a path could not be
//! read), a harness problem, not a measurement.

use crate::brief::{Check, ScriptCase};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Run `check` against `dir` (the attempt's checked-out worktree). See the
/// module docs for what `Ok(false)` versus `Err` means.
pub fn run(check: &Check, dir: &Path) -> Result<bool> {
    match check {
        Check::CargoTest => cargo_test(dir, &[]),
        Check::CargoTestNamed {
            test_bin,
            test_name,
        } => cargo_test(dir, &["--test", test_bin, test_name, "--", "--exact"]),
        Check::Script { script, cases } => script_cases(dir, script, cases),
        Check::Docs {
            file,
            required,
            max_words,
        } => docs(dir, file, required, *max_words),
    }
}

fn cargo_test(dir: &Path, extra_args: &[&str]) -> Result<bool> {
    let manifest = dir.join("Cargo.toml");
    if !manifest.is_file() {
        // The worker never created the crate at all: unsatisfied, not a
        // harness error (a weak model can simply do nothing).
        return Ok(false);
    }
    let output = Command::new("cargo")
        .arg("test")
        .arg("--manifest-path")
        .arg(&manifest)
        .args(extra_args)
        // Never reuse the workspace's own target/ or lock a shared registry
        // cache write against a concurrent bench attempt.
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .current_dir(dir)
        .output()
        .with_context(|| format!("cannot run `cargo test` in {}", dir.display()))?;
    Ok(output.status.success())
}

fn script_cases(dir: &Path, script: &str, cases: &[ScriptCase]) -> Result<bool> {
    let path = dir.join(script);
    if !path.is_file() {
        return Ok(false);
    }
    for case in cases {
        let output = Command::new("sh")
            .arg(script)
            .args(case.args)
            .current_dir(dir)
            .output()
            .with_context(|| format!("cannot run `sh {script}` in {}", dir.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let status = output.status.code().unwrap_or(-1);
        if stdout != case.expect_stdout || status != case.expect_status {
            return Ok(false);
        }
    }
    Ok(true)
}

fn docs(dir: &Path, file: &str, required: &[&str], max_words: usize) -> Result<bool> {
    let path = dir.join(file);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    if text.split_whitespace().count() > max_words {
        return Ok(false);
    }
    Ok(required.iter().all(|fact| text.contains(fact)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_check_wants_every_fact_and_the_word_cap() {
        let dir = tempfile::tempdir().expect("dir");
        let file = "NOTES.md";
        // Missing file.
        assert!(!docs(dir.path(), file, &["x"], 10).expect("check"));
        std::fs::write(dir.path().join(file), "one two x").expect("write");
        assert!(docs(dir.path(), file, &["x"], 10).expect("check"));
        assert!(!docs(dir.path(), file, &["missing"], 10).expect("check"));
        assert!(!docs(dir.path(), file, &["x"], 2).expect("check: over cap"));
    }

    #[test]
    fn script_check_compares_stdout_and_status_exactly() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("r.sh"), "#!/bin/sh\necho hi\nexit 3\n").expect("write");
        let ok = script_cases(
            dir.path(),
            "r.sh",
            &[ScriptCase {
                args: &[],
                expect_stdout: "hi\n",
                expect_status: 3,
            }],
        )
        .expect("check");
        assert!(ok);
        let wrong = script_cases(
            dir.path(),
            "r.sh",
            &[ScriptCase {
                args: &[],
                expect_stdout: "hi\n",
                expect_status: 0,
            }],
        )
        .expect("check");
        assert!(!wrong);
    }

    #[test]
    fn a_missing_manifest_or_script_is_unsatisfied_not_an_error() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(!cargo_test(dir.path(), &[]).expect("check"));
        assert!(
            !script_cases(dir.path(), "nope.sh", &[]).expect("check"),
            "no cases still needs the file to exist"
        );
    }
}
