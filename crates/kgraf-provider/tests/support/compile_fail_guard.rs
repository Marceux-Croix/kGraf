//! Shared runner for the trybuild compile-fail suites.
//!
//! Each suite includes this file with `#[path = ...] mod compile_fail_guard;`.
//! `env!` resolves in the including crate, so one copy serves every suite.
//!
//! Why this exists. trybuild builds the fixtures with its own `cargo` call and reads
//! only the compiler diagnostics from it. It ignores that call's exit status and
//! stderr. A fixture with no diagnostic is reported as "succeeded", which is also what
//! it reports when cargo never reached the fixture: a dependency or the crate under
//! test failed to build, a rustc process was killed, or the machine was out of memory
//! or process slots. The guard stays closed in that case (the suite fails), but the
//! failure then reads "Expected test case to fail to compile, but it succeeded", which
//! looks like a lost guarantee and hides the real cause.
//!
//! This runner keeps trybuild as the judge and adds a diagnosis on failure. It re-runs
//! `cargo check` over the generated project and reports whether each fixture was
//! rejected with a compiler error, plus the machine load and swap use at that moment.
//! A rerun that rejects every fixture while trybuild saw none means trybuild's own
//! build was disturbed, not that a fixture compiles. A rerun that does not reject a
//! fixture means the guarantee is really lost.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::PathBuf;
use std::process::Command;

/// Run `trybuild`'s compile-fail check over `pattern` (relative to the crate root).
/// On failure, print the diagnosis and then re-raise trybuild's own panic.
pub fn run(pattern: &str) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let t = trybuild::TestCases::new();
        t.compile_fail(pattern);
    }));
    if let Err(panic) = outcome {
        eprintln!("{}", diagnose(pattern));
        resume_unwind(panic);
    }
}

/// The workspace target directory, found from the running test binary
/// (`<target>/<profile>/deps/<name>-<hash>`), which holds under `--target-dir` too.
fn target_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().nth(3).map(PathBuf::from)
}

fn fixture_names(pattern: &str) -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        std::path::Path::new(pattern)
            .parent()
            .unwrap_or(std::path::Path::new(".")),
    );
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

fn command_output(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        Err(e) => format!("({program} unavailable: {e})"),
    }
}

fn diagnose(pattern: &str) -> String {
    let mut report = String::from("\n=== compile-fail diagnosis ===\n");
    let Some(target) = target_dir() else {
        report.push_str("cannot locate the target directory; no rerun\n");
        return report;
    };
    let trybuild_target = target.join("tests").join("trybuild");
    let project = trybuild_target.join(env!("CARGO_PKG_NAME"));
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(&cargo)
        .current_dir(&project)
        .env("CARGO_TARGET_DIR", &trybuild_target)
        .env("CARGO_INCREMENTAL", "0")
        .args(["check", "--offline", "--bins", "--keep-going"])
        .args(["--message-format=short", "--color=never"])
        .output();
    match out {
        Err(e) => report.push_str(&format!("cargo rerun could not start: {e}\n")),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let fixtures = fixture_names(pattern);
            report.push_str(&format!("cargo rerun exit status: {}\n", o.status));
            let mut all_rejected = !fixtures.is_empty();
            for name in &fixtures {
                let rejected = stderr
                    .lines()
                    .any(|l| l.contains(name.as_str()) && l.contains("error"));
                all_rejected &= rejected;
                report.push_str(&format!(
                    "  fixture {name}: {}\n",
                    if rejected {
                        "rejected by the compiler (expected)"
                    } else {
                        "NOT rejected: the guarantee may be lost, or the build never reached it"
                    }
                ));
            }
            report.push_str(if all_rejected {
                "verdict: every fixture is rejected on a rerun, so trybuild's own build was \
                 disturbed (see load and swap below); the guarantee is intact, rerun to confirm\n"
            } else {
                "verdict: a fixture is not rejected on a rerun; treat the guarantee as lost \
                 until the stderr below says otherwise\n"
            });
            report.push_str("cargo rerun stderr, last 40 lines:\n");
            let lines: Vec<&str> = stderr.lines().collect();
            for l in &lines[lines.len().saturating_sub(40)..] {
                report.push_str(&format!("  | {l}\n"));
            }
        }
    }
    report.push_str(&format!(
        "load: {}\nswap: {}\n",
        command_output("uptime", &[]),
        command_output("sysctl", &["-n", "vm.swapusage"]),
    ));
    report.push_str("==============================\n");
    report
}
