//! The report: one Markdown file with a summary table first, the details
//! after, and the same data as JSON in a fenced block at the end.

use serde::Serialize;

use crate::e2e::{Check, Status};
use crate::facts::{MachineFacts, Resources};
use crate::libtest::{Outcome, TestOutcome};
use crate::manifest::{NotShipped, Set};
use crate::workdir::{Cleanup, Traces};

pub const SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Failure {
    pub test: String,
    /// `kgraf_defect_suspected`, or the missing tool the output names.
    pub class: String,
    pub tail: String,
    /// Set when `tail` is the output of a one-test re-run after the binary
    /// hung or crashed: `passed`, `failed` or `hung`. The test's outcome in
    /// the first run is unchanged.
    pub solo: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BinaryResult {
    pub id: String,
    pub set: Set,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub start_error: Option<String>,
    pub secs: f64,
    pub expected_secs: f64,
    pub listed: Option<u64>,
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub tests: Vec<TestOutcome>,
    pub failures: Vec<Failure>,
    /// The test that was running when the binary hung or crashed.
    pub open_test: Option<String>,
    /// The open test run alone again, when the binary hung or crashed.
    pub open_test_solo: Option<Failure>,
    /// Failed tests not re-run alone because the per-binary cap was reached.
    pub rerun_skipped: Vec<String>,
    /// Output tail when the binary did not end with a test summary.
    pub tail: Option<String>,
    pub skipped: Vec<String>,
}

impl BinaryResult {
    /// A binary that ran every test it started to a result and exited 0.
    pub fn clean(&self) -> bool {
        self.failed == 0
            && !self.timed_out
            && !self.cancelled
            && self.start_error.is_none()
            && self.exit_code == Some(0)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StepTime {
    pub step: String,
    pub expected_secs: f64,
    pub actual_secs: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub schema: u32,
    pub runner_version: String,
    pub kgraf_version: String,
    pub git_commit: String,
    pub built_utc: String,
    pub started_utc: String,
    pub finished_utc: String,
    pub mode: String,
    pub interrupted: bool,
    pub machine: MachineFacts,
    pub resources_start: Resources,
    pub resources_end: Resources,
    pub binaries: Vec<BinaryResult>,
    pub e2e: Vec<Check>,
    pub not_shipped: Vec<NotShipped>,
    /// Steps of docs/WINDOWS-TESTING.md that stay manual, with the reason.
    pub manual: Vec<String>,
    pub network_isolation_experiment: String,
    pub cleanup: Cleanup,
    pub traces_found: Traces,
    pub bundle_removal: String,
    pub step_times: Vec<StepTime>,
    pub total_secs: f64,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub binaries: usize,
    pub binaries_clean: usize,
    pub passed: u64,
    pub failed: u64,
    pub failed_needs_tool: u64,
    pub ignored: u64,
    pub hung_or_crashed: usize,
    pub e2e_pass: usize,
    pub e2e_fail: usize,
    pub e2e_skipped: usize,
}

fn solo_note(outcome: &str) -> String {
    format!(
        "Tail from a solo re-run (`--exact`, one test); the solo re-run {outcome}. \
         The outcome above is from the first run.\n\n"
    )
}

/// Failures whose output names a program that is not on this machine are
/// counted apart: they say what the machine lacks, not what kGraf does.
pub fn classify_failure(tail: &str) -> String {
    let lower = tail.to_lowercase();
    // `program not found` is std's wording; the sandbox's spawn error names the
    // program between the words (`program cargo not found`), matched per tool below.
    let not_found = [
        "program not found",
        "not found",
        "cannot find the file specified",
        "os error 2",
        "is not recognized as an internal or external command",
        "no such file or directory",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if not_found {
        for tool in [
            "cargo", "rustc", "rustup", "python", "node", "npm", "pnpm", "docker", "uv", "go",
            "java", "gradle",
        ] {
            let quoted = [
                format!("`{tool}`"),
                format!("\"{tool}\""),
                format!("{tool}.exe"),
                format!("'{tool}'"),
                format!("program {tool} not found"),
            ];
            if quoted.iter().any(|q| lower.contains(q.as_str())) {
                return format!("needs `{tool}`, not on this machine");
            }
        }
    }
    "kgraf_defect_suspected".to_owned()
}

pub fn totals(r: &Report) -> Totals {
    let mut t = Totals {
        binaries: r.binaries.len(),
        ..Totals::default()
    };
    for b in &r.binaries {
        t.passed += b.passed;
        t.ignored += b.ignored;
        t.binaries_clean += usize::from(b.clean());
        let needs = b
            .failures
            .iter()
            .filter(|f| f.class != "kgraf_defect_suspected")
            .count() as u64;
        t.failed_needs_tool += needs.min(b.failed);
        t.failed += b.failed - needs.min(b.failed);
        if b.timed_out
            || b.start_error.is_some()
            || (b.exit_code != Some(0) && b.failed == 0 && !b.cancelled)
        {
            t.hung_or_crashed += 1;
        }
    }
    for c in &r.e2e {
        match c.status {
            Status::Pass => t.e2e_pass += 1,
            Status::Fail => t.e2e_fail += 1,
            Status::Skipped => t.e2e_skipped += 1,
        }
    }
    t
}

/// The verdict line: what the reader should look at first.
pub fn verdict(t: &Totals, r: &Report) -> String {
    if r.interrupted {
        "INTERRUPTED: the run was stopped before the end; results below are partial".into()
    } else if t.failed == 0 && t.hung_or_crashed == 0 && t.e2e_fail == 0 {
        "ALL PASSED (failures that need a missing tool are listed apart)".into()
    } else {
        format!(
            "FAILURES: {} test(s) failed, {} binary run(s) hung or crashed, {} end-to-end check(s) failed",
            t.failed, t.hung_or_crashed, t.e2e_fail
        )
    }
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "unknown".into())
}

/// A fenced block that cannot be closed early by its own content.
fn fence(lang: &str, body: &str) -> String {
    let mut ticks = "```".to_owned();
    while body.contains(&ticks) {
        ticks.push('`');
    }
    format!("{ticks}{lang}\n{body}\n{ticks}\n")
}

pub fn render(r: &Report) -> String {
    let t = totals(r);
    let mut md = String::new();
    let m = &r.machine;
    md.push_str("# kGraf Windows self-test report\n\n");
    md.push_str(&format!("**{}**\n\n", verdict(&t, r)));
    md.push_str("| Item | Result |\n|---|---|\n");
    let rows = [
        ("kGraf", format!("{} ({})", r.kgraf_version, r.git_commit)),
        (
            "Windows",
            format!(
                "{} {} build {}",
                opt(&m.windows),
                opt(&m.display_version),
                opt(&m.build)
            ),
        ),
        (
            "Machine",
            format!(
                "{} native, tests built for {}, emulated: {}",
                opt(&m.native_arch),
                m.binary_arch,
                m.emulated.map_or("unknown".into(), |e| e.to_string())
            ),
        ),
        ("Mode", r.mode.clone()),
        (
            "Test binaries",
            format!(
                "{} run, {} clean, {} hung or crashed",
                t.binaries, t.binaries_clean, t.hung_or_crashed
            ),
        ),
        (
            "Tests",
            format!(
                "{} passed, {} failed, {} failed for a missing tool, {} ignored",
                t.passed, t.failed, t.failed_needs_tool, t.ignored
            ),
        ),
        (
            "End-to-end checks",
            format!(
                "{} passed, {} failed, {} skipped",
                t.e2e_pass, t.e2e_fail, t.e2e_skipped
            ),
        ),
        (
            "Network isolation experiment",
            r.network_isolation_experiment.clone(),
        ),
        (
            "Cleanup",
            if r.cleanup.verified {
                "work folder removed and verified".to_owned()
            } else {
                format!("NOT verified: {}", r.cleanup.left.join("; "))
            },
        ),
        ("Total time", crate::progress::clock(r.total_secs)),
    ];
    for (k, v) in rows {
        md.push_str(&format!("| {k} | {} |\n", md_escape(&v)));
    }

    md.push_str("\n## Failures\n\n");
    let mut any = false;
    for b in &r.binaries {
        for f in &b.failures {
            any = true;
            md.push_str(&format!(
                "### {} :: {}\n\nClass: {}\n\n",
                b.id, f.test, f.class
            ));
            if let Some(s) = &f.solo {
                md.push_str(&solo_note(s));
            }
            md.push_str(&fence("text", &f.tail));
            md.push('\n');
        }
        if let Some(f) = &b.open_test_solo {
            any = true;
            md.push_str(&format!(
                "### {} :: {} (running when the binary ended)\n\n",
                b.id, f.test
            ));
            if let Some(s) = &f.solo {
                md.push_str(&solo_note(s));
            }
            md.push_str(&fence("text", &f.tail));
            md.push('\n');
        }
        if !b.rerun_skipped.is_empty() {
            any = true;
            md.push_str(&format!(
                "### {}: not re-run alone (cap reached)\n\n{}\n\n",
                b.id,
                b.rerun_skipped
                    .iter()
                    .map(|t| format!("- {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !b.clean() && b.failures.is_empty() {
            any = true;
            md.push_str(&format!(
                "### {} (exit {:?}, timed out {}, start error {:?}, running test {:?})\n\n",
                b.id, b.exit_code, b.timed_out, b.start_error, b.open_test
            ));
            md.push_str(&fence("text", b.tail.as_deref().unwrap_or("")));
            md.push('\n');
        }
    }
    for c in r.e2e.iter().filter(|c| c.status == Status::Fail) {
        any = true;
        md.push_str(&format!(
            "### e2e {}: {}\n\n{}\n\n",
            c.id, c.title, c.detail
        ));
        md.push_str(&fence("text", &c.evidence));
        md.push('\n');
    }
    if !any {
        md.push_str("None.\n");
    }

    md.push_str("\n## Test binaries\n\n| Binary | Set | Passed | Failed | Ignored | Exit | Seconds (expected) |\n|---|---|---|---|---|---|---|\n");
    for b in &r.binaries {
        md.push_str(&format!(
            "| {} | {:?} | {} | {} | {} | {} | {:.0} ({:.0}) |\n",
            md_escape(&b.id),
            b.set,
            b.passed,
            b.failed,
            b.ignored,
            if b.timed_out {
                "timed out".to_owned()
            } else {
                format!("{:?}", b.exit_code)
            },
            b.secs,
            b.expected_secs
        ));
    }

    md.push_str(
        "\n## End-to-end checks\n\n| Check | Status | Simulated | Detail |\n|---|---|---|---|\n",
    );
    for c in &r.e2e {
        md.push_str(&format!(
            "| {} {} | {:?} | {} | {} |\n",
            c.id,
            md_escape(&c.title),
            c.status,
            if c.simulated { "SIMULATED" } else { "live" },
            md_escape(&c.detail)
        ));
    }

    md.push_str("\n## Machine\n\n");
    md.push_str(&fence(
        "json",
        &serde_json::to_string_pretty(&r.machine).unwrap_or_default(),
    ));
    md.push_str(&format!(
        "\nResources at start: {} MiB free of {} MiB memory, {} MiB free disk ({}).\n",
        r.resources_start.free_memory_mib,
        r.resources_start.total_memory_mib,
        r.resources_start
            .free_disk_mib
            .map_or("unknown".into(), |d| d.to_string()),
        r.resources_start.disk.clone().unwrap_or_default()
    ));
    md.push_str(&format!(
        "Resources at end: {} MiB free memory, {} MiB free disk.\n",
        r.resources_end.free_memory_mib,
        r.resources_end
            .free_disk_mib
            .map_or("unknown".into(), |d| d.to_string()),
    ));

    md.push_str("\n## Not run\n\n");
    for n in &r.not_shipped {
        md.push_str(&format!("- {}: {}\n", n.what, n.reason));
    }
    for b in &r.binaries {
        for s in &b.skipped {
            md.push_str(&format!("- {} :: {}\n", b.id, s));
        }
    }
    md.push_str("\n## Stays manual\n\n");
    for s in &r.manual {
        md.push_str(&format!("- {s}\n"));
    }

    md.push_str("\n## Cleanup\n\n");
    for c in &r.cleanup.created {
        md.push_str(&format!(
            "- created: {}\n",
            serde_json::to_string(c).unwrap_or_default()
        ));
    }
    for c in &r.cleanup.removed {
        md.push_str(&format!("- removed: {c}\n"));
    }
    for c in &r.cleanup.left {
        md.push_str(&format!("- STILL THERE: {c}\n"));
    }
    for c in &r.cleanup.notes {
        md.push_str(&format!("- note: {c}\n"));
    }
    let tr = &r.traces_found;
    if tr.profile_files.is_empty() && tr.run_values.is_empty() && tr.kgraf_app_data.is_empty() {
        md.push_str("- outside the work folder: nothing new found (profile, HKCU Run key, kGraf app data)\n");
    } else {
        md.push_str(&format!(
            "- FOUND outside the work folder (removed where this run made it): profile files {:?}, Run values {:?}, kGraf app data {:?}\n",
            tr.profile_files, tr.run_values, tr.kgraf_app_data
        ));
    }
    md.push_str(&format!("- bundle folder: {}\n", r.bundle_removal));

    md.push_str("\n## Step times (expected vs actual, for tuning the estimate)\n\n| Step | Expected s | Actual s |\n|---|---|---|\n");
    for s in &r.step_times {
        md.push_str(&format!(
            "| {} | {:.0} | {:.0} |\n",
            md_escape(&s.step),
            s.expected_secs,
            s.actual_secs
        ));
    }
    if !r.notes.is_empty() {
        md.push_str("\n## Notes\n\n");
        for n in &r.notes {
            md.push_str(&format!("- {n}\n"));
        }
    }
    md.push_str("\n## Machine-readable data\n\n");
    md.push_str(&fence(
        "json",
        &serde_json::to_string_pretty(r).unwrap_or_default(),
    ));
    md
}

/// Read back the JSON block of a rendered report (used by the tests, and by
/// whoever analyzes a report).
pub fn embedded_json(md: &str) -> Option<serde_json::Value> {
    let start = md.rfind("## Machine-readable data")?;
    let rest = &md[start..];
    let open = rest.find("```json\n")? + "```json\n".len();
    let body = &rest[open..];
    let close = body.rfind("\n```")?;
    serde_json::from_str(&body[..close]).ok()
}

/// Outcome counts for one binary from its parsed tests.
pub fn count(tests: &[TestOutcome], o: Outcome) -> u64 {
    tests.iter().filter(|t| t.outcome == o).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(id: &str, failed: &[(&str, &str)]) -> BinaryResult {
        BinaryResult {
            id: id.into(),
            set: Set::Core,
            exit_code: Some(if failed.is_empty() { 0 } else { 101 }),
            timed_out: false,
            cancelled: false,
            start_error: None,
            secs: 3.0,
            expected_secs: 2.0,
            listed: Some(3),
            passed: 3 - failed.len() as u64,
            failed: failed.len() as u64,
            ignored: 0,
            tests: vec![],
            failures: failed
                .iter()
                .map(|(t, tail)| Failure {
                    test: (*t).into(),
                    class: classify_failure(tail),
                    tail: (*tail).into(),
                    solo: None,
                })
                .collect(),
            open_test: None,
            open_test_solo: None,
            rerun_skipped: vec![],
            tail: None,
            skipped: vec![],
        }
    }

    #[test]
    fn a_missing_tool_is_counted_apart_from_a_kgraf_failure() {
        let mut r = Report::default();
        r.binaries.push(bin("a", &[]));
        r.binaries.push(bin(
            "b",
            &[
                (
                    "t1",
                    "called `Result::unwrap()` on an `Err` value: program not found: `cargo`",
                ),
                ("t2", "assertion `left == right` failed"),
                (
                    "t3",
                    "check: cannot run `cargo` in C:\\t\n\nCaused by:\n    0: spawn failed: program cargo not found\n    1: program cargo not found",
                ),
            ],
        ));
        let t = totals(&r);
        assert_eq!(t.passed, 3);
        assert_eq!(t.failed, 1);
        assert_eq!(t.failed_needs_tool, 2);
        assert_eq!(t.binaries_clean, 1);
        assert!(verdict(&t, &r).starts_with("FAILURES: 1 test(s)"));
    }

    #[test]
    fn a_hang_counts_and_an_all_clean_run_says_so() {
        let mut r = Report::default();
        r.binaries.push(bin("a", &[]));
        assert_eq!(
            verdict(&totals(&r), &r),
            "ALL PASSED (failures that need a missing tool are listed apart)"
        );
        let mut hung = bin("h", &[]);
        hung.timed_out = true;
        hung.exit_code = None;
        r.binaries.push(hung);
        assert_eq!(totals(&r).hung_or_crashed, 1);
    }

    #[test]
    fn the_report_starts_with_the_summary_and_ends_with_its_json() {
        let mut r = Report {
            kgraf_version: "0.7.8".into(),
            network_isolation_experiment: "not included".into(),
            ..Report::default()
        };
        r.binaries.push(bin("x", &[("t", "boom ``` inside")]));
        let md = render(&r);
        let summary = md.find("| Tests |").expect("summary row");
        let failures = md.find("## Failures").expect("failures");
        assert!(summary < failures);
        assert!(md.contains("| Network isolation experiment | not included |"));
        let json = embedded_json(&md).expect("embedded json parses");
        assert_eq!(json["kgraf_version"], "0.7.8");
        assert_eq!(
            json["binaries"][0]["failures"][0]["tail"],
            "boom ``` inside"
        );
    }
}
