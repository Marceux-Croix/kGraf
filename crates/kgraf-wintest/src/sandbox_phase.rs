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

use crate::budget::{NotRun, not_run_reason};
use crate::e2e::{Check, Status};
use crate::libtest::Outcome;
use crate::report::BinaryResult;
use kgraf_sandbox::sandbox_user::probe;
use kgraf_sandbox::sandbox_user::session0::Proof;

pub const SETUP_LIMIT: Duration = Duration::from_secs(300);
pub const STATUS_LIMIT: Duration = Duration::from_secs(120);
pub const SELFCHECK_LIMIT: Duration = Duration::from_secs(300);
/// One run of the named-object probe through a launch path.
pub const PROBE_LIMIT: Duration = Duration::from_secs(180);

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
/// The error line of a selfcheck that failed on the sandbox user and printed
/// no result lines: the backend is the sandbox user, and selfcheck stopped at
/// its probe with one error line and none of its `network:`, `dns:`, folder
/// or tree lines. The probe may have run and failed one of its steps (in
/// a hosted Windows self-test it reached the origin directly, "a direct
/// connection to the origin worked"), or may not have finished at all (Git
/// Bash stopped at its start); [`probe_words`] says which the line shows.
/// `None` for any other output.
///
/// The re-run (e) still runs then. Its tests start cargo, Git Bash and curl
/// as the sandbox user one by one, which is the only evidence of which of
/// them fail there; stopping at the gate would throw that away and leave
/// only the probe's one error. The phase reports the gate as failed.
fn failure_under_user(step: &StepRun) -> Option<String> {
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

/// What a [`failure_under_user`] line says about the probe, in a sentence
/// for the report. Each of the probe's steps has its own error text in
/// selfcheck (`explain` in crates/kgraf-cli/src/sandbox_cmd.rs): a line that
/// names one says the probe ran and failed that step; a line that gives only
/// an exit status says the probe did not finish its steps.
fn probe_words(line: &str) -> &'static str {
    const STEPS: [&str; 4] = [
        "the command could not write in its tree",
        "the hidden folder was writable",
        "the command did not reach the allowed origin through the proxy",
        "a direct connection to the origin worked",
    ];
    if STEPS.iter().any(|step| line.contains(step)) {
        "The probe ran as the sandbox user and failed the step the line names."
    } else {
        "The probe did not finish its steps: the line gives its exit status, not a step."
    }
}

/// The report's first line: the Windows session of the process that runs
/// kGraf, read with `ProcessIdToSessionId` by the sandbox crate's
/// `session_of_current_process`. Session 0 is the session Windows gives its
/// services. If kGraf itself runs there, as it can on a hosted machine, the
/// launcher service path and the owner-session fallback name the same
/// machine-wide object directory, and no result below separates the two.
pub fn session_line(session: Result<u32, String>) -> String {
    const SOURCE: &str = "Read by the self-test runner with ProcessIdToSessionId \
                          (`session_of_current_process` in the sandbox crate).";
    match session {
        Ok(0) => format!(
            "**Windows session of the process that runs kGraf: 0.** This is the session Windows \
             gives its services. Both launch paths then name the same machine-wide object \
             directory, so read every result below that way: a named object made here does not \
             show that a command works from an interactive session. {SOURCE}"
        ),
        Ok(n) => format!(
            "**Windows session of the process that runs kGraf: {n}.** Session 0 is the session \
             of Windows services. The launcher service path reaches the machine-wide object \
             directory from session 0. The fallback reaches the directory of session {n}. {SOURCE}"
        ),
        Err(e) => format!(
            "**Windows session of the process that runs kGraf: not read** ({e}). If it is \
             session 0, both launch paths name the same machine-wide object directory and every \
             result below must be read that way. {SOURCE}"
        ),
    }
}

#[cfg(windows)]
fn read_owner_session() -> Result<u32, String> {
    kgraf_sandbox::sandbox_user::windows::named_object_dir::session_of_current_process()
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn read_owner_session() -> Result<u32, String> {
    Err("this is not a Windows host".into())
}

/// The lines of the steps' output that say which launch path is active and
/// what started each command: `named objects:` (`Active::line`, with what the
/// probe found) and `launch:` (the sandbox crate's launch record). Each line
/// is kept once, with the label of the first step that printed it.
pub fn launch_findings(p: &SandboxPhase) -> Vec<(String, String)> {
    let steps = [
        &p.selfcheck_before,
        &p.setup,
        &p.status,
        &p.selfcheck,
        &p.remove,
        &p.status_after,
    ];
    let mut out: Vec<(String, String)> = Vec::new();
    for s in steps.into_iter().flatten() {
        for l in s.output.lines().map(str::trim) {
            let wanted = l.starts_with("named objects: ") || l.starts_with("launch: ");
            if wanted && !out.iter().any(|(_, seen)| seen == l) {
                out.push((s.label.clone(), l.to_owned()));
            }
        }
    }
    out
}

fn render_launch_path(p: &SandboxPhase) -> String {
    let mut md = String::from("### Launch path\n\n");
    let found = launch_findings(p);
    let (named, launch): (Vec<_>, Vec<_>) = found
        .iter()
        .partition(|(_, l)| l.starts_with("named objects: "));
    let block = |rows: &[&(String, String)]| {
        crate::report::fence(
            "text",
            &rows
                .iter()
                .map(|(label, l)| format!("[{label}] {l}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    if named.is_empty() {
        md.push_str(
            "No step printed a `named objects:` line, so the report does not say which launch \
             path is active or what the probe found.\n\n",
        );
    } else {
        md.push_str(
            "The active launch path and what the probe found (`named objects:` lines):\n\n",
        );
        md.push_str(&block(&named));
        md.push('\n');
    }
    if launch.is_empty() {
        md.push_str("No step printed a `launch:` line.\n\n");
    } else {
        md.push_str("The launch lines the sandbox crate collects (`launch:` lines):\n\n");
        md.push_str(&block(&launch));
        md.push('\n');
    }
    md
}

/// The session of the owner's process and the access list (SDDL) of that
/// session's object directory, or why either could not be read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DirectoryRead {
    pub session: Result<u32, String>,
    pub access_list: Result<String, String>,
}

/// The sandbox crate's words for a probe outcome (`session0::Proof`).
pub fn proof_words(proof: &Proof) -> String {
    match proof {
        Proof::Made => "session0::Proof::Made (the command made both objects)".into(),
        Proof::Refused(why) => format!(
            "session0::Proof::Refused (the command ran and Windows refused a named object): {why}"
        ),
        Proof::CouldNotRun(why) => format!(
            "session0::Proof::CouldNotRun (the probe could not run, for a reason that is not a \
             refused named object): {why}"
        ),
    }
}

/// Which path a `named objects:` line names, as `Active::line` words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PathWord {
    Session0,
    Fallback,
    Neither,
}

impl PathWord {
    pub fn words(self) -> &'static str {
        match self {
            PathWord::Session0 => "session 0 (the launcher service)",
            PathWord::Fallback => "the fallback grant",
            PathWord::Neither => "neither (the owner's session, no grant)",
        }
    }
}

/// What a `named objects:` line (`session0::Active::line`) says: the path,
/// and the probe outcome. The outcome is `None` where no probe has run.
pub fn read_named_line(line: &str) -> (PathWord, Option<Proof>) {
    let path = if line.contains("start in session 0 through kGraf's launcher service") {
        PathWord::Session0
    } else if line.contains("which admits the sandbox group to its object directory") {
        PathWord::Fallback
    } else {
        PathWord::Neither
    };
    let proof = if line.contains(". Proved: a sandbox user made a named semaphore") {
        Some(Proof::Made)
    } else if let Some((_, why)) =
        line.split_once(". The proof failed, a named object was refused: ")
    {
        Some(Proof::Refused(why.to_owned()))
    } else if let Some((_, why)) = line.split_once("says nothing about named objects. ") {
        Some(Proof::CouldNotRun(why.to_owned()))
    } else {
        None
    };
    (path, proof)
}

/// The last `named objects:` line of a step's output.
fn named_line_of(step: &Option<StepRun>) -> Option<String> {
    step.as_ref()?
        .output
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("named objects: "))
        .map(str::to_owned)
}

/// The `named objects:` line the first probe left: setup's, else status's.
pub fn first_named_line(p: &SandboxPhase) -> Option<String> {
    named_line_of(&p.setup).or_else(|| named_line_of(&p.status))
}

fn serialize_proof<S: serde::Serializer>(p: &Proof, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&proof_words(p))
}

/// The fallback the phase ran itself because the first probe was refused.
#[derive(Clone, Debug, Serialize)]
pub struct Fallback {
    pub before: DirectoryRead,
    pub grant: StepRun,
    pub after: DirectoryRead,
    /// `kgraf sandbox-named-object-probe --launch` after the grant.
    pub probe: StepRun,
    /// What the probe proved, from its exit status and output
    /// (`probe::proof_of_relay`, the rule setup uses too).
    #[serde(serialize_with = "serialize_proof")]
    pub proof: Proof,
    pub status: StepRun,
}

fn run_fallback(ex: &mut dyn Exec) -> Fallback {
    let before = ex.object_directory();
    let grant = ex.grant_named_objects();
    let after = ex.object_directory();
    let probe = ex.cli(
        "(b3) probe through the fallback",
        &[probe::SUBCOMMAND, probe::LAUNCH_FLAG],
        PROBE_LIMIT,
        true,
    );
    let proof = kgraf_sandbox::sandbox_user::probe::proof_of_relay(probe.code, &probe.output);
    let status = ex.cli(
        "(b4) status after the fallback",
        &["sandbox", "status"],
        STATUS_LIMIT,
        true,
    );
    Fallback {
        before,
        grant,
        after,
        probe,
        proof,
        status,
    }
}

/// Which path is in force at the end of setup and the fallback: session 0 or
/// the grant where a probe made the objects, else neither.
pub fn chosen_path(p: &SandboxPhase) -> String {
    let Some(line) = first_named_line(p) else {
        return "not known: no step printed a `named objects:` line".into();
    };
    let (path, proof) = read_named_line(&line);
    if proof == Some(Proof::Made) && path != PathWord::Neither {
        return path.words().into();
    }
    match &p.fallback {
        Some(f) if f.proof == Proof::Made => PathWord::Fallback.words().into(),
        _ => PathWord::Neither.words().into(),
    }
}

fn render_named_objects(p: &SandboxPhase) -> String {
    let mut md = String::from("### Named objects: the fallback and the path chosen\n\n");
    let acl = |r: &Result<String, String>| match r {
        Ok(s) => format!("`{s}`"),
        Err(e) => format!("not read ({e})"),
    };
    if let Some(f) = &p.fallback {
        md.push_str(
            "The first probe was refused (`session0::Proof::Refused`), so the phase ran the \
             fallback's elevated action itself: `kgraf sandbox elevated --action \
             grant-named-objects`, started from the runner the way setup starts its own \
             elevated step.\n\n",
        );
        md.push_str(&match &f.before.session {
            Ok(n) => format!(
                "1. Windows session the elevated step ran in: {n}. The step starts as the \
                 same user from the runner, so this is the runner's session.\n"
            ),
            Err(e) => format!("1. Windows session the elevated step ran in: not read ({e}).\n"),
        });
        md.push_str(&format!(
            "2. Access list of that session's object directory before the grant: {}\n",
            acl(&f.before.access_list)
        ));
        md.push_str(&format!(
            "3. Access list after the grant: {}\n",
            acl(&f.after.access_list)
        ));
    } else {
        md.push_str(
            "The fallback grant did not run: the first probe was not refused, or setup or \
             elevation was missing.\n\n",
        );
    }
    let step = if p.fallback.is_some() { 4 } else { 1 };
    md.push_str(&format!("{step}. Path chosen: {}\n", chosen_path(p)));
    md.push_str(&format!(
        "{}. Probe outcome for each path tried:\n",
        step + 1
    ));
    match first_named_line(p).map(|l| read_named_line(&l)) {
        Some((path, Some(proof))) => md.push_str(&format!(
            "   - {} (setup's probe): {}\n",
            path.words(),
            proof_words(&proof)
        )),
        Some((path, None)) => md.push_str(&format!(
            "   - {} (setup's probe): no probe has run\n",
            path.words()
        )),
        None => md.push_str("   - setup's probe: no `named objects:` line was printed\n"),
    }
    if let Some(f) = &p.fallback {
        md.push_str(&format!(
            "   - {} (probe after the grant): {}\n",
            PathWord::Fallback.words(),
            proof_words(&f.proof)
        ));
    }
    md.push('\n');
    if let Some(f) = &p.fallback {
        md.push_str("The grant step:\n\n");
        md.push_str(&step_block(&Some(f.grant.clone())));
        md.push_str(&step_block(&Some(f.probe.clone())));
        md.push_str(&step_block(&Some(f.status.clone())));
    }
    md
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
    /// `--only` named every binary of the baseline, so the re-run takes each
    /// of them, whatever the baseline found ([`select`]).
    fn every_binary_named(&self) -> bool {
        false
    }
    fn e2e(&mut self) -> Vec<Check>;
    /// The fallback's elevated action, `kgraf sandbox elevated --action
    /// grant-named-objects`, started from the owner's side the way setup
    /// starts its own (`setup::grant_named_objects`): the output is the events
    /// the elevated step sent back.
    fn grant_named_objects(&mut self) -> StepRun;
    /// The Windows session of the owner's process and the access list of that
    /// session's object directory. The elevated step starts as the same user
    /// from this process, so it runs in this session.
    fn object_directory(&mut self) -> DirectoryRead;
    /// Undoes what `preflight` placed; returns notes.
    fn teardown(&mut self) -> Vec<String>;
    /// Time left under the run's budget, less the reserve for the removal.
    /// `None`: the run has no budget.
    fn time_left(&self) -> Option<Duration> {
        None
    }
    /// How long re-running the binary `id` is expected to take.
    fn rerun_expected(&self, _id: &str) -> Duration {
        Duration::from_secs(30)
    }
}

/// Time the setup, the status check and the gate need before the phase may
/// start: with less left, setup would be cut off half done.
pub const PHASE_START_SECS: u64 = 300;
/// Time the end-to-end checks of the re-run are expected to take.
pub const E2E_EXPECTED_SECS: u64 = 90;

/// Runs one `kgraf` step under the budget. A step that honors cancel is
/// capped to the time left and is not started when none is left; the
/// removal steps never are. The flag says the budget cut the step. A cut
/// step is recorded in `p` ([`SandboxPhase::stopped_at_budget`] or
/// [`SandboxPhase::not_run`]).
fn budgeted_cli(
    ex: &mut dyn Exec,
    p: &mut SandboxPhase,
    label: &str,
    args: &[&str],
    limit: Duration,
    honor_cancel: bool,
) -> (StepRun, bool) {
    let left = if honor_cancel { ex.time_left() } else { None };
    if left.is_some_and(|l| l.is_zero()) {
        p.not_run.push(NotRun {
            what: label.to_owned(),
            why: not_run_reason(limit, Duration::ZERO),
        });
        let step = StepRun {
            label: label.to_owned(),
            command: format!("kgraf {}", args.join(" ")),
            start_error: Some("not run: budget".into()),
            ..StepRun::default()
        };
        return (step, true);
    }
    let capped = left.filter(|l| *l < limit);
    let step = ex.cli(label, args, capped.unwrap_or(limit), honor_cancel);
    let cut = capped.is_some() && step.timed_out;
    if cut {
        p.stopped_at_budget.push(label.to_owned());
    }
    (step, cut)
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
    /// The status after removal printed the launcher service as MISSING.
    pub service_gone: bool,
    /// The status after removal shows no fallback grant on the owner's
    /// session object directory.
    pub grant_gone: bool,
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
    // A service or a grant left behind is a defect, and a status that does
    // not print the line cannot confirm it is gone (fail loud).
    match lines
        .iter()
        .find(|l| l.item.starts_with("launcher service "))
    {
        Some(l) => g.service_gone = l.word == "MISSING",
        None => g.left.push(
            "the status after removal printed no launcher service line, so the service is \
             not confirmed gone"
                .into(),
        ),
    }
    match lines.iter().find(|l| {
        l.item
            .contains("named objects in the owner's session object directory")
            || l.item.starts_with("no fallback grant")
    }) {
        Some(l) => {
            g.grant_gone = l.word == "MISSING" || l.item.starts_with("no fallback grant");
        }
        None => g.left.push(
            "the status after removal printed no fallback grant line, so the grant is not \
             confirmed gone"
                .into(),
        ),
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
/// baseline order, each once. With `every_named` (`--only`), the person named
/// each binary the baseline ran, so each runs again whatever it did there.
pub fn select(first: &[BinaryResult], every_named: bool) -> Vec<Selection> {
    first
        .iter()
        .filter_map(|b| {
            let why = if every_named {
                "named by --only".to_owned()
            } else if ALWAYS.contains(&base_id(&b.id)) {
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
    /// (d) failed on the sandbox user and printed no result lines (the
    /// error line, [`failure_under_user`]); (e) ran anyway.
    pub failed_under_user: Option<String>,
    pub cancelled: bool,
    /// Which Windows session the process that runs kGraf is in
    /// ([`session_line`]). The report says it before any verdict.
    pub owner_session: Option<String>,
    /// The fallback grant the phase ran because the first probe was refused.
    pub fallback: Option<Fallback>,
    pub reruns: Vec<Rerun>,
    /// Re-runs and steps the time budget kept from starting.
    pub not_run: Vec<NotRun>,
    /// Steps and re-runs that were running when the time budget ended, and
    /// were stopped.
    pub stopped_at_budget: Vec<String>,
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
        let gate = if !self.not_run.is_empty() || !self.stopped_at_budget.is_empty() {
            "INCOMPLETE: the time budget ran out, so some of the re-run did not run or was stopped"
        } else if self.stopped_at_gate {
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
        let probe = if self.failed_under_user.is_some() {
            "; GATE FAILED: selfcheck failed on the sandbox user and printed no result lines \
             (see the gate)"
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
    let mut p = SandboxPhase {
        owner_session: Some(session_line(read_owner_session())),
        ..SandboxPhase::default()
    };
    if elevated == Some(false) {
        p.notes.push(NEEDS_ELEVATION.into());
    }
    // With too little time for setup and the gate, nothing is started: a
    // setup cut off half done would need the removal to clean it.
    if let Some(left) = ex.time_left()
        && left < Duration::from_secs(PHASE_START_SECS)
    {
        p.skipped = Some(format!(
            "budget: setup and the gate need about {PHASE_START_SECS} s, {} s left",
            left.as_secs()
        ));
        return p;
    }
    if let Err(why) = ex.preflight() {
        p.skipped = Some(why);
        return p;
    }

    // From here on, (f) always runs.
    let (before, _) = budgeted_cli(
        ex,
        &mut p,
        "(b0) selfcheck before setup",
        &["sandbox", "selfcheck"],
        SELFCHECK_LIMIT,
        true,
    );
    p.selfcheck_before = Some(before);
    let args = setup_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (setup, _) = budgeted_cli(ex, &mut p, "(b) setup", &args, SETUP_LIMIT, true);
    if !setup.ok() {
        p.notes.push(format!("Setup {}.", setup.how()));
        if elevated.is_none() {
            p.notes.push(NEEDS_ELEVATION.into());
        }
    }
    p.setup = Some(setup);

    let (status, _) = budgeted_cli(
        ex,
        &mut p,
        "(c) status",
        &["sandbox", "status"],
        STATUS_LIMIT,
        true,
    );
    p.status_check = Some(check_ready(&status.output, status.code));
    p.status = Some(status);

    // The first probe was refused: exercise the fallback here, unattended.
    let refused = first_named_line(&p)
        .is_some_and(|l| matches!(read_named_line(&l).1, Some(Proof::Refused(_))));
    if refused && elevated != Some(false) {
        p.fallback = Some(run_fallback(ex));
    }

    let (gate, gate_cut) = budgeted_cli(
        ex,
        &mut p,
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
    p.failed_under_user = if passed {
        None
    } else {
        failure_under_user(&gate)
    };
    p.selfcheck = Some(gate);
    if gate_cut {
        // The budget ended in the gate: that is not a failed gate, and the
        // re-run does not start. (f) still runs.
    } else if !passed && p.dns_failed.is_none() && p.failed_under_user.is_none() {
        p.stopped_at_gate = true;
    } else {
        for s in select(first, ex.every_binary_named()) {
            if cancelled() {
                p.cancelled = true;
                break;
            }
            let expected = ex.rerun_expected(&s.id);
            if let Some(left) = ex.time_left()
                && (left.is_zero() || left < expected)
            {
                p.not_run.push(NotRun {
                    what: format!("sandbox user re-run of {}", s.id),
                    why: not_run_reason(expected, left),
                });
                continue;
            }
            let result = ex.rerun(&s.id);
            if result.budget_stopped {
                p.stopped_at_budget
                    .push(format!("sandbox user re-run of {}", s.id));
            }
            p.reruns.push(Rerun { why: s.why, result });
        }
        let e2e_expected = Duration::from_secs(E2E_EXPECTED_SECS);
        if cancelled() {
            p.cancelled = true;
        } else if let Some(left) = ex.time_left()
            && (left.is_zero() || left < e2e_expected)
        {
            p.not_run.push(NotRun {
                what: "sandbox user end-to-end checks".into(),
                why: not_run_reason(e2e_expected, left),
            });
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
            .unwrap_or_else(|| {
                if r.budget_stopped {
                    "stopped at the budget"
                } else if r.timed_out {
                    "hung"
                } else {
                    "failed"
                }
                .into()
            });
        return (how, tail);
    }
    match r.tests.iter().find(|t| t.name == test) {
        Some(t) if t.outcome == Outcome::Passed => ("passed".into(), "passed".into()),
        Some(_) => ("not reached".into(), r.tail.clone().unwrap_or_default()),
        None => ("not reached".into(), r.tail.clone().unwrap_or_default()),
    }
}

/// The forms of `STATUS_DLL_INIT_FAILED` (0xC0000142) a failed process
/// leaves in a test's output: hex, signed and unsigned decimal. Under the
/// token sandbox Git Bash's programs and rustc end with it before they run
/// (msys cannot make its object directory, rustc its jobserver).
const DLL_INIT_FAILED: &[&str] = &["0xc0000142", "-1073741502", "3221225794"];

/// The token sandbox limit a failed test's own output names: a [`blocker`],
/// or `STATUS_DLL_INIT_FAILED`. Only for [`expected_under_token_sandbox`],
/// which also needs the test to pass as the sandbox user; the re-run
/// selection reads [`blocker`] alone.
pub fn token_sandbox_marker(text: &str) -> Option<&'static str> {
    blocker(text).or_else(|| {
        let lower = text.to_lowercase();
        DLL_INIT_FAILED
            .iter()
            .any(|m| lower.contains(m))
            .then_some("STATUS_DLL_INIT_FAILED (0xC0000142)")
    })
}

/// A baseline failure that the token sandbox explains: the test passed when
/// it ran again as the sandbox user, and its baseline output names a limit
/// of the token sandbox that the sandbox user lifts
/// ([`token_sandbox_marker`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExpectedFailure {
    pub binary: String,
    pub test: String,
    /// The marker its baseline output carries, as the report names it.
    pub marker: String,
}

/// The baseline failures expected under the token sandbox. A failure counts
/// here only when both hold: (a) the same test passed in the sandbox user
/// re-run, a first-run pass and not a solo re-run's, and (b) its own
/// baseline output carries a token sandbox marker. Every other baseline
/// failure still counts, and so does one the baseline counted apart for a
/// missing tool. A hung or crashed baseline binary is not a test failure
/// and is never classified here.
pub fn expected_under_token_sandbox(
    first: &[BinaryResult],
    reruns: &[Rerun],
) -> Vec<ExpectedFailure> {
    let mut out = Vec::new();
    for rr in reruns {
        let Some(a) = first.iter().find(|b| b.id == rr.result.id) else {
            continue;
        };
        for f in &a.failures {
            if f.class != "kgraf_defect_suspected" {
                continue;
            }
            let passed = rr
                .result
                .tests
                .iter()
                .any(|t| t.name == f.test && t.outcome == Outcome::Passed)
                && !rr.result.failures.iter().any(|g| g.test == f.test);
            if let (true, Some(marker)) = (passed, token_sandbox_marker(&f.tail)) {
                out.push(ExpectedFailure {
                    binary: a.id.clone(),
                    test: f.test.clone(),
                    marker: marker.to_owned(),
                });
            }
        }
    }
    out
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
    // First, before any verdict: both launch paths read one directory when
    // kGraf itself runs in session 0.
    if let Some(line) = &p.owner_session {
        md.push_str(line);
        md.push_str("\n\n");
    }
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
    md.push_str(&render_named_objects(p));
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
    if let Some(line) = &p.failed_under_user {
        md.push_str(&format!(
            "**GATE FAILED:** `{line}`. Selfcheck failed on the sandbox user before it printed \
             its result lines, so its network, DNS and folder steps are not proved. {} The \
             re-run (e) ran anyway, so each program its tests start shows whether it runs as \
             the sandbox user.\n\n",
            probe_words(line)
        ));
    }

    md.push_str(&render_launch_path(p));

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
                if r.budget_stopped {
                    "stopped at the budget".to_owned()
                } else if r.timed_out {
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
        if !p.not_run.is_empty() || !p.stopped_at_budget.is_empty() {
            md.push_str("#### Cut by the time budget\n\n");
            for n in &p.not_run {
                md.push_str(&format!("- {}: {}\n", n.what, n.why));
            }
            for what in &p.stopped_at_budget {
                md.push_str(&format!("- {what}: stopped at the budget\n"));
            }
            md.push('\n');
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
        md.push_str(&format!(
            "Launcher service after the removal: {}. Fallback grant after the removal: {}.\n\n",
            if g.service_gone {
                "gone"
            } else {
                "STILL THERE or not confirmed"
            },
            if g.grant_gone {
                "gone"
            } else {
                "STILL THERE or not confirmed"
            },
        ));
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
            budget_stopped: false,
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

    const ACL_BEFORE: &str = "O:SYG:SYD:(A;;0xf0003;;;SY)";
    const ACL_AFTER: &str = "O:SYG:SYD:(A;;0xf0003;;;SY)(A;;0x2000f;;;S-1-5-21-1-2-3-1001)";

    const STATUS_OK: &str = "  present  Windows Firewall on for every profile\n  present  local firewall rules take effect\n  present  offline sandbox user kgraf-a\n  present  online sandbox user kgraf-b\n  present  stored password of kgraf-a\n  present  stored password of kgraf-b\nThe Windows sandbox user is set up.";
    const STATUS_GONE: &str = "  present  Windows Firewall on for every profile\n  present  local firewall rules take effect\n  MISSING  offline sandbox user kgraf-a\n  MISSING  online sandbox user kgraf-b\n  MISSING  stored passwords\n  MISSING  launcher service kgraf-launcher-00ff\n  MISSING  sandbox group may make named objects in the owner's session object directory (fallback, needed because the launcher service is not installed; the entry is absent or could not be read)\nThe Windows sandbox user is not fully set up: run `kgraf sandbox setup`.";

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
        /// What setup prints; `None`: "setup output".
        setup_out: Option<String>,
        /// What the probe after the grant prints, and its exit status.
        probe_out: String,
        probe_code: i32,
        /// The time budget left, less the reserve; `None`: no budget.
        left: Option<Duration>,
        /// What each `kgraf` step that honors cancel takes from `left`.
        spend_per_cli: Duration,
        /// Expected seconds of a re-run, by binary id; 30 when absent.
        expected: Vec<(&'static str, u64)>,
        /// The re-run of this binary is stopped at the budget: it returns
        /// `budget_stopped` and leaves no time.
        stopped_rerun: Option<&'static str>,
        /// The gate runs out of time (a timeout under a capped limit).
        gate_times_out: bool,
        /// The limit each `kgraf` step was given.
        limits: Vec<(String, Duration)>,
    }

    impl Exec for Fake {
        fn time_left(&self) -> Option<Duration> {
            self.left
        }
        fn rerun_expected(&self, id: &str) -> Duration {
            let secs = self
                .expected
                .iter()
                .find(|(i, _)| *i == id)
                .map_or(30, |(_, s)| *s);
            Duration::from_secs(secs)
        }
        fn preflight(&mut self) -> Result<(), String> {
            self.calls.push("preflight".into());
            self.preflight.clone().map_or(Ok(()), Err)
        }
        fn cli(&mut self, label: &str, args: &[&str], limit: Duration, honor: bool) -> StepRun {
            self.calls
                .push(format!("{} cancel={honor}", args.join(" ")));
            self.limits.push((label.to_owned(), limit));
            if honor {
                self.left = self.left.map(|l| l.saturating_sub(self.spend_per_cli));
            }
            let timed_out = self.gate_times_out && label.contains("(d)");
            let (code, out) = match args[1] {
                _ if args[0] == probe::SUBCOMMAND => (self.probe_code, self.probe_out.as_str()),
                "setup" => (
                    self.setup_code,
                    self.setup_out.as_deref().unwrap_or("setup output"),
                ),
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
                timed_out,
                output: out.into(),
                ..StepRun::default()
            }
        }
        fn rerun(&mut self, id: &str) -> BinaryResult {
            self.calls.push(format!("rerun {id}"));
            let mut b = bin(id, &[]);
            if self.stopped_rerun == Some(id) {
                b.budget_stopped = true;
                b.exit_code = None;
                self.left = Some(Duration::ZERO);
            }
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
        fn grant_named_objects(&mut self) -> StepRun {
            self.calls.push("grant-named-objects".into());
            StepRun {
                label: "(b2) fallback grant".into(),
                command: "kgraf sandbox elevated --action grant-named-objects".into(),
                code: Some(0),
                output: "granted".into(),
                ..StepRun::default()
            }
        }
        fn object_directory(&mut self) -> DirectoryRead {
            let n = self
                .calls
                .iter()
                .filter(|c| *c == "grant-named-objects")
                .count();
            self.calls.push("object-directory".into());
            DirectoryRead {
                session: Ok(2),
                access_list: Ok(if n == 0 { ACL_BEFORE } else { ACL_AFTER }.into()),
            }
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

    /// An item whose expected time does not fit in what is left is not
    /// started, and the report names it. The item before it, which fits,
    /// runs.
    #[test]
    fn a_rerun_that_would_overrun_the_budget_is_not_started_and_is_reported() {
        let mut ex = Fake {
            left: Some(Duration::from_secs(400)),
            expected: vec![
                ("kgraf-sandbox/test/escape_windows", 60),
                ("kgraf-cli/test/worker", 900),
            ],
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(
            ex.calls
                .contains(&"rerun kgraf-sandbox/test/escape_windows".to_owned()),
            "{:?}",
            ex.calls
        );
        assert!(
            !ex.calls.contains(&"rerun kgraf-cli/test/worker".to_owned()),
            "the 900 s item must not start with 400 s left: {:?}",
            ex.calls
        );
        let n = p
            .not_run
            .iter()
            .find(|n| n.what.contains("kgraf-cli/test/worker"))
            .expect("the item is reported");
        assert!(n.why.starts_with("not run: budget"), "{}", n.why);
        assert!(n.why.contains("900"), "{}", n.why);
        assert_eq!(p.reruns.len(), 1);
        // Cleanup always runs.
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
        assert!(p.summary().contains("INCOMPLETE"), "{}", p.summary());
        let md = render(&baseline(), &[], &p);
        assert!(md.contains("Cut by the time budget"), "{md}");
        assert!(md.contains("kgraf-cli/test/worker"), "{md}");
    }

    /// The budget ends while a re-run is running: that re-run is recorded as
    /// stopped, nothing after it starts, and the removal still runs.
    #[test]
    fn reaching_the_budget_stops_the_running_rerun_and_the_removal_still_runs() {
        let mut ex = Fake {
            left: Some(Duration::from_secs(4000)),
            stopped_rerun: Some("kgraf-sandbox/test/escape_windows"),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert_eq!(
            p.stopped_at_budget,
            vec!["sandbox user re-run of kgraf-sandbox/test/escape_windows".to_owned()]
        );
        assert!(p.reruns[0].result.budget_stopped);
        assert!(
            !ex.calls.contains(&"rerun kgraf-cli/test/worker".to_owned()),
            "{:?}",
            ex.calls
        );
        assert!(
            p.not_run
                .iter()
                .any(|n| n.what.contains("kgraf-cli/test/worker")),
            "{:?}",
            p.not_run
        );
        assert!(
            p.not_run.iter().any(|n| n.what.contains("end-to-end")),
            "the end-to-end checks need time too: {:?}",
            p.not_run
        );
        assert!(!ex.calls.contains(&"e2e".to_owned()));
        // The removal ran, after the stop, with no cancel and no cap.
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
        assert!(p.gone.as_ref().unwrap().ok);
        assert!(ex.calls.contains(&"teardown".to_owned()));
        let remove = ex.limits.iter().find(|(l, _)| l.contains("(f) remove"));
        assert_eq!(remove.map(|(_, l)| *l), Some(REMOVE_LIMIT));
    }

    /// A step the budget cuts short gets the time left as its limit, and a
    /// timeout under that limit is recorded as a stop at the budget, not as
    /// a failed gate.
    #[test]
    fn a_step_gets_the_time_left_as_its_limit_and_a_cut_gate_is_not_a_failed_gate() {
        let mut ex = Fake {
            left: Some(Duration::from_secs(400)),
            spend_per_cli: Duration::from_secs(120),
            gate_times_out: true,
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        let limit = |label: &str| {
            ex.limits
                .iter()
                .find(|(l, _)| l.contains(label))
                .map(|(_, l)| *l)
        };
        // 400 s left: (b0) keeps its 300 s; 280 s left: setup is capped.
        assert_eq!(limit("(b0)"), Some(SELFCHECK_LIMIT));
        assert_eq!(limit("(b) setup"), Some(Duration::from_secs(280)));
        assert!(
            p.stopped_at_budget
                .contains(&"(d) selfcheck (gate)".to_owned()),
            "{:?}",
            p.stopped_at_budget
        );
        assert!(!p.stopped_at_gate);
        assert!(p.reruns.is_empty());
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
    }

    /// With less time than setup and the gate need, the phase does not
    /// start and changes nothing.
    #[test]
    fn a_phase_with_too_little_time_does_not_start() {
        let mut ex = Fake {
            left: Some(Duration::from_secs(PHASE_START_SECS - 1)),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(p.skipped.as_deref().unwrap().starts_with("budget"));
        assert!(ex.calls.is_empty(), "{:?}", ex.calls);
        assert!(p.summary().starts_with("not run: budget"));
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
    /// re-run still runs and the summary and report say the gate failed, and
    /// that the probe did not finish its steps.
    #[test]
    fn a_probe_that_did_not_finish_on_the_sandbox_user_still_reruns() {
        let out = "  launch: spawn lock, every spawn in this process: taken 1\nError: sandbox selfcheck failed (backend: Windows sandbox user): the probe exited with status -1073741502\n      0 [main] bash (492) C:\\Program Files\\Git\\usr\\bin\\bash.exe: *** fatal error";
        let mut ex = Fake {
            selfcheck_code: 1,
            selfcheck_out: Some(out),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(!p.stopped_at_gate);
        assert!(ex.calls.iter().any(|c| c.starts_with("rerun")));
        let line = p.failed_under_user.clone().expect("the error line is kept");
        assert!(
            line.contains("the probe exited with status -1073741502"),
            "{line}"
        );
        assert!(p.summary().contains("GATE FAILED"), "{}", p.summary());
        assert!(
            p.summary().contains("printed no result lines"),
            "{}",
            p.summary()
        );
        let md = render(&baseline(), &[], &p);
        assert!(md.contains("**GATE FAILED:**"), "{md}");
        assert!(md.contains("The probe did not finish its steps"), "{md}");
        assert!(!md.contains("did not run on the sandbox user"), "{md}");
        assert!(ex.calls.contains(&"sandbox remove cancel=false".to_owned()));
    }

    /// A hosted Windows self-test: the probe ran as the sandbox user and failed
    /// its direct-connection step, and selfcheck stopped with that one line.
    /// The re-run still runs, and the report says the probe ran and which
    /// step failed, never that it did not run.
    #[test]
    fn a_probe_step_that_failed_on_the_sandbox_user_is_said_as_such() {
        let out = "  launch: CreateProcessWithLogonW calls 1 (0 failed): last 137.8 ms\nError: sandbox selfcheck failed (backend: Windows sandbox user): a direct connection to the origin worked; only the proxy should be reachable\n";
        let mut ex = Fake {
            selfcheck_code: 1,
            selfcheck_out: Some(out),
            ..Fake::default()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(!p.stopped_at_gate);
        assert!(ex.calls.iter().any(|c| c.starts_with("rerun")));
        let line = p.failed_under_user.clone().expect("the error line is kept");
        assert!(
            line.contains("a direct connection to the origin worked"),
            "{line}"
        );
        let md = render(&baseline(), &[], &p);
        assert!(
            md.contains("The probe ran as the sandbox user and failed the step the line names."),
            "{md}"
        );
        assert!(!md.contains("did not run on the sandbox user"), "{md}");
        assert!(!md.contains("did not finish"), "{md}");
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
            assert_eq!(p.failed_under_user, None, "{out}");
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
        let first = [
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
        ];
        // `--only` named every binary the baseline ran: each runs again.
        let named = select(&first, true);
        assert_eq!(named.len(), first.len());
        assert!(named.iter().all(|s| s.why == "named by --only"));
        let sel = select(&first, false);
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

    /// Only a baseline failure that both passed as the sandbox user and
    /// carries a token sandbox marker is expected; every other one counts.
    #[test]
    fn a_baseline_failure_is_expected_only_with_a_rerun_pass_and_a_marker() {
        const MSYS: &str = "bash.exe: *** fatal error - \
                            NtCreateDirectoryObject(\\BaseNamedObjects\\msys-2.0S5-1): 0xC0000022";
        const JOBSERVER: &str = "failed to create jobserver: Access is denied; cargo jobserver";
        const DLL_INIT: &str = "It said: $ true\n[exit code: 0xc0000142]";
        let mut first = vec![bin(
            "x/test/y",
            &[
                ("msys_passes", MSYS),
                ("jobserver_passes", JOBSERVER),
                ("dll_init_passes", DLL_INIT),
                ("plain_passes", "assertion `left == right` failed"),
                ("msys_fails_again", MSYS),
                ("msys_not_reached", MSYS),
                ("msys_solo_pass", MSYS),
                ("msys_needs_tool", MSYS),
            ],
        )];
        first[0].failures[7].class = "needs `cargo`, not on this machine".into();
        // A binary that was not run again keeps every failure.
        first.push(bin("z/test/w", &[("msys_elsewhere", MSYS)]));
        let mut r = bin("x/test/y", &[("msys_fails_again", "still broken")]);
        for name in [
            "msys_passes",
            "jobserver_passes",
            "dll_init_passes",
            "plain_passes",
            "msys_needs_tool",
        ] {
            r.tests.push(crate::libtest::TestOutcome {
                name: name.into(),
                outcome: Outcome::Passed,
            });
        }
        // Failed in the re-run's first run, passed only when run alone.
        r.tests.push(crate::libtest::TestOutcome {
            name: "msys_solo_pass".into(),
            outcome: Outcome::Failed,
        });
        r.failures.push(Failure {
            test: "msys_solo_pass".into(),
            class: "kgraf_defect_suspected".into(),
            tail: "passed".into(),
            solo: Some("passed".into()),
        });
        let reruns = [Rerun {
            why: "w".into(),
            result: r,
        }];
        let got = expected_under_token_sandbox(&first, &reruns);
        let names: Vec<(&str, &str)> = got
            .iter()
            .map(|e| (e.test.as_str(), e.marker.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("msys_passes", "Git Bash (msys)"),
                ("jobserver_passes", "cargo jobserver"),
                ("dll_init_passes", "STATUS_DLL_INIT_FAILED (0xC0000142)")
            ]
        );
        assert!(got.iter().all(|e| e.binary == "x/test/y"));
        // The decimal forms a Rust `ExitStatus` prints are the same status.
        for text in ["exit status: -1073741502", "ExitStatus(3221225794)"] {
            assert_eq!(
                token_sandbox_marker(text),
                Some("STATUS_DLL_INIT_FAILED (0xC0000142)")
            );
        }
        assert_eq!(token_sandbox_marker("assertion failed"), None);
        // The re-run selection does not read it.
        assert_eq!(blocker("[exit code: 0xc0000142]"), None);
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

    const NAMED_LINE: &str = "named objects: sandboxed commands start in session 0 through kGraf's launcher service, so cargo and rustc can make the named objects they need. Proved: a sandbox user made a named semaphore and an unprefixed mutex through this path";
    const LAUNCH_LINE: &str =
        "launch: the session0 path started a command: started (at 1700000000)";

    /// The report says which Windows session kGraf runs in before any
    /// verdict, and a session 0 is read as "both paths name one directory".
    #[test]
    fn the_report_names_the_owner_session_before_the_verdict() {
        let zero = session_line(Ok(0));
        assert!(zero.contains("kGraf: 0."), "{zero}");
        assert!(
            zero.contains("same machine-wide object directory"),
            "{zero}"
        );
        assert!(session_line(Ok(2)).contains("kGraf: 2."));
        let unread = session_line(Err("boom".into()));
        assert!(unread.contains("not read"), "{unread}");
        assert!(unread.contains("boom"), "{unread}");

        let p = SandboxPhase {
            owner_session: Some(zero),
            ..SandboxPhase::default()
        };
        let md = render(&baseline(), &[], &p);
        let at = |t: &str| {
            md.find(t)
                .unwrap_or_else(|| panic!("missing {t:?} in\n{md}"))
        };
        assert!(
            at("Windows session of the process that runs kGraf") < at("ran to the end"),
            "{md}"
        );
    }

    /// The `named objects:` and `launch:` lines of the steps' output reach
    /// the report in a section of their own, each once.
    #[test]
    fn the_report_lists_the_launch_path_and_the_launch_lines() {
        let step = |label: &str, out: String| {
            Some(StepRun {
                label: label.into(),
                command: format!("kgraf {label}"),
                code: Some(0),
                output: out,
                ..StepRun::default()
            })
        };
        let p = SandboxPhase {
            setup: step("(b) setup", format!("setup output\n  {NAMED_LINE}")),
            status: step("(c) status", format!("  {NAMED_LINE}")),
            selfcheck: step(
                "(d) selfcheck (gate)",
                format!("  {LAUNCH_LINE}\n  {NAMED_LINE}"),
            ),
            ..SandboxPhase::default()
        };
        let found = launch_findings(&p);
        assert_eq!(found.len(), 2, "each line once: {found:?}");
        assert_eq!(found[0], ("(b) setup".to_owned(), NAMED_LINE.to_owned()));
        let md = render(&baseline(), &[], &p);
        assert!(md.contains("### Launch path"), "{md}");
        assert!(md.contains(&format!("[(b) setup] {NAMED_LINE}")), "{md}");
        assert!(
            md.contains(&format!("[(d) selfcheck (gate)] {LAUNCH_LINE}")),
            "{md}"
        );

        let none = render(&baseline(), &[], &SandboxPhase::default());
        assert!(
            none.contains("No step printed a `named objects:` line"),
            "{none}"
        );
        assert!(none.contains("No step printed a `launch:` line"), "{none}");
    }

    fn named_line(
        path: kgraf_sandbox::sandbox_user::session0::LaunchPath,
        proof: Option<Proof>,
    ) -> String {
        use kgraf_sandbox::sandbox_user::session0::Active;
        Active {
            path,
            granted: path == kgraf_sandbox::sandbox_user::session0::LaunchPath::OwnerSession
                && proof == Some(Proof::Made),
            proved: proof,
            because: String::new(),
        }
        .line()
    }

    /// The phase reads the path and the probe outcome back from the words
    /// `Active::line` prints, for each outcome the sandbox crate has.
    #[test]
    fn a_named_objects_line_is_read_back_into_a_path_and_a_probe_outcome() {
        use kgraf_sandbox::sandbox_user::session0::LaunchPath::{OwnerSession, Session0};
        for (path, proof, want) in [
            (Session0, Some(Proof::Made), PathWord::Session0),
            (
                Session0,
                Some(Proof::Refused("denied".into())),
                PathWord::Session0,
            ),
            (
                Session0,
                Some(Proof::CouldNotRun("no exe".into())),
                PathWord::Session0,
            ),
            (OwnerSession, Some(Proof::Made), PathWord::Fallback),
            (OwnerSession, None, PathWord::Neither),
        ] {
            let line = named_line(path, proof.clone());
            let (got_path, got) = read_named_line(&line);
            assert_eq!(got_path, want, "{line}");
            assert_eq!(got, proof, "{line}");
        }
    }

    fn refused_setup() -> Fake {
        use kgraf_sandbox::sandbox_user::session0::LaunchPath::Session0;
        Fake {
            setup_out: Some(format!(
                "  {}",
                named_line(Session0, Some(Proof::Refused("Access is denied".into())))
            )),
            probe_out: "probe made semaphore\nprobe made mutex".into(),
            ..Fake::default()
        }
    }

    /// A refused first probe makes the phase run the fallback itself, in
    /// order: read the directory, grant, read it again, probe again.
    #[test]
    fn a_refused_probe_makes_the_phase_run_the_fallback_and_probe_again() {
        let mut ex = refused_setup();
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        let at = |c: &str| ex.calls.iter().position(|x| x.starts_with(c)).unwrap();
        assert!(
            at("object-directory") < at("grant-named-objects"),
            "{:?}",
            ex.calls
        );
        assert!(at("grant-named-objects") < at("sandbox-named-object-probe --launch"));
        assert!(p.fallback.as_ref().unwrap().proof == Proof::Made, "{p:?}");

        // A probe that made both objects never runs the fallback, and an
        // unelevated window never starts one.
        let mut ok = Fake::default();
        run_phase(&mut ok, &baseline(), Some(true), &|| false);
        assert!(!ok.calls.contains(&"grant-named-objects".to_owned()));
        let mut plain = refused_setup();
        let q = run_phase(&mut plain, &baseline(), Some(false), &|| false);
        assert!(q.fallback.is_none());
    }

    /// The report prints, in this order, the session, the access list before
    /// and after the grant, the path chosen and each probe outcome.
    #[test]
    fn the_report_prints_the_fallback_in_its_order() {
        let mut ex = refused_setup();
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        let md = render(&baseline(), &[], &p);
        let order = [
            "Windows session the elevated step ran in: 2",
            "before the grant: `O:SYG:SYD:(A;;0xf0003;;;SY)`",
            "after the grant: `O:SYG:SYD:(A;;0xf0003;;;SY)(A;;0x2000f;;;S-1-5-21-1-2-3-1001)`",
            "Path chosen: the fallback grant",
            "(setup's probe): session0::Proof::Refused",
            "(probe after the grant): session0::Proof::Made",
        ];
        let mut last = 0;
        for want in order {
            let at = md
                .find(want)
                .unwrap_or_else(|| panic!("missing {want:?} in\n{md}"));
            assert!(at >= last, "{want:?} is out of order in\n{md}");
            last = at;
        }
    }

    /// A fallback whose probe is refused again leaves the path as neither.
    #[test]
    fn a_fallback_that_is_refused_too_chooses_neither() {
        let mut ex = Fake {
            probe_code: 1,
            probe_out: "Access is denied (os error 5)".into(),
            ..refused_setup()
        };
        let p = run_phase(&mut ex, &baseline(), Some(true), &|| false);
        assert!(
            chosen_path(&p).starts_with("neither"),
            "{}",
            chosen_path(&p)
        );
        assert!(chosen_path(&SandboxPhase::default()).starts_with("not known"));
    }

    /// A removal that leaves the service or the grant behind is a failure,
    /// and a status that does not print the lines cannot confirm either.
    #[test]
    fn a_service_or_a_grant_left_after_removal_is_a_failure() {
        let g = check_gone(STATUS_GONE);
        assert!(g.ok && g.service_gone && g.grant_gone, "{g:?}");
        let service = STATUS_GONE.replace(
            "MISSING  launcher service kgraf-launcher-00ff",
            "present  launcher service kgraf-launcher-00ff (installed, stopped)",
        );
        let g = check_gone(&service);
        assert!(!g.ok && !g.service_gone && g.grant_gone, "{g:?}");
        let grant = STATUS_GONE.replace(
            "MISSING  sandbox group may make named objects",
            "present  sandbox group may make named objects",
        );
        let g = check_gone(&grant);
        assert!(!g.ok && g.service_gone && !g.grant_gone, "{g:?}");
        let silent = "  MISSING  offline sandbox user kgraf-a";
        let g = check_gone(silent);
        assert!(!g.ok && g.left.len() == 2, "{g:?}");
    }
}
