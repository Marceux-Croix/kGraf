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
    /// The run's time budget ended while this binary ran, and it was
    /// stopped. `timed_out` stays false: its own limit was not the cause.
    pub budget_stopped: bool,
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
    /// Every line the binary printed that starts with `measurement:`.
    pub measurements: Vec<Measurement>,
}

/// One `measurement:` line of a test binary's output.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Measurement {
    /// The test that printed it, when the output says which.
    pub test: Option<String>,
    /// The test's time as the runner measured it: from the result of the
    /// test before it (or the binary's start) to its own result.
    pub secs: Option<f64>,
    pub line: String,
}

impl BinaryResult {
    /// A binary that ran every test it started to a result and exited 0.
    pub fn clean(&self) -> bool {
        self.failed == 0
            && !self.timed_out
            && !self.cancelled
            && !self.budget_stopped
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
    /// The sandbox user phase, when the run asked for it.
    pub sandbox_user: Option<crate::sandbox_phase::SandboxPhase>,
    /// The `--only` filter the run had, if any: only what it named ran.
    pub only: Option<String>,
    /// What the time budget did to the baseline, when the run had one.
    pub budget: Option<crate::budget::BudgetReport>,
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

/// One binary's counts: failed tests that count, failed tests that need a
/// missing tool, and whether the binary hung or crashed.
fn tally(b: &BinaryResult) -> (u64, u64, bool) {
    let needs = b
        .failures
        .iter()
        .filter(|f| f.class != "kgraf_defect_suspected")
        .count() as u64;
    let needs = needs.min(b.failed);
    let hung = b.timed_out
        || b.start_error.is_some()
        || (b.exit_code != Some(0) && b.failed == 0 && !b.cancelled && !b.budget_stopped);
    (b.failed - needs, needs, hung)
}

/// The baseline's counts: its test binaries and its end-to-end checks.
pub fn totals(r: &Report) -> Totals {
    let mut t = Totals {
        binaries: r.binaries.len(),
        ..Totals::default()
    };
    for b in &r.binaries {
        t.passed += b.passed;
        t.ignored += b.ignored;
        t.binaries_clean += usize::from(b.clean());
        let (failed, needs, hung) = tally(b);
        t.failed_needs_tool += needs;
        t.failed += failed;
        t.hung_or_crashed += usize::from(hung);
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

/// What the verdict counts, over both halves of a run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headline {
    /// Failed tests: the baseline's, less those expected under the token
    /// sandbox, plus every failure of the sandbox user re-run.
    pub failed: u64,
    /// Hung or crashed binary runs, of the baseline and of the re-run.
    pub hung_or_crashed: usize,
    /// Failed end-to-end checks, of the baseline and of the sandbox user.
    pub e2e_fail: usize,
    /// Baseline failures expected under the token sandbox, listed apart
    /// ([`crate::sandbox_phase::expected_under_token_sandbox`]), and
    /// baseline end-to-end checks by the same rule
    /// ([`crate::sandbox_phase::e2e_expected_under_token_sandbox`]).
    pub expected: usize,
    /// Known gaps the run measured, each named on a line of its own.
    pub known_gaps: Vec<String>,
}

/// The counts behind the verdict. A baseline failure, or a baseline
/// end-to-end check, leaves the count only when the sandbox user passed the
/// same test or check and its baseline output names a token sandbox limit.
/// Of the re-run, only the DNS test's failure leaves it, and only when it is
/// the DNS gap the gate measured and the headline names as a known gap
/// ([`crate::sandbox_phase::is_recorded_dns_gap`]); every other failure of
/// the re-run counts.
pub fn headline(r: &Report) -> Headline {
    let t = totals(r);
    let mut h = Headline {
        failed: t.failed,
        hung_or_crashed: t.hung_or_crashed,
        e2e_fail: t.e2e_fail,
        ..Headline::default()
    };
    if let Some(p) = &r.sandbox_user {
        let tests =
            crate::sandbox_phase::expected_under_token_sandbox(&r.binaries, &p.reruns).len();
        let checks = crate::sandbox_phase::e2e_expected_under_token_sandbox(&r.e2e, &p.e2e).len();
        h.expected = tests + checks;
        h.failed = h.failed.saturating_sub(tests as u64);
        h.e2e_fail = h.e2e_fail.saturating_sub(checks);
        let mut gap_tests = Vec::new();
        for rr in &p.reruns {
            let (failed, _, hung) = tally(&rr.result);
            let gap: Vec<&Failure> = rr
                .result
                .failures
                .iter()
                .filter(|f| crate::sandbox_phase::is_recorded_dns_gap(p, &rr.result.id, f))
                .collect();
            h.failed += failed.saturating_sub(gap.len() as u64);
            gap_tests.extend(
                gap.iter()
                    .map(|f| format!("{} :: {}", rr.result.id, f.test)),
            );
            h.hung_or_crashed += usize::from(hung);
        }
        h.e2e_fail += p.e2e.iter().filter(|c| c.status == Status::Fail).count();
        if let Some(dns) = &p.dns_failed {
            let tests = if gap_tests.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} fails as the sandbox user while the gap stands and is listed under \
                     Known gaps, not counted",
                    gap_tests.join(", ")
                )
            };
            h.known_gaps.push(format!(
                "DNS is not confined on this machine: name lookups by the sandbox users without \
                 the network can leave it ({dns}){tests}"
            ));
        }
    }
    h
}

/// What the time budget cut, over both halves of a run: the items that did
/// not start and the items it stopped.
pub fn budget_cuts(r: &Report) -> (Vec<crate::budget::NotRun>, Vec<String>) {
    let mut not_run = Vec::new();
    let mut stopped = Vec::new();
    if let Some(b) = &r.budget {
        not_run.extend(b.not_run.iter().cloned());
        stopped.extend(b.stopped.iter().cloned());
    }
    if let Some(p) = &r.sandbox_user {
        not_run.extend(p.not_run.iter().cloned());
        stopped.extend(p.stopped_at_budget.iter().cloned());
        if let Some(why) = &p.skipped
            && why.starts_with("budget")
        {
            not_run.push(crate::budget::NotRun {
                what: "sandbox user phase".into(),
                why: format!("not run: {why}"),
            });
        }
    }
    (not_run, stopped)
}

/// The verdict line: what the reader should look at first.
pub fn verdict(r: &Report) -> String {
    let h = headline(r);
    let apart = if h.expected > 0 {
        format!(
            "; {} baseline failure(s) expected under the token sandbox are listed apart",
            h.expected
        )
    } else {
        String::new()
    };
    let (not_run, stopped) = budget_cuts(r);
    if r.interrupted {
        "INTERRUPTED: the run was stopped before the end; results below are partial".into()
    } else if !not_run.is_empty() || !stopped.is_empty() {
        // Never ALL PASSED: what did not run proves nothing.
        format!(
            "INCOMPLETE: the time budget of {} min ran out; {} item(s) did not run and {} \
             item(s) were stopped at the budget. Of what ran: {} test(s) failed, {} binary \
             run(s) hung or crashed, {} end-to-end check(s) failed{apart}",
            r.budget.as_ref().map_or(0, |b| b.mins),
            not_run.len(),
            stopped.len(),
            h.failed,
            h.hung_or_crashed,
            h.e2e_fail
        )
    } else if h.failed == 0 && h.hung_or_crashed == 0 && h.e2e_fail == 0 {
        format!("ALL PASSED (failures that need a missing tool are listed apart{apart})")
    } else {
        format!(
            "FAILURES: {} test(s) failed, {} binary run(s) hung or crashed, {} end-to-end check(s) failed{apart}",
            h.failed, h.hung_or_crashed, h.e2e_fail
        )
    }
}

pub(crate) fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "unknown".into())
}

/// A fenced block that cannot be closed early by its own content.
pub(crate) fn fence(lang: &str, body: &str) -> String {
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
    if let Some(only) = &r.only {
        md.push_str(&format!(
            "**Filter: `--only {only}`. Only the named test binaries, tests and end-to-end \
             checks ran, in the suite and in the sandbox user re-run. This is not a full \
             run.**\n\n"
        ));
    }
    let h = headline(r);
    md.push_str(&format!("**{}**\n\n", verdict(r)));
    for gap in &h.known_gaps {
        md.push_str(&format!("**Known gap: {gap}**\n\n"));
    }
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
        (
            "Mode",
            match &r.only {
                Some(only) => format!("{}, filtered: --only {only}", r.mode),
                None => r.mode.clone(),
            },
        ),
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
    let mut rows = rows.to_vec();
    if let Some(p) = &r.sandbox_user {
        rows.insert(7, ("Sandbox user phase", p.summary()));
    }
    if let Some(b) = &r.budget {
        let (not_run, stopped) = budget_cuts(r);
        rows.push((
            "Time budget",
            if not_run.is_empty() && stopped.is_empty() {
                format!("{} min, not reached", b.mins)
            } else {
                format!(
                    "{} min, REACHED: {} item(s) not run, {} stopped (see Not run)",
                    b.mins,
                    not_run.len(),
                    stopped.len()
                )
            },
        ));
    }
    for (k, v) in rows {
        md.push_str(&format!("| {k} | {} |\n", md_escape(&v)));
    }

    let expected = r.sandbox_user.as_ref().map_or_else(Vec::new, |p| {
        crate::sandbox_phase::expected_under_token_sandbox(&r.binaries, &p.reruns)
    });
    let expected_checks = r.sandbox_user.as_ref().map_or_else(Vec::new, |p| {
        crate::sandbox_phase::e2e_expected_under_token_sandbox(&r.e2e, &p.e2e)
    });
    md.push_str("\n## Failures\n\n");
    let mut any = false;
    for b in &r.binaries {
        for f in &b.failures {
            if expected
                .iter()
                .any(|e| e.binary == b.id && e.test == f.test)
            {
                continue;
            }
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
        // A test listed apart as expected needs no solo re-run: the sandbox
        // user's run of it is the evidence. Only the counted ones are named.
        let skipped: Vec<&String> = b
            .rerun_skipped
            .iter()
            .filter(|t| !expected.iter().any(|e| e.binary == b.id && &e.test == *t))
            .collect();
        if !skipped.is_empty() {
            any = true;
            md.push_str(&format!(
                "### {}: not re-run alone (cap reached)\n\n{}\n\n",
                b.id,
                skipped
                    .iter()
                    .map(|t| format!("- {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !b.clean() && b.failures.is_empty() {
            any = true;
            md.push_str(&format!(
                "### {} (exit {:?}, timed out {}, stopped at the budget {}, start error {:?}, \
                 running test {:?})\n\n",
                b.id, b.exit_code, b.timed_out, b.budget_stopped, b.start_error, b.open_test
            ));
            md.push_str(&fence("text", b.tail.as_deref().unwrap_or("")));
            md.push('\n');
        }
    }
    for c in r
        .e2e
        .iter()
        .filter(|c| c.status == Status::Fail)
        .filter(|c| !expected_checks.iter().any(|e| e.id == c.id))
    {
        any = true;
        md.push_str(&format!(
            "### e2e {}: {}\n\n{}\n\n",
            c.id, c.title, c.detail
        ));
        md.push_str(&fence("text", &c.evidence));
        md.push('\n');
    }
    // The sandbox user re-run's failures count in the verdict, so each is
    // here with its output, whether or not the baseline failed it too.
    if let Some(p) = &r.sandbox_user {
        for b in p.reruns.iter().map(|rr| &rr.result) {
            for f in &b.failures {
                if crate::sandbox_phase::is_recorded_dns_gap(p, &b.id, f) {
                    continue;
                }
                any = true;
                md.push_str(&format!(
                    "### sandbox user: {} :: {}\n\nClass: {}\n\n",
                    b.id, f.test, f.class
                ));
                if let Some(s) = &f.solo {
                    md.push_str(&solo_note(s));
                }
                md.push_str(&fence("text", &f.tail));
                md.push('\n');
            }
            if tally(b).2 {
                any = true;
                md.push_str(&format!(
                    "### sandbox user: {} (exit {:?}, timed out {}, stopped at the budget {}, \
                     start error {:?}, running test {:?})\n\n",
                    b.id, b.exit_code, b.timed_out, b.budget_stopped, b.start_error, b.open_test
                ));
                let tail = b
                    .open_test_solo
                    .as_ref()
                    .map(|f| f.tail.as_str())
                    .or(b.tail.as_deref())
                    .unwrap_or("");
                md.push_str(&fence("text", tail));
                md.push('\n');
            }
        }
        for c in p.e2e.iter().filter(|c| c.status == Status::Fail) {
            any = true;
            md.push_str(&format!(
                "### sandbox user e2e {}: {}\n\n{}\n\n",
                c.id, c.title, c.detail
            ));
            md.push_str(&fence("text", &c.evidence));
            md.push('\n');
        }
    }
    if !any {
        md.push_str("None.\n");
    }

    if let Some(p) = &r.sandbox_user {
        let gap: Vec<(&str, &Failure)> = p
            .reruns
            .iter()
            .flat_map(|rr| {
                rr.result
                    .failures
                    .iter()
                    .map(|f| (rr.result.id.as_str(), f))
            })
            .filter(|(id, f)| crate::sandbox_phase::is_recorded_dns_gap(p, id, f))
            .collect();
        if !gap.is_empty() {
            md.push_str("\n## Known gaps\n\n");
            md.push_str(
                "Sandbox user failures not counted in the verdict: each is the gap the headline \
                 names, which the selfcheck gate measured on this machine, and its own output \
                 says so. A leak with every filter in place would be counted above.\n\n",
            );
            for (id, f) in gap {
                md.push_str(&format!("### sandbox user: {id} :: {}\n\n", f.test));
                md.push_str(&fence("text", &f.tail));
                md.push('\n');
            }
        }
    }

    if r.sandbox_user.is_some() {
        md.push_str(&format!(
            "\n## Expected under the token sandbox ({})\n\n",
            expected.len() + expected_checks.len()
        ));
        md.push_str(
            "Baseline failures not counted in the verdict: each test or end-to-end check passed \
             when it ran again as the sandbox user, and its baseline output names a limit of the \
             token sandbox that the sandbox user lifts. Any other baseline failure is counted \
             above.\n\n",
        );
        if expected.is_empty() && expected_checks.is_empty() {
            md.push_str("None.\n");
        } else {
            md.push_str(
                "| Binary | Test | Baseline output names | Sandbox user |\n|---|---|---|---|\n",
            );
            for e in &expected {
                md.push_str(&format!(
                    "| {} | {} | {} | passed |\n",
                    md_escape(&e.binary),
                    md_escape(&e.test),
                    md_escape(&e.marker)
                ));
            }
            for e in &expected_checks {
                md.push_str(&format!(
                    "| end-to-end check | {} | {} | passed |\n",
                    md_escape(&e.id),
                    md_escape(&e.marker)
                ));
            }
        }
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
            if b.budget_stopped {
                "stopped at the budget".to_owned()
            } else if b.timed_out {
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

    if let Some(p) = &r.sandbox_user {
        md.push_str(&crate::sandbox_phase::render(&r.binaries, &r.e2e, p));
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
    let (cut_not_run, cut_stopped) = budget_cuts(r);
    for n in &cut_not_run {
        md.push_str(&format!("- {}: {}\n", n.what, n.why));
    }
    for what in &cut_stopped {
        md.push_str(&format!("- {what}: stopped at the budget\n"));
    }
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
            budget_stopped: false,
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
            measurements: vec![],
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
        assert!(verdict(&r).starts_with("FAILURES: 1 test(s)"));
    }

    #[test]
    fn a_hang_counts_and_an_all_clean_run_says_so() {
        let mut r = Report::default();
        r.binaries.push(bin("a", &[]));
        assert_eq!(
            verdict(&r),
            "ALL PASSED (failures that need a missing tool are listed apart)"
        );
        let mut hung = bin("h", &[]);
        hung.timed_out = true;
        hung.exit_code = None;
        r.binaries.push(hung);
        assert_eq!(totals(&r).hung_or_crashed, 1);
    }

    /// A run the budget cut is never ALL PASSED, whatever else it found,
    /// and the report says what did not run.
    #[test]
    fn a_run_the_budget_cut_is_incomplete_and_never_all_passed() {
        let mut r = Report::default();
        r.binaries.push(bin("a", &[]));
        assert!(verdict(&r).starts_with("ALL PASSED"));
        r.budget = Some(crate::budget::BudgetReport {
            mins: 85,
            not_run: vec![crate::budget::NotRun {
                what: "k/test/late".into(),
                why: "not run: budget (expected about 90 s, 12 s left)".into(),
            }],
            stopped: vec!["k/test/slow".into()],
        });
        let v = verdict(&r);
        assert!(v.starts_with("INCOMPLETE"), "{v}");
        assert!(!v.contains("ALL PASSED"), "{v}");
        assert!(v.contains("85 min"), "{v}");
        let md = render(&r);
        assert!(md.contains("k/test/late: not run: budget"), "{md}");
        assert!(md.contains("k/test/slow: stopped at the budget"), "{md}");
        assert!(md.contains("| Time budget | 85 min, REACHED"), "{md}");
        // Failures that did happen stay in the line.
        r.binaries.push(bin("f", &[("t", "boom")]));
        let v = verdict(&r);
        assert!(
            v.starts_with("INCOMPLETE") && v.contains("1 test(s) failed"),
            "{v}"
        );
    }

    /// Cuts in the sandbox user half alone make the run incomplete too, and
    /// a phase that did not start for lack of time counts.
    #[test]
    fn a_cut_in_the_sandbox_user_half_makes_the_run_incomplete() {
        let mut r = Report::default();
        r.binaries.push(bin("a", &[]));
        r.budget = Some(crate::budget::BudgetReport {
            mins: 60,
            ..Default::default()
        });
        assert!(verdict(&r).starts_with("ALL PASSED"));
        assert!(render(&r).contains("| Time budget | 60 min, not reached"));
        let mut p = crate::sandbox_phase::SandboxPhase::default();
        p.stopped_at_budget.push("sandbox user re-run of x".into());
        r.sandbox_user = Some(p);
        assert!(verdict(&r).starts_with("INCOMPLETE"), "{}", verdict(&r));
        let p = crate::sandbox_phase::SandboxPhase {
            skipped: Some("budget: setup and the gate need about 300 s, 12 s left".into()),
            ..Default::default()
        };
        r.sandbox_user = Some(p);
        assert!(verdict(&r).starts_with("INCOMPLETE"), "{}", verdict(&r));
        assert!(render(&r).contains("sandbox user phase: not run: budget"));
    }

    /// A binary the budget stopped is neither clean nor hung or crashed: the
    /// verdict is about the budget, not a hang.
    #[test]
    fn a_binary_stopped_at_the_budget_is_not_counted_as_hung() {
        let mut r = Report::default();
        let mut b = bin("s", &[]);
        b.budget_stopped = true;
        b.exit_code = None;
        assert!(!b.clean());
        r.binaries.push(b);
        assert_eq!(totals(&r).hung_or_crashed, 0);
        r.budget = Some(crate::budget::BudgetReport {
            mins: 5,
            not_run: vec![],
            stopped: vec!["s".into()],
        });
        assert!(verdict(&r).starts_with("INCOMPLETE"));
        assert!(render(&r).contains("| s | Core | 3 | 0 | 0 | stopped at the budget |"));
    }

    const MSYS: &str = "bash.exe: *** fatal error - \
                        NtCreateDirectoryObject(\\BaseNamedObjects\\msys-2.0S5-1): 0xC0000022";

    fn passed(b: &mut BinaryResult, names: &[&str]) {
        for n in names {
            b.tests.push(TestOutcome {
                name: (*n).into(),
                outcome: Outcome::Passed,
            });
        }
    }

    fn e2e_check(id: &str, status: Status) -> Check {
        Check {
            id: id.into(),
            title: id.into(),
            simulated: false,
            status,
            detail: String::new(),
            evidence: String::new(),
        }
    }

    /// A run whose baseline fails three tests (two by the token sandbox,
    /// one plainly) and one end-to-end check, and whose sandbox user half
    /// fails one test of its own, one end-to-end check and the DNS gate.
    fn two_halves() -> Report {
        let mut r = Report::default();
        r.binaries.push(bin(
            "x/test/y",
            &[
                ("msys_a", MSYS),
                ("msys_b", MSYS),
                ("plain", "assertion failed"),
            ],
        ));
        r.e2e.push(e2e_check("shell.git_bash", Status::Fail));
        let mut again = bin("x/test/y", &[("new_failure", "the station is wrong")]);
        passed(&mut again, &["msys_a", "msys_b", "plain"]);
        r.sandbox_user = Some(crate::sandbox_phase::SandboxPhase {
            reruns: vec![crate::sandbox_phase::Rerun {
                why: "w".into(),
                result: again,
            }],
            e2e: vec![
                e2e_check("shell.git_bash", Status::Pass),
                e2e_check("readers.cmd_plain", Status::Fail),
            ],
            dns_failed: Some("dns: not confined (status 9003)".into()),
            ..Default::default()
        });
        r
    }

    /// The headline drops only the baseline failures the token sandbox
    /// explains, and counts every failure of the sandbox user half and every
    /// failed end-to-end check; the DNS gap is a named line.
    #[test]
    fn the_headline_counts_both_halves_and_lists_the_expected_apart() {
        let r = two_halves();
        let h = headline(&r);
        assert_eq!(h.expected, 2);
        // `plain` (no marker) and the re-run's own failure.
        assert_eq!(h.failed, 2);
        assert_eq!(h.e2e_fail, 2);
        assert_eq!(h.known_gaps.len(), 1);
        assert_eq!(
            verdict(&r),
            "FAILURES: 2 test(s) failed, 0 binary run(s) hung or crashed, 2 end-to-end check(s) \
             failed; 2 baseline failure(s) expected under the token sandbox are listed apart"
        );
        let md = render(&r);
        assert!(
            md.contains("**Known gap: DNS is not confined on this machine"),
            "{md}"
        );
        assert!(md.contains("dns: not confined (status 9003)"));
        let failures = &md[md.find("## Failures").expect("failures")..];
        let failures = &failures[..failures.find("## Expected").expect("expected")];
        assert!(failures.contains("### x/test/y :: plain"));
        assert!(!failures.contains(":: msys_a"), "{failures}");
        assert!(failures.contains("### sandbox user: x/test/y :: new_failure"));
        assert!(failures.contains("### sandbox user e2e readers.cmd_plain"));
        assert!(failures.contains("### e2e shell.git_bash"));
        let expected = &md[md
            .find("## Expected under the token sandbox (2)")
            .expect("section")..];
        assert!(expected.contains("| x/test/y | msys_a | Git Bash (msys) | passed |"));
        assert!(expected.contains("| x/test/y | msys_b | Git Bash (msys) | passed |"));
    }

    /// With every failure explained, the verdict reads clean and still says
    /// how many it listed apart; a hung re-run binary counts.
    #[test]
    fn a_run_whose_baseline_failures_are_all_expected_reads_clean() {
        let mut r = Report::default();
        r.binaries.push(bin("x/test/y", &[("msys_a", MSYS)]));
        let mut again = bin("x/test/y", &[]);
        passed(&mut again, &["msys_a"]);
        r.sandbox_user = Some(crate::sandbox_phase::SandboxPhase {
            reruns: vec![crate::sandbox_phase::Rerun {
                why: "w".into(),
                result: again,
            }],
            ..Default::default()
        });
        assert_eq!(
            verdict(&r),
            "ALL PASSED (failures that need a missing tool are listed apart; 1 baseline \
             failure(s) expected under the token sandbox are listed apart)"
        );
        if let Some(p) = r.sandbox_user.as_mut() {
            p.reruns[0].result.timed_out = true;
        }
        assert!(verdict(&r).starts_with("FAILURES: 0 test(s) failed, 1 binary run(s) hung"));
    }

    /// A baseline end-to-end check leaves the count by the rule a test does:
    /// its own output names a token sandbox limit and the sandbox user passed
    /// the same check. Without either half it counts.
    #[test]
    fn a_baseline_check_is_expected_only_with_a_marker_and_a_sandbox_user_pass() {
        let mut marked = e2e_check("shell.git_bash", Status::Fail);
        marked.evidence = format!("ERROR: exit status: -1073741502\nstderr:\n{MSYS}");
        let run = |baseline: Check, user: Status| {
            let mut r = Report::default();
            r.binaries.push(bin("a", &[]));
            r.e2e.push(baseline);
            r.sandbox_user = Some(crate::sandbox_phase::SandboxPhase {
                e2e: vec![e2e_check("shell.git_bash", user)],
                ..Default::default()
            });
            r
        };
        let r = run(marked.clone(), Status::Pass);
        let h = headline(&r);
        assert_eq!((h.e2e_fail, h.expected), (0, 1));
        assert_eq!(
            verdict(&r),
            "ALL PASSED (failures that need a missing tool are listed apart; 1 baseline \
             failure(s) expected under the token sandbox are listed apart)"
        );
        let md = render(&r);
        let failures = &md[md.find("## Failures").expect("failures")..];
        let failures = &failures[..failures.find("## Expected").expect("expected")];
        assert!(!failures.contains("### e2e shell.git_bash"), "{failures}");
        assert!(
            md.contains("| end-to-end check | shell.git_bash | Git Bash (msys) | passed |"),
            "{md}"
        );
        // The sandbox user failed or skipped it: it counts.
        for user in [Status::Fail, Status::Skipped] {
            let h = headline(&run(marked.clone(), user));
            assert_eq!(
                (h.e2e_fail, h.expected),
                (2 - usize::from(user == Status::Skipped), 0)
            );
        }
        // No marker in its own output: it counts, though the user passed it.
        let plain = e2e_check("shell.git_bash", Status::Fail);
        let h = headline(&run(plain, Status::Pass));
        assert_eq!((h.e2e_fail, h.expected), (1, 0));
    }

    /// The DNS test's failure as the sandbox user is the known gap, not a
    /// counted failure, only when the gate measured the gap and the test says
    /// it is the recorded one. A leak with every filter in place counts.
    #[test]
    fn the_dns_tests_failure_is_the_known_gap_only_when_the_gate_measured_it() {
        let gap_tail = "the offline user's DnsQuery got a DNS server's answer (status 9003): \
                        lookups leave the machine. This is the recorded gap: a name resolution \
                        cache filter is absent or disabled";
        let defect_tail = "the offline user's DnsQuery got a DNS server's answer (status 9003): \
                           lookups leave the machine although every name resolution cache \
                           filter is present and running. That is a defect of the filters, not \
                           the recorded gap";
        let run = |test: &str, tail: &str, dns: bool| {
            let mut r = Report::default();
            r.binaries.push(bin("kgraf-sandbox/test/dns_windows", &[]));
            r.sandbox_user = Some(crate::sandbox_phase::SandboxPhase {
                reruns: vec![crate::sandbox_phase::Rerun {
                    why: "sandbox test binary".into(),
                    result: bin("kgraf-sandbox/test/dns_windows", &[(test, tail)]),
                }],
                dns_failed: dns.then(|| "dns: not confined (status 9003)".to_owned()),
                ..Default::default()
            });
            r
        };
        let test = crate::sandbox_phase::DNS_GAP_TEST.1;
        let r = run(test, gap_tail, true);
        let h = headline(&r);
        assert_eq!(h.failed, 0);
        assert!(
            h.known_gaps[0].contains(&format!("kgraf-sandbox/test/dns_windows :: {test} fails")),
            "{:?}",
            h.known_gaps
        );
        let md = render(&r);
        let failures = &md[md.find("## Failures").expect("failures")..];
        let failures = &failures[..failures.find("## Known gaps").expect("known gaps")];
        assert!(failures.contains("None."), "{failures}");
        assert!(
            md.contains("## Known gaps\n\nSandbox user failures not counted in the verdict")
                && md.contains(&format!(
                    "### sandbox user: kgraf-sandbox/test/dns_windows :: {test}"
                )),
            "{md}"
        );
        assert!(verdict(&r).starts_with("ALL PASSED"), "{}", verdict(&r));
        // The gate did not measure the gap: the failure counts.
        assert_eq!(headline(&run(test, gap_tail, false)).failed, 1);
        // Every filter in place, so not the recorded gap: it counts.
        assert_eq!(headline(&run(test, defect_tail, true)).failed, 1);
        // Another test of the binary, with the same words: it counts.
        assert_eq!(headline(&run("netsh_lists", gap_tail, true)).failed, 1);
    }

    /// A test the baseline could not re-run alone is named under the cap
    /// only while it counts: one the sandbox user explained is listed apart.
    #[test]
    fn the_cap_list_names_only_the_counted_failures() {
        let mut r = Report::default();
        let mut b = bin(
            "x/test/y",
            &[("msys_a", MSYS), ("plain", "assertion failed")],
        );
        b.rerun_skipped = vec!["msys_a".into(), "plain".into()];
        r.binaries.push(b);
        let mut again = bin("x/test/y", &[("plain", "assertion failed")]);
        passed(&mut again, &["msys_a"]);
        r.sandbox_user = Some(crate::sandbox_phase::SandboxPhase {
            reruns: vec![crate::sandbox_phase::Rerun {
                why: "w".into(),
                result: again,
            }],
            ..Default::default()
        });
        let md = render(&r);
        let cap = &md[md
            .find("### x/test/y: not re-run alone (cap reached)")
            .expect("cap list")..];
        let cap = &cap[..cap.find("\n\n###").unwrap_or(cap.len())];
        assert!(cap.contains("- plain"), "{cap}");
        assert!(!cap.contains("- msys_a"), "{cap}");
        // With every skipped test explained, no cap list is printed.
        if let Some(b) = r.binaries.first_mut() {
            b.rerun_skipped = vec!["msys_a".into()];
        }
        assert!(!render(&r).contains("cap reached"));
    }

    #[test]
    fn a_filtered_run_says_so_before_the_verdict() {
        let r = Report {
            only: Some("kgraf-sandbox/test/escape_windows,e2e".into()),
            ..Report::default()
        };
        let md = render(&r);
        let filter = md
            .find("**Filter: `--only kgraf-sandbox/test/escape_windows,e2e`.")
            .expect("filter line");
        let verdict = md.find("**ALL PASSED").expect("verdict");
        assert!(filter < verdict);
        assert!(md.contains("filtered: --only kgraf-sandbox/test/escape_windows,e2e"));
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
