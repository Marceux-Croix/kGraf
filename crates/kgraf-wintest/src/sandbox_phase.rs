//! The "sandbox user" phase of a run (`--sandbox-user`).
//!
//! The suite runs first, as always, with no sandbox user set up: that is the
//! baseline. This phase then sets the Windows sandbox user up, checks it, runs
//! the parts of the suite the user changes with it active, and always removes
//! it again:
//!
//! ```text
//!  (b) kgraf sandbox setup
//!  (c) kgraf sandbox status      every kGraf line present, every stored password logs on
//!  (d) kgraf sandbox selfcheck   the gate: if it fails, (e) does not run
//!  (e) re-run: the sandbox test binaries, the baseline binaries that failed for
//!      Git Bash, the cargo jobserver or Schannel, and the end-to-end checks
//!  (f) kgraf sandbox remove, then kgraf sandbox status: nothing left (always)
//! ```
//!
//! The decisions live here and run against the [`Exec`] trait, so tests use
//! fake results. The real [`Exec`] is in `runner.rs`; it reuses the runner's
//! one process path (`proc::run`), binary runner and end-to-end runner.

use std::time::Duration;

use serde::Serialize;

use crate::e2e::{Check, Status};
use crate::libtest::Outcome;
use crate::report::BinaryResult;

pub const SETUP_LIMIT: Duration = Duration::from_secs(300);
pub const STATUS_LIMIT: Duration = Duration::from_secs(120);
pub const SELFCHECK_LIMIT: Duration = Duration::from_secs(300);

/// The lines `kgraf sandbox selfcheck` prints for a pass that proved network
/// confinement, and the backend line of the Windows sandbox user
/// (crates/kgraf-cli/src/sandbox_cmd.rs, `backend_name`).
const SELFCHECK_OK: &str = "sandbox selfcheck: ok";
const SELFCHECK_SANDBOX_USER: &str = "  backend: Windows sandbox user";
/// The first line of a selfcheck that ran every step and failed one, the
/// `dns:` step.
const SELFCHECK_FAILED: &str = "sandbox selfcheck: failed";

/// The gate passes only when selfcheck exited 0, proved confinement, and ran
/// on the sandbox user. Under the token sandbox selfcheck also exits 0, with a
/// network line that is not a pass; a setup that did not take effect must
/// stop the phase, not re-run the tests on the old backend.
fn gate_passed(step: &StepRun) -> bool {
    step.ok()
        && step.output.lines().any(|l| l.trim_end() == SELFCHECK_OK)
        && step
            .output
            .lines()
            .any(|l| l.trim_end() == SELFCHECK_SANDBOX_USER)
}

/// The `dns:` line of a selfcheck that failed on DNS alone: it ran on the
/// sandbox user, proved the network, the hidden folder and the tree, and its
/// only failed line is `dns:`. `None` for any other output.
///
/// The DNS filters are the newest part of the sandbox user and the ones most
/// likely not to work on a given Windows (decision 0097): it is not known
/// whether a block at the name resolution cache layers makes a lookup fail at
/// all. A DNS failure is reported in the summary, but the re-run (e) still
/// runs, so one unproved filter does not throw away the evidence about Git
/// Bash, the cargo jobserver and Schannel that the phase exists to collect.
/// A `dns: not proved` line counts as a DNS failure here, like
/// `dns: not confined`: neither is a pass.
fn dns_only_failure(step: &StepRun) -> Option<String> {
    let lines: Vec<&str> = step.output.lines().map(str::trim_end).collect();
    let has = |want: &str| lines.contains(&want);
    let dns = lines
        .iter()
        .find(|l| l.starts_with("  dns: ") && !l.starts_with("  dns: confined"))?;
    (has(SELFCHECK_FAILED)
        && has(SELFCHECK_SANDBOX_USER)
        && has("  network: only the egress proxy was reachable")
        && has("  hidden folder: not writable")
        && has("  tree: writable"))
    .then(|| dns.trim().to_owned())
}
/// The error line of a selfcheck whose probe did not run on the sandbox
/// user: the backend is the sandbox user, but the probe failed before it
/// printed any result line (for example, Git Bash did not start as that
/// user). `None` for any other output.
///
/// The re-run (e) still runs then. Its tests start cargo, Git Bash and curl
/// as the sandbox user one by one, which is the only evidence of which of
/// them fail there; stopping at the gate would throw that away and leave
/// only the probe's one error. The phase reports the gate as failed.
fn probe_failure_under_user(step: &StepRun) -> Option<String> {
    const PREFIX: &str = "sandbox selfcheck failed (backend: Windows sandbox user): ";
    let lines: Vec<&str> = step.output.lines().map(str::trim_end).collect();
    if step.ok() || lines.iter().any(|l| l.starts_with("  network: ")) {
        return None;
    }
    lines.iter().find_map(|l| {
        let l = l.strip_prefix("Error: ").unwrap_or(l);
        l.starts_with(PREFIX).then(|| l.to_owned())
    })
}

pub const REMOVE_LIMIT: Duration = Duration::from_secs(300);

/// The worker slots setup makes users for, passed explicitly so the run
/// does not depend on the default. Two or more: the escape tests check that
/// two worktrees run as two different users, and with one slot or none that
/// check only sees the refusal.
pub const SETUP_SLOTS: u8 = kgraf_sandbox::sandbox_user::DEFAULT_SLOTS;

/// The arguments of step (b).
pub fn setup_args() -> Vec<String> {
    ["sandbox", "setup", "--slots"]
        .iter()
        .map(|s| (*s).to_owned())
        .chain(std::iter::once(SETUP_SLOTS.to_string()))
        .collect()
}

/// Test binaries that always re-run with the sandbox user active (ids without
/// the `#core` or `#rest` suffix).
pub const ALWAYS: &[&str] = &[
    "kgraf-sandbox/lib/kgraf_sandbox",
    "kgraf-sandbox/test/escape_windows",
    "kgraf-sandbox/test/tls_windows",
    "kgraf-sandbox/test/shell_windows",
    "kgraf-sandbox/test/startup_windows",
    "kgraf-sandbox/test/dns_windows",
    "kgraf-sandbox/test/runner_windows",
    "kgraf-sandbox/test/stdio_windows",
];

/// Output of a failed test that names a limit the sandbox user may lift, as
/// (text to find in lower case, name in the report).
const BLOCKERS: &[(&str, &str)] = &[
    ("jobserver", "cargo jobserver"),
    ("git bash", "Git Bash"),
    ("msys", "Git Bash (msys)"),
    (
        "ntcreatedirectoryobject",
        "Git Bash (NtCreateDirectoryObject)",
    ),
    ("signal pipe", "Git Bash (signal pipe)"),
    ("sec_e_no_credentials", "Schannel (SEC_E_NO_CREDENTIALS)"),
    ("schannel", "Schannel"),
];

pub const NEEDS_ELEVATION: &str = "This window is not elevated. `kgraf sandbox setup` and \
`kgraf sandbox remove` need administrator rights: start the runner from an elevated prompt \
(Run as administrator), or accept the UAC prompt when Windows shows it. \
Without administrator rights these steps fail.";

/// One `kgraf` command of the phase, as it ended.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StepRun {
    pub label: String,
    pub command: String,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub start_error: Option<String>,
    pub secs: f64,
    /// All the output kept (stdout and stderr, interleaved).
    pub output: String,
}

impl StepRun {
    pub fn ok(&self) -> bool {
        self.start_error.is_none() && !self.timed_out && !self.cancelled && self.code == Some(0)
    }

    fn how(&self) -> String {
        if let Some(e) = &self.start_error {
            format!("did not start: {e}")
        } else if self.timed_out {
            "timed out".into()
        } else if self.cancelled {
            "cancelled".into()
        } else {
            format!("exit {:?}", self.code)
        }
    }
}

/// What the phase needs from the real run. Fakes implement it in tests.
pub trait Exec {
    /// Checks that the phase may run and prepares it. `Err` skips the whole
    /// phase, and nothing was changed.
    fn preflight(&mut self) -> Result<(), String>;
    /// Runs `kgraf <args>`. `honor_cancel` is false for the removal, which
    /// must run even after Ctrl+C.
    fn cli(&mut self, label: &str, args: &[&str], limit: Duration, honor_cancel: bool) -> StepRun;
    /// Runs one test binary of the suite again, by manifest id.
    fn rerun(&mut self, id: &str) -> BinaryResult;
    fn e2e(&mut self) -> Vec<Check>;
    /// Undoes what `preflight` placed; returns notes.
    fn teardown(&mut self) -> Vec<String>;
}

/// One line of `kgraf sandbox status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusLine {
    /// `present`, `MISSING`, `WRONG`, `FAILED` or `unknown`. `FAILED` is a
    /// part Windows refused at setup, such as a DNS filter.
    pub word: String,
    pub item: String,
}

/// The two lines about the machine, not about kGraf's own users. They stay
/// `present` after a removal.
const MACHINE_ITEMS: &[&str] = &[
    "Windows Firewall on for every profile",
    "local firewall rules take effect",
];

pub fn parse_status(out: &str) -> Vec<StatusLine> {
    out.lines()
        .filter_map(|l| {
            let t = l.strip_prefix("  ")?;
            let (word, item) = t.split_once(' ')?;
            matches!(word, "present" | "MISSING" | "WRONG" | "FAILED" | "unknown").then(|| {
                StatusLine {
                    word: word.to_owned(),
                    item: item.trim().to_owned(),
                }
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StatusCheck {
    pub ok: bool,
    /// Lines that are missing or wrong, or other faults.
    pub problems: Vec<String>,
    /// Lines `unknown` (for example logon rights read without administrator
    /// rights): not a fault, but listed.
    pub unknown: Vec<String>,
    /// `stored password of <user>` lines that are present. A stored password
    /// is `present` only when a real logon with it worked.
    pub logon_works: usize,
    /// `<role> sandbox user <name>` lines: how many users the setup has, two
    /// fixed ones and two per worker slot.
    pub users: usize,
}

/// (c): after setup every line is present, and the stored password of every
/// sandbox user logs on.
pub fn check_ready(out: &str, code: Option<i32>) -> StatusCheck {
    let lines = parse_status(out);
    let mut c = StatusCheck::default();
    if lines.is_empty() {
        c.problems.push("no status line was printed".into());
    }
    for l in &lines {
        match l.word.as_str() {
            "MISSING" | "WRONG" | "FAILED" => c.problems.push(format!("{} {}", l.word, l.item)),
            "unknown" => c.unknown.push(l.item.clone()),
            _ => {}
        }
        if l.word == "present" && l.item.starts_with("stored password of ") {
            c.logon_works += 1;
        }
        if l.item.contains(" sandbox user kgraf-") {
            c.users += 1;
        }
    }
    if c.users < 2 || c.logon_works != c.users {
        c.problems.push(format!(
            "{} of {} stored passwords log on (expected every one, and at least 2)",
            c.logon_works, c.users
        ));
    }
    if code != Some(0) {
        c.problems
            .push(format!("the status command ended with {code:?}"));
    }
    c.ok = c.problems.is_empty();
    c
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GoneCheck {
    pub ok: bool,
    pub left: Vec<String>,
}

/// (f): after the removal every line about kGraf's own items is MISSING.
pub fn check_gone(out: &str) -> GoneCheck {
    let lines = parse_status(out);
    let mut g = GoneCheck::default();
    if lines.is_empty() {
        g.left
            .push("no status line was printed, so removal is not confirmed".into());
    }
    for l in &lines {
        if l.word != "MISSING" && !MACHINE_ITEMS.contains(&l.item.as_str()) {
            g.left.push(format!("{} {}", l.word, l.item));
        }
    }
    g.ok = g.left.is_empty();
    g
}

/// The limit a failed test's output names, if any.
pub fn blocker(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    BLOCKERS
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, name)| *name)
}

fn base_id(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
}

/// The text of a baseline binary result where a blocker may be named.
fn failure_texts(b: &BinaryResult) -> Vec<&str> {
    let mut v: Vec<&str> = b.failures.iter().map(|f| f.tail.as_str()).collect();
    if let Some(f) = &b.open_test_solo {
        v.push(&f.tail);
    }
    if let Some(t) = &b.tail {
        v.push(t);
    }
    v
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Selection {
    pub id: String,
    pub why: String,
}

/// The binaries of the baseline to run again with the sandbox user active, in
/// baseline order, each once.
pub fn select(first: &[BinaryResult]) -> Vec<Selection> {
    first
        .iter()
        .filter_map(|b| {
            let why = if ALWAYS.contains(&base_id(&b.id)) {
                "sandbox test binary".to_owned()
            } else if !b.clean() {
                let hit = failure_texts(b).into_iter().find_map(blocker)?;
                format!("failed in the baseline, output names {hit}")
            } else {
                return None;
            };
            Some(Selection {
                id: b.id.clone(),
                why,
            })
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct Rerun {
    pub why: String,
    pub result: BinaryResult,
}

/// Everything the phase did and found.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SandboxPhase {
    /// Why the phase did not start (nothing was changed), if so.
    pub skipped: Option<String>,
    /// selfcheck before setup, for comparison: on the token sandbox it passes
    /// with a network line that is not a pass. Not a gate.
    pub selfcheck_before: Option<StepRun>,
    pub setup: Option<StepRun>,
    pub status: Option<StepRun>,
    pub status_check: Option<StatusCheck>,
    pub selfcheck: Option<StepRun>,
    /// (d) failed, so (e) did not run.
    pub stopped_at_gate: bool,
    /// (d) failed on its `dns:` line alone (that line); (e) ran anyway.
    pub dns_failed: Option<String>,
    /// (d) failed because its probe did not run on the sandbox user (the
    /// error line); (e) ran anyway.
    pub probe_failed: Option<String>,
    pub cancelled: bool,
    pub reruns: Vec<Rerun>,
    pub e2e: Vec<Check>,
    pub remove: Option<StepRun>,
    pub status_after: Option<StepRun>,
    pub gone: Option<GoneCheck>,
    pub notes: Vec<String>,
}

impl SandboxPhase {
    /// The phase left a sandbox user behind.
    pub fn left_behind(&self) -> bool {
        self.gone.as_ref().is_some_and(|g| !g.ok)
    }

    pub fn summary(&self) -> String {
        if let Some(s) = &self.skipped {
            return format!("not run: {s}");
        }
        let gate = if self.stopped_at_gate {
            "stopped at the gate (selfcheck failed), the re-run did not happen"
        } else if self.cancelled {
            "cancelled before the re-run ended"
        } else {
            "ran to the end"
        };
        let left = match &self.gone {
            Some(g) if g.ok => "removed, nothing left",
            Some(_) => "STILL THERE after removal",
            None => "removal not checked",
        };
        let dns = if self.dns_failed.is_some() {
            "; DNS FAILED in selfcheck (see the gate)"
        } else {
            ""
        };
        let probe = if self.probe_failed.is_some() {
            "; GATE FAILED: the selfcheck probe did not run on the sandbox user (see the gate)"
        } else {
            ""
        };
        format!("{gate}; sandbox user {left}{dns}{probe}")
    }
}

/// Runs the phase. `first` is the baseline's test binaries; `elevated` is
/// what the machine facts say.
pub fn run_phase(
    ex: &mut dyn Exec,
    first: &[BinaryResult],
    elevated: Option<bool>,
    cancelled: &dyn Fn() -> bool,
) -> SandboxPhase {
    let mut p = SandboxPhase::default();
    if elevated == Some(false) {
        p.notes.push(NEEDS_ELEVATION.into());
    }
    if let Err(why) = ex.preflight() {
        p.skipped = Some(why);
        return p;
    }

    // From here on, (f) always runs.
    p.selfcheck_before = Some(ex.cli(
        "(b0) selfcheck before setup",
        &["sandbox", "selfcheck"],
        SELFCHECK_LIMIT,
        true,
    ));
    let args = setup_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let setup = ex.cli("(b) setup", &args, SETUP_LIMIT, true);
    if !setup.ok() {
        p.notes.push(format!("Setup {}.", setup.how()));
        if elevated.is_none() {
            p.notes.push(NEEDS_ELEVATION.into());
        }
    }
    p.setup = Some(setup);

    let status = ex.cli("(c) status", &["sandbox", "status"], STATUS_LIMIT, true);
    p.status_check = Some(check_ready(&status.output, status.code));
    p.status = Some(status);

    let gate = ex.cli(
        "(d) selfcheck (gate)",
        &["sandbox", "selfcheck"],
        SELFCHECK_LIMIT,
        true,
    );
    let passed = gate_passed(&gate);
    p.dns_failed = if passed {
        None
    } else {
        dns_only_failure(&gate)
    };
    p.probe_failed = if passed {
        None
    } else {
        probe_failure_under_user(&gate)
    };
    p.selfcheck = Some(gate);
    if !passed && p.dns_failed.is_none() && p.probe_failed.is_none() {
        p.stopped_at_gate = true;
    } else {
        for s in select(first) {
            if cancelled() {
                p.cancelled = true;
                break;
            }
            let result = ex.rerun(&s.id);
            p.reruns.push(Rerun { why: s.why, result });
        }
        if cancelled() {
            p.cancelled = true;
        } else {
            p.e2e = ex.e2e();
        }
    }

    // (f): never skipped, not even by Ctrl+C.
    p.remove = Some(ex.cli("(f) remove", &["sandbox", "remove"], REMOVE_LIMIT, false));
    let after = ex.cli(
        "(f) status after removal",
        &["sandbox", "status"],
        STATUS_LIMIT,
        false,
    );
    p.gone = Some(check_gone(&after.output));
    p.status_after = Some(after);
    p.notes.extend(ex.teardown());
    p
}

/// One test that failed in the baseline, and what it did in the re-run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Compared {
    pub binary: String,
    pub test: String,
    /// `passed`, `failed`, `hung` or `not reached`.
    pub after: String,
    pub tail: String,
}

fn after_of(test: &str, r: &BinaryResult) -> (String, String) {
    if let Some(f) = r.failures.iter().find(|f| f.test == test) {
        return (
            f.solo.clone().unwrap_or_else(|| "failed".into()),
            f.tail.clone(),
        );
    }
    if r.open_test.as_deref() == Some(test) {
        let tail = r
            .open_test_solo
            .as_ref()
            .map(|f| f.tail.clone())
            .or_else(|| r.tail.clone())
            .unwrap_or_default();
        let how = r
            .open_test_solo
            .as_ref()
            .and_then(|f| f.solo.clone())
            .unwrap_or_else(|| if r.timed_out { "hung" } else { "failed" }.into());
        return (how, tail);
    }
    match r.tests.iter().find(|t| t.name == test) {
        Some(t) if t.outcome == Outcome::Passed => ("passed".into(), "passed".into()),
        Some(_) => ("not reached".into(), r.tail.clone().unwrap_or_default()),
        None => ("not reached".into(), r.tail.clone().unwrap_or_default()),
    }
}

/// Every test that failed (or was running when the binary ended) in the
/// baseline of a re-run binary, with its result in the re-run.
pub fn compare(first: &[BinaryResult], reruns: &[Rerun]) -> Vec<Compared> {
    let mut out = Vec::new();
    for rr in reruns {
        let Some(a) = first.iter().find(|b| b.id == rr.result.id) else {
            continue;
        };
        let mut names: Vec<&str> = a.failures.iter().map(|f| f.test.as_str()).collect();
        if let Some(o) = a.open_test.as_deref()
            && !names.contains(&o)
        {
            names.push(o);
        }
        for n in names {
            let (after, tail) = after_of(n, &rr.result);
            out.push(Compared {
                binary: a.id.clone(),
                test: n.to_owned(),
                after,
                tail,
            });
        }
    }
    out
}

fn step_block(s: &Option<StepRun>) -> String {
    match s {
        None => "Not run.\n\n".into(),
        Some(s) => format!(
            "`{}`: {}, {:.0} s\n\n{}\n",
            s.command,
            s.how(),
            s.secs,
            crate::report::fence("text", &s.output)
        ),
    }
}

fn e2e_word(c: Option<&Check>) -> String {
    c.map_or("not run".into(), |c| format!("{:?}", c.status))
}

/// The `measurement:` lines of the re-run (e), each with its binary, test
/// and the test's time. libtest prints a passing test's output only in its
/// summary (`--show-output`), so a binary that ended without one lost the
/// lines of its passing tests; the section says which.
pub fn render_measurements(reruns: &[Rerun]) -> String {
    let mut md = String::from("#### Measurements of the re-run\n\n");
    let all: Vec<(&str, &crate::report::Measurement)> = reruns
        .iter()
        .flat_map(|r| {
            r.result
                .measurements
                .iter()
                .map(move |m| (r.result.id.as_str(), m))
        })
        .collect();
    if all.is_empty() {
        md.push_str("No re-run test printed a line that starts with `measurement:`.\n\n");
    } else {
        md.push_str("| Binary | Test | Duration | Line |\n|---|---|---|---|\n");
        for (id, m) in &all {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                crate::report::md_escape(id),
                m.test
                    .as_deref()
                    .map_or("unknown".to_owned(), crate::report::md_escape),
                m.secs.map_or("unknown".to_owned(), |s| format!("{s:.1} s")),
                crate::report::md_escape(&m.line)
            ));
        }
        md.push('\n');
    }
    let lost: Vec<&str> = reruns
        .iter()
        .filter(|r| r.result.tail.is_some())
        .map(|r| r.result.id.as_str())
        .collect();
    if !lost.is_empty() {
        md.push_str(&format!(
            "Ended without a test summary, so the lines of their passing tests are missing: \
             {}.\n\n",
            lost.join(", ")
        ));
    }
    md
}

/// The report part for the phase: baseline and sandbox user side by side.
pub fn render(first: &[BinaryResult], first_e2e: &[Check], p: &SandboxPhase) -> String {
    let mut md = String::from("\n## Sandbox user phase\n\n");
    md.push_str(&format!("**{}**\n\n", p.summary()));
    for n in &p.notes {
        md.push_str(&format!("- {n}\n"));
    }
    if !p.notes.is_empty() {
        md.push('\n');
    }
    if p.skipped.is_some() {
        return md;
    }

    md.push_str("### (b0) selfcheck before setup (not a gate)\n\n");
    md.push_str(&step_block(&p.selfcheck_before));
    md.push_str("### (b) setup\n\n");
    md.push_str(&step_block(&p.setup));
    md.push_str("### (c) status\n\n");
    md.push_str(&step_block(&p.status));
    if let Some(c) = &p.status_check {
        md.push_str(&format!(
            "Check: {}. Stored passwords that log on: {} of {}. Unknown lines: {}.{}\n\n",
            if c.ok { "every line present" } else { "FAILED" },
            c.logon_works,
            c.users,
            c.unknown.len(),
            if c.problems.is_empty() {
                String::new()
            } else {
                format!(" Problems: {}.", c.problems.join("; "))
            }
        ));
    }
    md.push_str("### (d) selfcheck (gate)\n\n");
    md.push_str(&step_block(&p.selfcheck));
    if p.stopped_at_gate {
        md.push_str(
            "The phase stopped at the gate: selfcheck did not pass, so the re-run (e) did not \
             happen. The full selfcheck output is above.\n\n",
        );
    }
    if let Some(line) = &p.dns_failed {
        md.push_str(&format!(
            "**DNS FAILED:** `{line}`. Every other selfcheck step passed on the sandbox user, so \
             the re-run (e) ran. Name lookups by the offline users are not proved confined on \
             this machine.\n\n"
        ));
    }
    if let Some(line) = &p.probe_failed {
        md.push_str(&format!(
            "**GATE FAILED:** `{line}`. The selfcheck probe did not run on the sandbox user, so \
             nothing about its network, DNS or folders is proved. The re-run (e) ran anyway, so \
             each program its tests start shows whether it runs as the sandbox user.\n\n"
        ));
    }

    if !p.stopped_at_gate {
        md.push_str("### (e) baseline and sandbox user, side by side\n\n");
        md.push_str(
            "| Binary | Why re-run | Baseline passed | Baseline failed | Sandbox user passed | Sandbox user failed | Exit |\n|---|---|---|---|---|---|---|\n",
        );
        for rr in &p.reruns {
            let r = &rr.result;
            let a = first.iter().find(|b| b.id == r.id);
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                r.id,
                rr.why,
                a.map_or("-".into(), |a| a.passed.to_string()),
                a.map_or("-".into(), |a| a.failed.to_string()),
                r.passed,
                r.failed,
                if r.timed_out {
                    "timed out".to_owned()
                } else {
                    format!("{:?}", r.exit_code)
                }
            ));
        }
        let cmp = compare(first, &p.reruns);
        md.push_str("\n#### Failed in the baseline, run again with the sandbox user\n\n");
        if cmp.is_empty() {
            md.push_str("No re-run binary had a failure in the baseline.\n\n");
        } else {
            md.push_str("| Binary | Test | Baseline | Sandbox user |\n|---|---|---|---|\n");
            for c in &cmp {
                md.push_str(&format!(
                    "| {} | {} | failed | {} |\n",
                    crate::report::md_escape(&c.binary),
                    crate::report::md_escape(&c.test),
                    c.after
                ));
            }
            md.push('\n');
            for c in &cmp {
                md.push_str(&format!(
                    "##### {} :: {}: {}\n\n",
                    c.binary, c.test, c.after
                ));
                md.push_str(&crate::report::fence(
                    "text",
                    &crate::libtest::tail_text(&c.tail, 60),
                ));
                md.push('\n');
            }
        }
        md.push_str(&render_measurements(&p.reruns));
        md.push_str(
            "#### End-to-end checks\n\n| Check | Baseline | Sandbox user |\n|---|---|---|\n",
        );
        for c in &p.e2e {
            md.push_str(&format!(
                "| {} {} | {} | {:?} |\n",
                c.id,
                crate::report::md_escape(&c.title),
                e2e_word(first_e2e.iter().find(|f| f.id == c.id)),
                c.status
            ));
        }
        md.push('\n');
        for c in p.e2e.iter().filter(|c| c.status == Status::Fail) {
            md.push_str(&format!(
                "##### e2e {}: {}\n\n{}\n\n",
                c.id, c.title, c.detail
            ));
            md.push_str(&crate::report::fence("text", &c.evidence));
            md.push('\n');
        }
    }

    md.push_str("### (f) remove\n\n");
    md.push_str(&step_block(&p.remove));
    md.push_str("Status after the removal:\n\n");
    md.push_str(&step_block(&p.status_after));
    if let Some(g) = &p.gone {
        md.push_str(&if g.ok {
            "Confirmed: nothing of the sandbox user is left (every kGraf line is MISSING).\n"
                .to_owned()
        } else {
            format!("NOT CONFIRMED, still there: {}.\n", g.left.join("; "))
        });
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Set;
    use crate::report::Failure;

    fn bin(id: &str, failures: &[(&str, &str)]) -> BinaryResult {
        BinaryResult {
            id: id.into(),
            set: Set::Core,
            exit_code: Some(if failures.is_empty() { 0 } else { 101 }),
            timed_out: false,
            cancelled: false,
            start_error: None,
            secs: 1.0,
            expected_secs: 1.0,
            listed: None,
            passed: 3,
            failed: failures.len() as u64,
            ignored: 0,
            tests: failures
                .iter()
                .map(|(n, _)| crate::libtest::TestOutcome {
                    name: (*n).into(),
                    outcome: Outcome::Failed,
                })
                .collect(),
            failures: failures
                .iter()
                .map(|(n, t)| Failure {
                    test: (*n).into(),
                    class: "kgraf_defect_suspected".into(),
                    tail: (*t).into(),
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

    const SELFCHECK_PASS: &str =
        "sandbox selfcheck: ok\n  backend: Windows sandbox user\n  network: confined";
    const SELFCHECK_TOKEN: &str = "sandbox selfcheck: network not confined\n  backend: Windows restricted token (token sandbox)";

    const STATUS_OK: &str = "  present  Windows Firewall on for every profile\n  present  local firewall rules take effect\n  present  offline sandbox user kgraf-a\n  present  online sandbox user kgraf-b\n  present  stored password of kgraf-a\n  present  stored password of kgraf-b\nThe Windows sandbox user is set up.";
    const STATUS_GONE: &str = "  present  Windows Firewall on for every profile\n  present  local firewall rules take effect\n  MISSING  offline sandbox user kgraf-a\n  MISSING  online sandbox user kgraf-b\n  MISSING  stored passwords\nThe Windows sandbox user is not fully set up: run `kgraf sandbox setup`.";

    /// Records the calls; each `kgraf` command answers from `script`.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        selfcheck_code: i32,
        /// What selfcheck prints; `None`: a pass on the sandbox user.
        selfcheck_out: Option<&'static str>,
        setup_code: i32,
        removed: bool,
        preflight: Option<String>,
        /// Tests that pass in every re-run.
        passing: Vec<String>,
    }

    impl Exec for Fake {
        fn preflight(&mut self) -> Result<(), String> {
            self.calls.push("preflight".into());
            self.preflight.clone().map_or(Ok(()), Err)
        }
        fn cli(&mut self, label: &str, args: &[&str], _l: Duration, honor: bool) -> StepRun {
            self.calls
                .push(format!("{} cancel={honor}", args.join(" ")));
            let (code, out) = match args[1] {
                "setup" => (self.setup_code, "setup output"),
                "remove" => {
                    self.removed = true;
                    (0, "removed")
                }
                "status" if self.removed => (1, STATUS_GONE),
                "status" => (0, STATUS_OK),
                _ => (
                    self.selfcheck_code,
                    self.selfcheck_out.unwrap_or(SELFCHECK_PASS),
                ),
            };
            StepRun {
                label: label.into(),
                command: format!("kgraf {}", args.join(" ")),
                code: Some(code),
                output: out.into(),
                ..StepRun::default()
            }
        }
        fn rerun(&mut self, id: &str) -> BinaryResult {
            self.calls.push(format!("rerun {id}"));
            let mut b = bin(id, &[]);
            b.tests = self
                .passing
                .iter()
                .map(|n| crate::libtest::TestOutcome {
                    name: n.clone(),
                    outcome: Outcome::Passed,
                })
                .collect();
            b
        }
        fn e2e(&mut self) -> Vec<Check> {
            self.calls.push("e2e".into());
            vec![]
        }
        fn teardown(&mut self) -> Vec<String> {
            self.calls.push("teardown".into());
            vec![]
        }
    }

    fn baseline() -> Vec<BinaryResult> {
        vec![
            bin("kgraf-sandbox/test/escape_windows", &[]),
            bin("kgraf-host/lib/kgraf_host", &[]),
            bin(
                "kgraf-cli/test/worker",
                &[("w::cargo", "cargo: jobserver failed"), ("w::x", "plain")],
            ),
        ]
    }

    #[test]
    fn the_gate_stops_the_rerun_and_the_removal_still_runs() {
        let mut ex = Fake {
            selfcheck_code: 1,
            selfcheck_out: Some("selfcheck output line"),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(p.stopped_at_gate);
        assert!(p.reruns.is_empty());
        assert!(p.e2e.is_empty());
        assert!(
            !ex.calls
                .iter()
                .any(|c| c.starts_with("rerun") || c == "e2e")
        );
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
        assert!(p.gone.as_ref().unwrap().ok);
        let md = render(&baseline(), &[], &p);
        assert!(md.contains("stopped at the gate"), "{md}");
        assert!(md.contains("selfcheck output line"));
    }

    /// Under the token sandbox selfcheck exits 0 with a network line that is
    /// not a pass. A setup that did not take effect must stop the phase.
    #[test]
    fn a_selfcheck_pass_on_the_token_sandbox_does_not_open_the_gate() {
        let mut ex = Fake {
            selfcheck_out: Some(SELFCHECK_TOKEN),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(p.stopped_at_gate);
        assert!(
            !ex.calls
                .iter()
                .any(|c| c.starts_with("rerun") || c == "e2e")
        );
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
    }

    const SELFCHECK_DNS_ONLY: &str = "sandbox selfcheck: failed\n  backend: Windows sandbox user\n  network: only the egress proxy was reachable\n  dns: not confined (lookups leave the machine): the sandbox user's lookup got a DNS server's answer (status 9003)\n  hidden folder: not writable\n  tree: writable\nError: sandbox selfcheck failed (backend: Windows sandbox user): dns: not confined";

    /// A selfcheck that failed on its DNS line alone does not stop the phase:
    /// the re-run still runs, and the summary and the report say DNS failed.
    #[test]
    fn a_dns_only_failure_reruns_and_says_dns_failed() {
        let mut ex = Fake {
            selfcheck_code: 1,
            selfcheck_out: Some(SELFCHECK_DNS_ONLY),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(!p.stopped_at_gate);
        assert!(ex.calls.iter().any(|c| c.starts_with("rerun")));
        let line = p.dns_failed.clone().expect("the DNS line is kept");
        assert!(line.starts_with("dns: not confined"), "{line}");
        assert!(p.summary().contains("DNS FAILED"), "{}", p.summary());
        let md = render(&baseline(), &[], &p);
        assert!(md.contains("**DNS FAILED:**"), "{md}");
    }

    /// Any other failed line still closes the gate, DNS line or not.
    #[test]
    fn a_dns_failure_with_another_failure_still_stops_at_the_gate() {
        for out in [
            // The network was not proved.
            "sandbox selfcheck: failed\n  backend: Windows sandbox user\n  dns: not proved: x\n  hidden folder: not writable\n  tree: writable",
            // The token sandbox, not the sandbox user.
            "sandbox selfcheck: failed\n  backend: Windows restricted token (token sandbox)\n  network: only the egress proxy was reachable\n  dns: not proved: x\n  hidden folder: not writable\n  tree: writable",
            // A DNS line that passed is no DNS failure.
            "sandbox selfcheck: failed\n  backend: Windows sandbox user\n  network: only the egress proxy was reachable\n  dns: confined (x)\n  hidden folder: not writable\n  tree: writable",
        ] {
            let mut ex = Fake {
                selfcheck_code: 1,
                selfcheck_out: Some(out),
                ..Fake::default()
            };
            let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
            assert!(p.stopped_at_gate, "{out}");
            assert_eq!(p.dns_failed, None, "{out}");
            assert!(!p.summary().contains("DNS"), "{}", p.summary());
        }
    }

    /// A probe that did not start as the sandbox user fails the gate, but the
    /// re-run still runs and the summary and report say the gate failed.
    #[test]
    fn a_probe_that_did_not_run_on_the_sandbox_user_still_reruns() {
        let out = "  launch: spawn lock, every spawn in this process: taken 1\nError: sandbox selfcheck failed (backend: Windows sandbox user): the probe exited with status -1073741502\n      0 [main] bash (492) C:\\Program Files\\Git\\usr\\bin\\bash.exe: *** fatal error";
        let mut ex = Fake {
            selfcheck_code: 1,
            selfcheck_out: Some(out),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(!p.stopped_at_gate);
        assert!(ex.calls.iter().any(|c| c.starts_with("rerun")));
        let line = p.probe_failed.clone().expect("the error line is kept");
        assert!(
            line.contains("the probe exited with status -1073741502"),
            "{line}"
        );
        assert!(p.summary().contains("GATE FAILED"), "{}", p.summary());
        let md = render(&baseline(), &[], &p);
        assert!(md.contains("**GATE FAILED:**"), "{md}");
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
    }

    /// The same probe failure under the token sandbox, or a probe that ran
    /// and printed its lines, still closes the gate.
    #[test]
    fn a_probe_failure_off_the_sandbox_user_or_with_results_still_stops() {
        for out in [
            "Error: sandbox selfcheck failed (backend: Windows restricted token (token sandbox)): the probe exited with status -1073741502",
            "sandbox selfcheck: failed\n  backend: Windows sandbox user\n  network: a non-proxy address was reachable\nError: sandbox selfcheck failed (backend: Windows sandbox user): network",
        ] {
            let mut ex = Fake {
                selfcheck_code: 1,
                selfcheck_out: Some(out),
                ..Fake::default()
            };
            let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
            assert!(p.stopped_at_gate, "{out}");
            assert_eq!(p.probe_failed, None, "{out}");
        }
    }

    #[test]
    fn a_passing_gate_runs_the_selected_binaries_and_the_checks() {
        let mut ex = Fake::default();
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(!p.stopped_at_gate);
        let reran: Vec<&str> = p.reruns.iter().map(|r| r.result.id.as_str()).collect();
        assert_eq!(
            reran,
            vec!["kgraf-sandbox/test/escape_windows", "kgraf-cli/test/worker"]
        );
        assert!(ex.calls.contains(&"e2e".to_owned()));
        // The order of the steps.
        let pos = |s: &str| ex.calls.iter().position(|c| c.starts_with(s)).unwrap();
        assert!(pos("sandbox setup") < pos("sandbox status"));
        assert!(pos("sandbox selfcheck") < pos("rerun"));
        assert!(pos("e2e") < pos("sandbox remove"));
        assert!(p.status_check.as_ref().unwrap().ok);
    }

    #[test]
    fn removal_runs_after_a_failed_setup_and_ignores_ctrl_c() {
        let mut ex = Fake {
            setup_code: 1,
            selfcheck_code: 1,
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(false), &|| true);
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
        assert!(ex.calls.contains(&"sandbox status cancel=false".to_owned()));
        // Not elevated: the report says what to do.
        let md = render(&baseline(), &[], &p);
        assert!(md.contains("elevated"), "{md}");
        assert!(md.contains("UAC"));
    }

    #[test]
    fn cancel_skips_the_rerun_but_not_the_removal() {
        let mut ex = Fake::default();
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| true);
        assert!(p.cancelled);
        assert!(p.reruns.is_empty() && p.e2e.is_empty());
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
    }

    #[test]
    fn a_failed_preflight_changes_nothing() {
        let mut ex = Fake {
            preflight: Some("a sandbox user is already set up".into()),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert_eq!(ex.calls, vec!["preflight"]);
        assert!(p.summary().starts_with("not run:"));
    }

    #[test]
    fn the_right_binaries_are_chosen() {
        let mut jobserver = bin("a/test/one", &[("t", "the cargo jobserver is closed")]);
        let msys = bin(
            "b/test/two",
            &[("t", "NtCreateDirectoryObject: Access denied")],
        );
        let schannel = bin("c/test/three", &[("t", "curl: SEC_E_NO_CREDENTIALS")]);
        let other = bin("d/test/four", &[("t", "assertion failed: left == right")]);
        let mut hung = bin("e/test/five", &[]);
        hung.timed_out = true;
        hung.exit_code = None;
        hung.tail = Some("fatal: Git Bash could not start".into());
        // A clean binary whose output once named a blocker is not re-run.
        let clean = bin("f/test/six", &[]);
        jobserver.id = "a/test/one#core".into();
        let sel = select(&[
            jobserver,
            msys,
            schannel,
            other,
            hung,
            clean,
            bin("kgraf-sandbox/test/tls_windows", &[]),
            bin("kgraf-sandbox/test/shell_windows#core", &[]),
            bin("kgraf-sandbox/test/startup_windows", &[]),
            bin("kgraf-sandbox/lib/kgraf_sandbox", &[]),
        ]);
        let ids: Vec<&str> = sel.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "a/test/one#core",
                "b/test/two",
                "c/test/three",
                "e/test/five",
                "kgraf-sandbox/test/tls_windows",
                "kgraf-sandbox/test/shell_windows#core",
                "kgraf-sandbox/test/startup_windows",
                "kgraf-sandbox/lib/kgraf_sandbox",
            ]
        );
        assert!(sel[0].why.contains("cargo jobserver"));
        assert!(sel[2].why.contains("Schannel"));
    }

    #[test]
    fn every_slot_users_stored_password_must_log_on() {
        let with_slot = STATUS_OK.replace(
            "  present  stored password of kgraf-a\n",
            "  present  slot00 sandbox user kgraf-s\n  present  online00 sandbox user kgraf-o\n  \
             present  stored password of kgraf-a\n  present  stored password of kgraf-s\n  \
             present  stored password of kgraf-o\n",
        );
        let c = check_ready(&with_slot, Some(0));
        assert!(c.ok, "{:?}", c.problems);
        assert_eq!((c.logon_works, c.users), (4, 4));
        // A setup whose slot users were made but whose passwords were not
        // stored is not ready, though the two fixed passwords log on.
        let short = with_slot
            .replace("  present  stored password of kgraf-s\n", "")
            .replace("  present  stored password of kgraf-o\n", "");
        let c = check_ready(&short, Some(0));
        assert!(!c.ok);
        assert!(
            c.problems.iter().any(|p| p.contains("2 of 4")),
            "{:?}",
            c.problems
        );
    }

    #[test]
    fn status_and_removal_are_read_from_the_lines() {
        assert!(check_ready(STATUS_OK, Some(0)).ok);
        assert_eq!(check_ready(STATUS_OK, Some(0)).logon_works, 2);
        let bad = STATUS_OK.replace(
            "present  stored password of kgraf-b",
            "WRONG    stored password of kgraf-b",
        );
        let c = check_ready(&bad, Some(0));
        assert!(
            !c.ok && c.problems.iter().any(|p| p.contains("kgraf-b")),
            "{c:?}"
        );
        assert!(!check_ready("", Some(0)).ok);
        assert!(check_gone(STATUS_GONE).ok);
        assert!(!check_gone(STATUS_OK).ok);
        assert!(!check_gone("").ok);
    }

    /// A DNS filter Windows refused at setup is a `FAILED` status line: a
    /// problem after setup, never read past as an unknown word, and a line
    /// that is not `MISSING` after a removal.
    #[test]
    fn a_failed_status_line_is_a_problem() {
        let failed = format!(
            "{STATUS_OK}\n  FAILED   DNS filter on the name resolution cache layer, IPv4 x: \
             Windows refused it at setup"
        );
        let lines = parse_status(&failed);
        assert!(lines.iter().any(|l| l.word == "FAILED"), "{lines:?}");
        let c = check_ready(&failed, Some(1));
        assert!(
            !c.ok
                && c.problems
                    .iter()
                    .any(|p| p.starts_with("FAILED DNS filter")),
            "{c:?}"
        );
        assert!(!check_gone(&failed).ok);
    }

    #[test]
    fn the_comparison_reports_passed_failed_and_hung() {
        let first = vec![bin(
            "x/test/y",
            &[
                ("t::passes", "boom"),
                ("t::fails", "boom"),
                ("t::hangs", "boom"),
            ],
        )];
        let mut r = bin("x/test/y", &[("t::fails", "still broken")]);
        r.tests.push(crate::libtest::TestOutcome {
            name: "t::passes".into(),
            outcome: Outcome::Passed,
        });
        r.timed_out = true;
        r.open_test = Some("t::hangs".into());
        r.tail = Some("stuck".into());
        let cmp = compare(
            &first,
            &[Rerun {
                why: "w".into(),
                result: r,
            }],
        );
        let after: Vec<(&str, &str)> = cmp
            .iter()
            .map(|c| (c.test.as_str(), c.after.as_str()))
            .collect();
        assert_eq!(
            after,
            vec![
                ("t::passes", "passed"),
                ("t::fails", "failed"),
                ("t::hangs", "hung")
            ]
        );
        assert_eq!(cmp[1].tail, "still broken");
    }

    #[test]
    fn setup_passes_two_or_more_slots_explicitly() {
        let args = setup_args();
        assert_eq!(&args[..3], ["sandbox", "setup", "--slots"]);
        let n: u8 = args[3].parse().unwrap();
        assert!(n >= 2, "{n}");
        let mut ex = Fake::default();
        run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(
            ex.calls
                .iter()
                .any(|c| c == &format!("sandbox setup --slots {n} cancel=true")),
            "{:?}",
            ex.calls
        );
    }

    /// Every measurement line of the re-run is in the report, with its
    /// binary, test and time; a binary that lost its summary is named.
    #[test]
    fn the_report_lists_the_measurements_of_the_rerun() {
        use crate::report::Measurement;
        let mut a = bin("kgraf-sandbox/test/escape_windows", &[]);
        a.measurements = vec![
            Measurement {
                test: Some("a_unc_path_outside_loopback_is_measured_not_blocked".into()),
                secs: Some(61.24),
                line: "measurement: 192.0.2.1 as the offline slot user: exit 53, x|y".into(),
            },
            Measurement {
                test: None,
                secs: None,
                line: "measurement: loose".into(),
            },
        ];
        let mut cut = bin("kgraf-sandbox/test/tls_windows", &[]);
        cut.tail = Some("killed".into());
        let reruns = vec![
            Rerun {
                why: "w".into(),
                result: a,
            },
            Rerun {
                why: "w".into(),
                result: cut,
            },
        ];
        let md = render_measurements(&reruns);
        assert!(md.contains("#### Measurements of the re-run"), "{md}");
        assert!(
            md.contains(
                "| kgraf-sandbox/test/escape_windows | \
                 a_unc_path_outside_loopback_is_measured_not_blocked | 61.2 s | "
            ),
            "{md}"
        );
        assert!(md.contains(r"exit 53, x\|y |"), "{md}");
        assert!(
            md.contains("| unknown | unknown | measurement: loose |"),
            "{md}"
        );
        assert!(
            md.contains("missing: kgraf-sandbox/test/tls_windows"),
            "{md}"
        );
        assert!(render_measurements(&[]).contains("No re-run test printed"));
        // The section is part of the phase's report.
        let mut ex = Fake::default();
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(render(&baseline(), &[], &p).contains("#### Measurements of the re-run"));
    }

    #[test]
    fn the_report_shows_both_sides_and_the_removal() {
        let first = vec![bin(
            "kgraf-sandbox/test/escape_windows",
            &[("e::one", "SEC_E_NO_CREDENTIALS")],
        )];
        let mut ex = Fake {
            passing: vec!["e::one".into()],
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &first, Some(true), &|| false);
        let md = render(&first, &[], &p);
        for want in [
            "## Sandbox user phase",
            "### (b0) selfcheck before setup (not a gate)",
            "### (b) setup",
            "### (c) status",
            "### (d) selfcheck (gate)",
            "Baseline passed",
            "Sandbox user passed",
            "| kgraf-sandbox/test/escape_windows | e::one | failed | passed |",
            "### (f) remove",
            "Confirmed: nothing of the sandbox user is left",
        ] {
            assert!(md.contains(want), "missing {want:?} in\n{md}");
        }
    }
}
