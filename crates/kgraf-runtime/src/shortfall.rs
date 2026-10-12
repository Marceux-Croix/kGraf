//! Calls refused by policy, and how a worker reports what it lacks
//! (decision 0093).
//!
//! A worker whose call is refused by a policy (its write scope, the command
//! policy, a skill's `allowed-tools`, an unattended approval, a fence) used to
//! see only the refusal, and could then guess: a benchmark worker refused
//! `python3` wrote an empty answer that its vacuous check accepted. Three
//! things change here:
//!
//! - **The refusal says how to report it.** Its text ends with
//!   [`refusal_hint`]: stop and call [`SHORTFALL_TOOL`] (when the
//!   environment offers it) instead of working around the refusal.
//! - **The result lists the refusals.** [`RefusedCalls`] records each policy
//!   refusal by its capability ([`capability_of`]: the program for a shell
//!   call, the tool name otherwise), the policy that refused it, the path a
//!   write-scope refusal named, and whether a later call of the same
//!   capability ran without error. The orchestrator reports an unretried
//!   refusal of an accepted attempt: in its unverified reason when the check
//!   checks nothing, else as a notice beside the verified contract.
//! - **A report is grounded in the record.** While the worker runs, its
//!   refusals are mirrored into a [`RefusalDirectory`] the run's
//!   `report_shortfall` reads, so the orchestrator decides on what a policy
//!   actually refused, never on the worker's prose (decision 0093, amended
//!   2026-10-01).
//!
//! A refusal that is not a policy's (malformed input, a path outside the
//! worktree, an unknown tool) is not recorded as one: the call was wrong, not
//! forbidden, and retrying it correctly is the worker's own business. The
//! last such input error is kept, so an answer to a report it caused can
//! quote it back.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The worker tool that reports a shortfall to the orchestrator. Defined here
/// so the refusal text can name it; the orchestrator crate implements it.
pub const SHORTFALL_TOOL: &str = "report_shortfall";

/// One call a policy refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedCall {
    /// The tool called.
    pub tool: String,
    /// What was refused: the program (`argv[0]`) of a shell call, else the
    /// tool name ([`capability_of`]).
    pub capability: String,
    /// The refusal as the worker saw it, without the reporting hint.
    pub reason: String,
    /// Whether a later call of the same tool and capability ran without error.
    #[serde(default)]
    pub retried_ok: bool,
    /// The policy that refused it: the approval chain's `by` (a hook, a skill
    /// as `skill:<name>`, the ask policy), or empty for a write-scope or fence
    /// refusal.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: String,
    /// The repository path a write-scope refusal named, or a user rule on a
    /// file tool matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The policy refused the tool itself, not one command of it: a skill's
    /// `allowed-tools` (decision 0053) or a user rule with no pattern. A
    /// report of scope tool grounds in it; `capability` still names the
    /// program a refused shell call ran, so a report of scope command does
    /// too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole_tool: bool,
    /// For a shell script refused whole (`capability` is its shell, no
    /// command-level match named another program): the programs of the
    /// commands it held, a directory change left out. The refusal counts as
    /// retried once each of them later ran without error, in any form: a
    /// worker refused `sh -c "cargo test -p a"` that then runs
    /// `cargo test -p a` ran what the script held, though it never ran
    /// `sh`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<String>,
    /// For a shell command that runs several programs, refused for lack of
    /// a grant (`env curl x`, `xargs curl`): every program a grant must
    /// name for it to run, the innermost first, so `capability` is the
    /// program that runs and a grant covers its wrappers too (decision
    /// 0094, amended 2026-10-06). Empty for a command of one program, a
    /// script refused whole, and a refusal no grant lifts (a user rule).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
}

impl RefusedCall {
    /// The skill that refused this call, if a skill's `allowed-tools` did:
    /// the name its refusal gives (``... is not permitted by skill `NAME`'s
    /// allowed-tools``) under [`kgraf_tools::SKILLS_POLICY`].
    pub fn skill(&self) -> Option<&str> {
        if self.by != kgraf_tools::SKILLS_POLICY {
            return None;
        }
        const LEAD: &str = "by skill `";
        let rest = &self.reason[self.reason.find(LEAD)? + LEAD.len()..];
        rest.split_once('`').map(|(name, _)| name)
    }
}

/// The record of a call of `tool` with `input` that the policy `by` refused
/// with `output` (the text the worker saw, without the reporting hint).
/// `path`: the path a write-scope refusal named. What was refused is what a
/// command-level match named, or the whole tool, or the call's own
/// capability ([`kgraf_tools::refused_subject`]).
pub fn refused_call(
    tool: &str,
    input: &serde_json::Value,
    by: &str,
    output: &str,
    path: Option<String>,
) -> RefusedCall {
    CallCapability::of(tool, input).refusal(by, output, path)
}

/// The capability a call exercises, for matching a refusal to a later retry:
/// a shell call's program, or the tool's name.
pub fn capability_of(tool: &str, input: &serde_json::Value) -> String {
    // The argv the shell tool runs (a one-string command line split into its
    // words), so `["cargo test"]` and `["cargo", "test"]` are one capability.
    if tool == "shell"
        && let Some(program) = kgraf_tools::command_argv(input).and_then(|a| a.into_iter().next())
    {
        return program;
    }
    tool.to_owned()
}

/// What a call of the current round exercises, kept from the call to its
/// result: a refusal is recorded against the program a policy refused, and a
/// success clears the refusals of every program the call ran.
#[derive(Clone, Debug)]
pub(crate) struct CallCapability {
    pub(crate) tool: String,
    /// The call's own capability ([`capability_of`]).
    pub(crate) capability: String,
    /// Every capability the call exercises: for a shell call each program of
    /// its script ([`kgraf_tools::programs_run`]), else the tool's name.
    programs: Vec<String>,
    /// The call's rule subjects ([`kgraf_tools::permissions::subjects`]: a
    /// shell call's command lines, a file tool's paths), to name what a rule
    /// refused without keeping an input that can be large.
    subjects: Vec<String>,
    /// The file an `apply_patch` call's patch names first.
    pub(crate) patch_target: Option<kgraf_tools::PatchTarget>,
    /// Every program a grant must name for a shell call to run, outermost
    /// first ([`kgraf_tools::permissions::programs_to_grant`]); empty when no
    /// program grant can cover the call.
    grant_programs: Vec<String>,
    /// The file a shell call only writes, when it is a pure file write inside
    /// the worktree ([`kgraf_tools::pure_file_write`]). `apply_patch` writes
    /// it, so a refusal of the call is no missing permission.
    file_write: Option<kgraf_tools::FileWrite>,
}

impl CallCapability {
    /// Of a call read with no worktree: the recipes, scripts and
    /// configuration its programs run are unread, so only what the call
    /// names itself is seen. [`CallCapability::of_in`] reads them.
    pub(crate) fn of(tool: &str, input: &serde_json::Value) -> Self {
        Self::of_in(
            tool,
            input,
            &kgraf_tools::permissions::ReadContext::default(),
        )
    }

    /// Of a call read with the worktree `ctx`: a rule or a grant that
    /// matches a program inside a recipe, a script or a configuration value
    /// (`make deploy` whose recipe runs `curl`) names that inner program,
    /// not the call's own (decision 0094, amended 2026-10-06).
    pub(crate) fn of_in(
        tool: &str,
        input: &serde_json::Value,
        ctx: &kgraf_tools::permissions::ReadContext,
    ) -> Self {
        let capability = capability_of(tool, input);
        let mut programs = vec![capability.clone()];
        if tool == "shell"
            && let Some(argv) = kgraf_tools::command_argv(input)
        {
            for command in kgraf_tools::permissions::simple_commands_in(&argv, ctx) {
                if let Some(p) = command.into_iter().next()
                    && !programs.contains(&p)
                {
                    programs.push(p);
                }
            }
        }
        Self {
            tool: tool.to_owned(),
            capability,
            programs,
            subjects: kgraf_tools::permissions::subjects_in(tool, input, ctx),
            patch_target: (tool == "apply_patch")
                .then(|| kgraf_tools::patch_target(input))
                .flatten(),
            file_write: (tool == "shell")
                .then(|| {
                    ctx.root
                        .as_deref()
                        .and_then(|root| kgraf_tools::pure_file_write(input, root))
                })
                .flatten(),
            grant_programs: if tool == "shell" {
                kgraf_tools::command_argv(input)
                    .and_then(|argv| {
                        kgraf_tools::permissions::programs_to_grant_in(&argv, ctx).ok()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
        }
    }

    /// The file this call only writes, if it is a pure file write.
    pub(crate) fn file_write(&self) -> Option<&kgraf_tools::FileWrite> {
        self.file_write.as_ref()
    }

    pub(crate) fn programs(&self) -> &[String] {
        &self.programs
    }

    /// The call's rule subjects: for a shell call, its command lines.
    pub(crate) fn subjects(&self) -> &[String] {
        &self.subjects
    }

    /// The record of this call refused by `by` with `output`. Only a
    /// command-level match (a user rule, the deletion limit, the mode's
    /// command policy) names another program than the call's own
    /// (`sh -c "... && python3 ..."` refused by a rule on `python3`); a
    /// refusal of the whole tool (a skill's, a rule with no pattern) keeps
    /// the call's capability and is marked [`RefusedCall::whole_tool`]; a
    /// rule on a file tool's path records that path.
    pub(crate) fn refusal(&self, by: &str, output: &str, path: Option<String>) -> RefusedCall {
        use kgraf_tools::RefusedSubject;
        let reason = output
            .strip_prefix(crate::worker::DENIED_PREFIX)
            .and_then(|rest| rest.split_once(": "))
            .map_or(output, |(_, reason)| reason);
        let mut call = RefusedCall {
            tool: self.tool.clone(),
            capability: self.capability.clone(),
            reason: output.to_owned(),
            retried_ok: false,
            by: by.to_owned(),
            path,
            whole_tool: false,
            held: Vec::new(),
            needs: Vec::new(),
        };
        let subject = kgraf_tools::refused_subject_of(&self.tool, &self.subjects, by, reason);
        let by_rule = matches!(&subject, Some(RefusedSubject::Program(_)))
            && !reason.contains(kgraf_tools::permissions::SCRIPT_RUNS_LEAD)
            && !reason.contains(kgraf_tools::permissions::COMMAND_RUNS_LEAD);
        match subject {
            Some(RefusedSubject::Tool) => call.whole_tool = true,
            Some(RefusedSubject::Program(p)) => call.capability = p,
            Some(RefusedSubject::Path(p)) if call.path.is_none() => call.path = Some(p),
            Some(RefusedSubject::Path(_)) | None => {}
        }
        call.held = self.held_by_script(&call);
        if !by_rule {
            call.needs = self.needs_for(&call, by);
            if let Some(first) = call.needs.first()
                && call.capability == self.capability
            {
                call.capability = first.clone();
            }
        }
        call
    }

    /// Every program a grant must name for this refused command to run, the
    /// innermost first ([`RefusedCall::needs`]), when the command runs
    /// several and a grant could lift the refusal: the command policy's (in
    /// its hook, or quoted by the access check), a write scope's refusal of
    /// a command that declares no paths, or an unattended ask's. A script refused whole keeps its own
    /// record ([`RefusedCall::held`]).
    fn needs_for(&self, call: &RefusedCall, by: &str) -> Vec<String> {
        let grantable = by == kgraf_tools::shell::SHELL_POLICY_HOOK
            || by == kgraf_tools::approval::USER
            || (by == "access" && call.reason.contains("forbidden by command policy"))
            // A command a write scope refused for declaring no paths runs
            // confined once every program it runs is granted.
            || (by.is_empty() && call.path.is_none());
        if self.tool != "shell"
            || !grantable
            || call.whole_tool
            || call.path.is_some()
            || !call.held.is_empty()
            || self.grant_programs.len() < 2
        {
            return Vec::new();
        }
        // The program a command-level match named comes first; the rest,
        // innermost first.
        let mut needs: Vec<String> = Vec::new();
        if self.grant_programs.contains(&call.capability) && call.capability != self.capability {
            needs.push(call.capability.clone());
        }
        for p in self.grant_programs.iter().rev() {
            if !needs.contains(p) {
                needs.push(p.clone());
            }
        }
        needs
    }

    /// The programs a script refused whole held ([`RefusedCall::held`]):
    /// empty unless `call` is a shell call recorded against its own shell
    /// program, refused neither as a tool nor for a path.
    fn held_by_script(&self, call: &RefusedCall) -> Vec<String> {
        let shell = call
            .capability
            .rsplit('/')
            .next()
            .unwrap_or(&call.capability);
        if self.tool != "shell"
            || call.whole_tool
            || call.path.is_some()
            || call.capability != self.capability
            || !kgraf_tools::permissions::SCRIPT_SHELLS.contains(&shell)
        {
            return Vec::new();
        }
        self.programs
            .iter()
            .filter(|p| **p != call.capability && !DIRECTORY_CHANGERS.contains(&p.as_str()))
            .cloned()
            .collect()
    }
}

/// Builtins that only change the shell's directory: nothing of them is left
/// to run again once the script is gone.
const DIRECTORY_CHANGERS: &[&str] = &["cd", "pushd", "popd"];

/// Whether a denial names a policy, not a malformed or misplaced call. The
/// approval chain's `access` step refuses paths outside the worktree; it is a
/// policy only when it quotes the command policy. Input the tool could not
/// read ([`kgraf_tools::INVALID_INPUT`]) and an unknown tool (`registry`) are
/// never a policy's. Every other `by` is a hook or a policy by name.
pub(crate) fn is_policy_denial(by: &str, reason: &str) -> bool {
    match by {
        "registry" | kgraf_tools::INVALID_INPUT => false,
        "access" => reason.contains("forbidden by command policy"),
        _ => true,
    }
}

/// Appended to every policy refusal a worker sees.
pub fn refusal_hint(can_report: bool) -> &'static str {
    if can_report {
        " If your instruction cannot be done without this call, do not work around it or \
         guess an answer: call report_shortfall with kind \"permission\", naming in need \
         exactly one program, repository path or tool that was refused, and why, then stop."
    } else {
        " If your instruction cannot be done without this call, do not work around it or \
         guess an answer: stop, and say in your answer which command or tool you needed and \
         why."
    }
}

/// What follows the next step in a refusal a worker sees: the refusal's own
/// text, as the policy wrote it.
pub const REFUSAL_TEXT_LEAD: &str = "The refusal: ";

/// A policy refusal as the worker sees it: its header (`ERROR: denied by
/// <policy>: ` or `ERROR: refused: `), then [`refusal_hint`], then the
/// policy's text after [`REFUSAL_TEXT_LEAD`].
///
/// The next step comes before the policy's text because that text can be
/// long: a shell refusal quotes the whole build and test allowlist, over
/// 2,000 characters. With the step at the end, a model read the allowlist,
/// tried one listed-looking command after another, and never asked. The
/// header is kept as it was, so every reader that matches it still does.
pub(crate) fn with_refusal_hint(output: &str, can_report: bool) -> String {
    let header = match output.strip_prefix(crate::worker::DENIED_PREFIX) {
        Some(rest) => {
            crate::worker::DENIED_PREFIX.len() + rest.find(": ").map_or(rest.len(), |i| i + 2)
        }
        None if output.starts_with(crate::worker::REFUSED_PREFIX) => {
            crate::worker::REFUSED_PREFIX.len()
        }
        None => 0,
    };
    let (head, text) = output.split_at(header);
    let text = text.trim_end();
    let stop = if text.ends_with('.') { "" } else { "." };
    format!(
        "{head}{} {REFUSAL_TEXT_LEAD}{text}{stop}",
        refusal_hint(can_report).trim_start()
    )
}

/// What one running worker's policies refused so far, as `report_shortfall`
/// reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefusalRecord {
    /// The policy refusals, in order.
    pub refused: Vec<RefusedCall>,
    /// The newest call refused for malformed input, as the worker saw it:
    /// not a policy refusal, kept so an answer can quote it back.
    pub last_input_error: Option<String>,
}

/// The refusal records of a run's running workers, by worker id, so a
/// run-wide tool (`report_shortfall`) can read what refused the worker that
/// calls it. A worker's entry is written by its own loop and removed when it
/// finishes; worker ids are unique within a run.
#[derive(Debug, Default)]
pub struct RefusalDirectory {
    workers: Mutex<HashMap<String, RefusalRecord>>,
}

impl RefusalDirectory {
    /// An empty directory.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// `worker`'s record so far; empty for a worker with none.
    pub fn record_of(&self, worker: &str) -> RefusalRecord {
        self.workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(worker)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace `worker`'s record. In a run its own loop is the only writer.
    pub fn set_record(&self, worker: &str, record: RefusalRecord) {
        self.workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(worker.to_owned(), record);
    }

    fn forget(&self, worker: &str) {
        self.workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(worker);
    }
}

/// Opens the `why` of a report that kGraf filed for a worker. The trace of
/// the run and the worker log carry it, so a reader can tell this report from
/// one the model wrote.
pub const KGRAF_FILED_LEAD: &str =
    "Filed by kGraf on the worker's behalf; the model did not call report_shortfall: ";

/// The most command lines kept per refused program in a report kGraf files.
const MAX_REPORTED_LINES: usize = 3;

/// The most characters of one refused command line a report quotes.
const MAX_REPORTED_LINE_CHARS: usize = 160;

/// A permission report that kGraf filed for a worker that was refused the
/// same command line twice and never reported it. At most one per attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KgrafShortfall {
    /// The program the report asks for: the one whose command line was refused
    /// twice.
    pub need: String,
    /// The report's reason, which names every refused program and its
    /// command lines. Begins with [`KGRAF_FILED_LEAD`].
    pub why: String,
    /// What the report tool answered, if it was called and answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

/// Refusals of shell programs in one attempt, and whether a report was made
/// (by the model) or filed (by kGraf). The rule, kept here so it is one
/// readable function:
///
/// - Only a shell program refused by a command policy or the unattended ask
///   counts. A whole-tool refusal (a skill), a path refusal (a write scope)
///   and a user deny rule do not: a grant cannot lift them, or the report is
///   a different kind, so the model's own report is the right one.
/// - A later run of the program without error clears its count.
/// - Two different programs refused once each do not count.
/// - Two different command lines of one program, refused once each, do not
///   count either: a worker that tries a second exploratory command is not
///   stuck on the refusal. Only the same command line (the same words,
///   whitespace aside) refused twice is a worker that will not give up the
///   command, so kGraf reports it.
/// - A shell command whose only effect is writing a file inside the worktree
///   does not count (the caller skips it): `apply_patch` writes that file, so
///   the worker lacks no permission.
/// - Any `report_shortfall` call by the model ends the guard for the attempt.
/// - It fires once per attempt.
#[derive(Debug, Default)]
pub(crate) struct RepeatedRefusals {
    /// Per program, in order of first refusal: the command lines refused,
    /// each with its whitespace normalized and not shortened, so two lines
    /// that differ only late compare as different.
    by_program: Vec<(String, Vec<String>)>,
    model_reported: bool,
    filed: bool,
}

impl RepeatedRefusals {
    /// The model called the report tool: kGraf files nothing for it.
    pub(crate) fn model_reported(&mut self) {
        self.model_reported = true;
    }

    /// A call that ran `programs` finished without error.
    pub(crate) fn ran(&mut self, programs: &[String]) {
        self.by_program.retain(|(p, _)| !programs.contains(p));
    }

    /// A call of `tool` was refused: `call` is its record and `lines` its
    /// command lines.
    pub(crate) fn refused(&mut self, tool: &str, call: &RefusedCall, lines: &[String]) {
        let counts = tool == "shell"
            && !call.whole_tool
            && call.path.is_none()
            && call.by != kgraf_tools::permissions::PERMISSION_RULES_HOOK
            && !call.capability.is_empty();
        if !counts {
            return;
        }
        let entry = match self
            .by_program
            .iter()
            .position(|(p, _)| *p == call.capability)
        {
            Some(i) => &mut self.by_program[i].1,
            None => {
                self.by_program.push((call.capability.clone(), Vec::new()));
                &mut self.by_program.last_mut().expect("just pushed").1
            }
        };
        let line = lines.join(" ; ");
        entry.push(line.split_whitespace().collect::<Vec<_>>().join(" "));
    }

    /// The report to file now, if the rule says so and none was filed.
    pub(crate) fn due(&self) -> Option<KgrafShortfall> {
        if self.model_reported || self.filed {
            return None;
        }
        let (need, _) = self.by_program.iter().find(|(_, lines)| {
            lines
                .iter()
                .enumerate()
                .any(|(i, line)| lines[..i].contains(line))
        })?;
        let named: Vec<String> = self
            .by_program
            .iter()
            .map(|(program, lines)| {
                let shown: Vec<String> = lines
                    .iter()
                    .take(MAX_REPORTED_LINES)
                    .map(|l| {
                        let short: String = l.chars().take(MAX_REPORTED_LINE_CHARS).collect();
                        format!("`{short}`")
                    })
                    .collect();
                format!(
                    "`{program}` refused {} time(s): {}",
                    lines.len(),
                    shown.join(", ")
                )
            })
            .collect();
        Some(KgrafShortfall {
            need: need.clone(),
            why: format!(
                "{KGRAF_FILED_LEAD}the worker was refused {} and the instruction cannot be done \
                 without it. Refused: {}.",
                format_args!("`{need}`"),
                named.join("; ")
            ),
            reply: None,
        })
    }

    /// The report is filed or tried; none is filed again.
    pub(crate) fn mark_filed(&mut self) {
        self.filed = true;
    }
}

/// The policy refusals of one worker run, in order, and which were retried
/// successfully; mirrored into the run's [`RefusalDirectory`] when it has one.
#[derive(Debug, Default)]
pub(crate) struct RefusedCalls {
    record: RefusalRecord,
    /// The tool whose malformed input `record.last_input_error` quotes.
    input_error_tool: Option<String>,
    mirror: Option<(Arc<RefusalDirectory>, String)>,
    /// For each refusal, by index, the programs of [`RefusedCall::held`]
    /// that have not yet run without error since it. Local to this run.
    unrun: Vec<std::collections::BTreeSet<String>>,
}

impl RefusedCalls {
    /// Calls of `worker`, mirrored into `directory`.
    pub(crate) fn mirrored(directory: Option<Arc<RefusalDirectory>>, worker: &str) -> Self {
        Self {
            mirror: directory.map(|d| (d, worker.to_owned())),
            ..Self::default()
        }
    }

    fn publish(&self) {
        if let Some((directory, worker)) = &self.mirror {
            directory.set_record(worker, self.record.clone());
        }
    }

    pub(crate) fn refused(&mut self, call: RefusedCall) {
        self.unrun.push(call.held.iter().cloned().collect());
        self.record.refused.push(call);
        self.publish();
    }

    /// A call of `tool` was refused for malformed input, with this text.
    pub(crate) fn input_error(&mut self, tool: &str, text: String) {
        self.record.last_input_error = Some(text);
        self.input_error_tool = Some(tool.to_owned());
        self.publish();
    }

    /// A call of `tool` exercising `capabilities` ran without error: every
    /// earlier refusal of one of them was retried successfully, a script
    /// refused whole was retried once every program it held has run
    /// ([`RefusedCall::held`]), and an input error of the same tool was
    /// fixed.
    pub(crate) fn ran_ok(&mut self, tool: &str, capabilities: &[String]) {
        let mut changed = false;
        for (i, c) in self.record.refused.iter_mut().enumerate() {
            if c.tool != tool || c.retried_ok {
                continue;
            }
            if capabilities.contains(&c.capability) {
                c.retried_ok = true;
                changed = true;
                continue;
            }
            if let Some(unrun) = self.unrun.get_mut(i)
                && !c.held.is_empty()
            {
                unrun.retain(|p| !capabilities.contains(p));
                if unrun.is_empty() {
                    c.retried_ok = true;
                    changed = true;
                }
            }
        }
        if self.input_error_tool.as_deref() == Some(tool) {
            self.record.last_input_error = None;
            self.input_error_tool = None;
            changed = true;
        }
        if changed {
            self.publish();
        }
    }

    /// The refusals, for the result; the worker is finished, so its
    /// directory entry is removed.
    pub(crate) fn take(&mut self) -> Vec<RefusedCall> {
        if let Some((directory, worker)) = &self.mirror {
            directory.forget(worker);
        }
        self.record.last_input_error = None;
        self.input_error_tool = None;
        self.unrun.clear();
        std::mem::take(&mut self.record.refused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of an unattended shell refusal: the question quotes the
    /// whole build and test allowlist before the unattended text.
    fn long_shell_refusal() -> String {
        format!(
            "ERROR: denied by user: Run `mkdir -p out`? It is not on the build and test \
             allowlist, which runs without approval: {}; exactly [\"python3\", \"check.py\"]. \
             Nobody is attached to answer, and no persisted allow rule for this project \
             covers it; run attached to a terminal and answer `a` to record one for this \
             exact call.",
            "cargo build, cargo test with their usual options; ".repeat(50)
        )
    }

    #[test]
    fn the_next_step_comes_before_a_long_refusal_text() {
        let raw = long_shell_refusal();
        for can_report in [true, false] {
            let seen = with_refusal_hint(&raw, can_report);
            assert!(
                seen.starts_with("ERROR: denied by user: If your instruction"),
                "{seen}"
            );
            let step = seen
                .find(if can_report {
                    "call report_shortfall"
                } else {
                    "stop, and say"
                })
                .expect("the step");
            let allowlist = seen
                .find("build and test allowlist")
                .expect("allowlist kept");
            assert!(step < 400, "the step is near the top: at {step}");
            assert!(step < allowlist);
            // The policy's text is kept whole, after the lead.
            let rest = raw.strip_prefix("ERROR: denied by user: ").expect("prefix");
            assert!(
                seen.ends_with(&format!("{REFUSAL_TEXT_LEAD}{rest}")),
                "{seen}"
            );
        }
    }

    /// Every reader matches the header, and the policy name stays readable
    /// from it as before (`by` up to the first `: `).
    #[test]
    fn the_header_and_the_policy_name_are_kept() {
        let seen = with_refusal_hint("ERROR: denied by skills:allowed-tools: no shell", true);
        let rest = seen
            .strip_prefix(crate::worker::DENIED_PREFIX)
            .expect("header");
        assert_eq!(
            rest.split_once(": ").map(|(by, _)| by),
            Some("skills:allowed-tools")
        );
        assert!(seen.ends_with("The refusal: no shell."), "{seen}");
        let seen = with_refusal_hint("ERROR: refused: outside the write scope", false);
        assert!(
            seen.starts_with("ERROR: refused: If your instruction"),
            "{seen}"
        );
        assert!(
            seen.ends_with("The refusal: outside the write scope."),
            "{seen}"
        );
    }

    fn refusal(capability: &str) -> RefusedCall {
        RefusedCall {
            tool: "shell".into(),
            capability: capability.into(),
            reason: "refused".into(),
            retried_ok: false,
            by: "ask-policy".into(),
            path: None,
            whole_tool: false,
            held: Vec::new(),
            needs: Vec::new(),
        }
    }

    #[test]
    fn a_shell_capability_is_its_program_and_another_tool_is_its_name() {
        let input = serde_json::json!({"command": ["python3", "-c", "print(1)"]});
        assert_eq!(capability_of("shell", &input), "python3");
        assert_eq!(capability_of("mcp__x__y", &input), "mcp__x__y");
        assert_eq!(capability_of("shell", &serde_json::json!({})), "shell");
    }

    /// The tool every refusal names is the one skills never restrict; a
    /// rename on one side only would refuse the report the refusal asks for.
    #[test]
    fn the_shortfall_tool_is_never_restricted_by_a_skill() {
        assert!(kgraf_session::skills::NEVER_RESTRICTED.contains(&SHORTFALL_TOOL));
    }

    #[test]
    fn only_a_policy_denial_counts() {
        assert!(is_policy_denial("ask-policy", "unattended"));
        assert!(is_policy_denial("skill:git", "not allowed"));
        assert!(is_policy_denial(
            "access",
            "tool `shell` failed: forbidden by command policy: no"
        ));
        assert!(!is_policy_denial(
            "access",
            "invalid input for tool `shell`"
        ));
        assert!(!is_policy_denial("registry", "no tool named `x`"));
        // A malformed patch is the caller's mistake, whatever its text says.
        assert!(!is_policy_denial(
            kgraf_tools::INVALID_INPUT,
            "invalid input for tool `apply_patch`: invalid patch: The first line of the patch \
             must be '*** Begin Patch'"
        ));
    }

    #[test]
    fn a_later_successful_call_of_the_same_capability_retries_the_refusal() {
        let mut r = RefusedCalls::default();
        r.refused(refusal("python3"));
        r.refused(refusal("curl"));
        r.ran_ok("shell", &["python3".to_owned()]);
        let calls = r.take();
        assert!(calls[0].retried_ok);
        assert!(!calls[1].retried_ok);
    }

    /// The capability a refusal is recorded against is the program a rule
    /// refused, and a successful call clears each program it ran, not only
    /// the shell that wrapped them.
    #[test]
    fn a_wrapped_refusal_is_recorded_against_the_refused_program() {
        let input = serde_json::json!({"command": ["sh", "-c", "mkdir x && python3 -c pass"]});
        let call = CallCapability::of("shell", &input);
        assert_eq!(call.capability, "sh");
        let by_rule = call.refusal(
            kgraf_tools::permissions::PERMISSION_RULES_HOOK,
            "ERROR: denied by user-permission-rules: your deny rule `shell(python3*)` blocks \
             this call in every permission mode",
            None,
        );
        assert_eq!(by_rule.capability, "python3");
        assert!(!by_rule.whole_tool);
        // A reason that quotes no rule is the call's own program's.
        assert_eq!(
            call.refusal("some-hook", "ERROR: denied by some-hook: no", None)
                .capability,
            "sh"
        );
        let mut r = RefusedCalls::default();
        r.refused(refusal("python3"));
        r.refused(refusal("sh"));
        r.ran_ok("shell", call.programs());
        assert!(r.take().iter().all(|c| c.retried_ok));
        // A script that never ran python3 leaves a python3 refusal open.
        let mut r = RefusedCalls::default();
        r.refused(refusal("python3"));
        let other =
            CallCapability::of("shell", &serde_json::json!({"command": ["sh", "-c", "ls"]}));
        r.ran_ok("shell", other.programs());
        assert!(!r.take()[0].retried_ok);
    }

    /// A shell script refused whole is retried once every program it held
    /// has run without error, in any form. The shape seen in a real run: a worker was
    /// denied `sh -c "cargo test -p discount_flat"`, then ran
    /// `cargo test -p discount_flat` as argv, and its contract was still
    /// reported unverified because it "never ran `sh`". A script of two
    /// programs needs both; a refusal a rule narrowed to one program keeps
    /// matching that program only; a directory change is nothing to rerun.
    #[test]
    fn a_script_refused_whole_is_retried_when_its_commands_run() {
        let denied = "ERROR: denied by user: Run `sh -c 'cargo test -p discount_flat'`? It is \
                      not on the build and test allowlist, which runs without approval: cargo \
                      build, cargo check, cargo test.";
        let script = CallCapability::of(
            "shell",
            &serde_json::json!({"command": ["sh", "-c", "cd discount_flat && cargo test -p discount_flat"]}),
        );
        let refused = script.refusal(kgraf_tools::approval::USER, denied, None);
        assert_eq!(refused.capability, "sh");
        assert_eq!(refused.held, ["cargo"]);
        let mut r = RefusedCalls::default();
        r.refused(refused);
        let argv = CallCapability::of(
            "shell",
            &serde_json::json!({"command": ["cargo", "test", "-p", "discount_flat"]}),
        );
        r.ran_ok("shell", argv.programs());
        assert!(r.take()[0].retried_ok, "the script's command ran");

        // Two programs: retried only once both ran.
        let two = CallCapability::of(
            "shell",
            &serde_json::json!({"command": ["sh", "-c", "cargo build && python3 check.py"]}),
        );
        let mut r = RefusedCalls::default();
        r.refused(two.refusal("ask-policy", "ERROR: denied by ask-policy: no", None));
        r.ran_ok("shell", &["cargo".to_owned()]);
        r.ran_ok("apply_patch", &["python3".to_owned()]);
        assert!(
            !r.record.refused[0].retried_ok,
            "python3 never ran as a shell call"
        );
        r.ran_ok("shell", &["python3".to_owned()]);
        assert!(r.take()[0].retried_ok);

        // A refusal a rule narrowed to `python3` holds nothing else.
        let ruled = two.refusal(
            kgraf_tools::permissions::PERMISSION_RULES_HOOK,
            "ERROR: denied by user-permission-rules: your deny rule `shell(python3*)` blocks \
             this call in every permission mode",
            None,
        );
        assert_eq!(ruled.capability, "python3");
        assert!(ruled.held.is_empty());
        let mut r = RefusedCalls::default();
        r.refused(ruled);
        r.ran_ok("shell", &["cargo".to_owned()]);
        assert!(!r.take()[0].retried_ok);

        // Serialized only when held; an older record reads with none.
        let old: RefusedCall = serde_json::from_value(serde_json::json!({
            "tool": "shell", "capability": "sh", "reason": "x"
        }))
        .expect("old record");
        assert!(old.held.is_empty());
    }

    /// A rule that matches a command inside a recipe names the inner program
    /// in the refusal record, not the recipe runner. Without a reading
    /// context the recipe is invisible and the refusal would name `make`.
    #[test]
    fn a_rule_inside_a_recipe_names_the_inner_program_not_the_runner() {
        use kgraf_tools::permissions::ReadContext;
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("Makefile"),
            "deploy:\n\tcurl -X POST https://x\n",
        )
        .expect("makefile");
        let input = serde_json::json!({"command": ["make", "deploy"]});
        let ctx = ReadContext::in_worktree(dir.path(), None);
        let call = CallCapability::of_in("shell", &input, &ctx);
        let refused = call.refusal(
            kgraf_tools::permissions::PERMISSION_RULES_HOOK,
            "ERROR: denied by user-permission-rules: your deny rule `shell(curl*)` blocks this \
             call in every permission mode",
            None,
        );
        assert_eq!(refused.capability, "curl", "{refused:?}");
        // With no context the recipe is unread, so the runner is named: the
        // old behavior this fix corrects.
        let blind = CallCapability::of("shell", &input).refusal(
            kgraf_tools::permissions::PERMISSION_RULES_HOOK,
            "ERROR: denied by user-permission-rules: your deny rule `shell(curl*)` blocks this \
             call in every permission mode",
            None,
        );
        assert_eq!(blind.capability, "make");
    }

    /// The directory sees each refusal and input error while the worker
    /// runs, sees a fixed input error cleared, and forgets the worker when it
    /// finishes.
    #[test]
    fn the_directory_mirrors_a_running_worker_and_forgets_a_finished_one() {
        let directory = RefusalDirectory::new();
        let mut r = RefusedCalls::mirrored(Some(directory.clone()), "w1");
        r.refused(refusal("python3"));
        r.input_error("apply_patch", "ERROR: invalid call: bad patch".into());
        let seen = directory.record_of("w1");
        assert_eq!(seen.refused.len(), 1);
        assert_eq!(
            seen.last_input_error.as_deref(),
            Some("ERROR: invalid call: bad patch")
        );
        assert_eq!(directory.record_of("w2"), RefusalRecord::default());
        r.ran_ok("apply_patch", &["apply_patch".to_owned()]);
        assert_eq!(directory.record_of("w1").last_input_error, None);
        r.ran_ok("shell", &["python3".to_owned()]);
        assert!(directory.record_of("w1").refused[0].retried_ok);
        assert_eq!(r.take().len(), 1);
        assert_eq!(directory.record_of("w1"), RefusalRecord::default());
    }

    /// A skill's refusal, recorded from the text the worker sees, names
    /// its skill; another policy's names none, whatever its reason says.
    #[test]
    fn a_skill_refusal_names_its_skill() {
        let input = serde_json::json!({"command": ["wc", "-l", "notes/beta.txt"]});
        let c = refused_call(
            "shell",
            &input,
            kgraf_tools::SKILLS_POLICY,
            "ERROR: denied by skills:allowed-tools: tool `shell` is not permitted by skill \
             `read-only-audit`'s allowed-tools; `shell-command-policy` would also have asked to \
             run it",
            None,
        );
        assert_eq!(c.skill(), Some("read-only-audit"));
        let mut other = c.clone();
        other.by = "user".into();
        assert_eq!(other.skill(), None);
        assert_eq!(refusal("python3").skill(), None);
    }

    /// A skill's refusal of the shell tool, and a user rule on every shell
    /// call, are refusals of the tool: recorded as such, with the program
    /// the call ran kept for a report of scope command and for a retry. A
    /// skill's reason that quotes a rule matching one command does not
    /// rename it to that command's program.
    #[test]
    fn a_tool_level_refusal_of_a_shell_call_is_recorded_as_of_the_tool() {
        let input = serde_json::json!({"command": ["sh", "-c", "mkdir x && python3 -c pass"]});
        let skill = refused_call(
            "shell",
            &input,
            kgraf_tools::SKILLS_POLICY,
            "ERROR: denied by skills:allowed-tools: tool `shell` is not permitted by skill \
             `read-only-audit`'s allowed-tools; `user-permission-rules` also refused it: your \
             deny rule `shell(python3*)` blocks this call in every permission mode",
            None,
        );
        assert!(skill.whole_tool, "{skill:?}");
        assert_eq!(skill.capability, "sh");
        let every_shell = refused_call(
            "shell",
            &input,
            kgraf_tools::permissions::PERMISSION_RULES_HOOK,
            "ERROR: denied by user-permission-rules: your deny rule `shell` blocks this call in \
             every permission mode",
            None,
        );
        assert!(every_shell.whole_tool, "{every_shell:?}");
        assert_eq!(every_shell.capability, "sh");
        // A rule on a file tool's path records the path.
        let patch = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: secrets/k.txt\n+a\n*** End Patch"
        });
        let by_path = refused_call(
            "apply_patch",
            &patch,
            kgraf_tools::permissions::PERMISSION_RULES_HOOK,
            "ERROR: denied by user-permission-rules: your deny rule `apply_patch(secrets/*)` \
             blocks this call in every permission mode",
            None,
        );
        assert_eq!(by_path.path.as_deref(), Some("secrets/k.txt"));
        assert!(!by_path.whole_tool);
    }
}
