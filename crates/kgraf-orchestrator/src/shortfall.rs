//! A worker reports that it cannot do its brief as given; the orchestrator
//! grants, amends, asks the user, or holds, as the user's permission mode
//! says (decisions 0093 and 0094).
//!
//! - **The report travels the challenge path.** The worker tool
//!   [`ShortfallTool`] (`report_shortfall`) is late-bound through a
//!   [`ContractLine`], as `challenge_contract` is (0029), and sends
//!   [`ContractControl::Shortfall`]. Who reports is the calling worker's agent
//!   id from the tool context; the orchestrator finds the contract from the
//!   attempt that worker runs.
//! - **Two kinds.** `permission`: a policy refused a call the brief needs
//!   (a command's program, a file outside the write scope, or a tool).
//!   `instructions`: the brief is missing something or is wrong (0027's
//!   inadequacy, for a work contract).
//! - **The worker stops.** The orchestrator answers, then cancels the
//!   attempt. The attempt is not verified and not counted a failure.
//! - **The mode is the ceiling** (0094). [`decide`] reads the user's mode and
//!   allow set ([`Authority`]), never the worker's words: Bypass and
//!   Orchestrator decides grant what can be granted; Auto grants its allow
//!   set; Accept edits grants file-scope widenings and the common file
//!   programs; anything else asks the user, or, in a run nobody attends, is
//!   held with the question. An `instructions` report amends the contract
//!   once, additively.
//! - **Bounded.** A contract is re-dispatched after a shortfall at most
//!   [`MAX_SHORTFALL_REDISPATCHES`] times; a grant that did not help, or a
//!   second `instructions` report, holds it. Under Orchestrator decides these
//!   are the orchestrator's refusals: it never asks the user.
//! - **Grounded in the refusal record** (amended 2026-10-01). A permission
//!   report is resolved against the calls a policy actually refused this
//!   worker ([`ground`], reading the run's [`RefusalDirectory`]); the
//!   worker's `need` only picks among them, or names one when it is a single
//!   clean token. A report nothing refused, or one naming several refused
//!   subjects, is answered to the worker, which keeps working: nothing is
//!   granted or held. So is a path the contract may already write, and a
//!   path another contract owns (disjoint writes, in every mode). A worker
//!   answered [`MAX_ANSWERED_SHORTFALLS`] times that reports again ends as
//!   stuck. A user deny or ask rule the grant would meet again holds or asks
//!   in every mode, Bypass included.
//!
//! ```text
//!   report(permission) --ground--> nothing refused | ambiguous --> answered (worker continues)
//!                              \-> refused subject --path in scope | owned elsewhere--> answered
//!                                                  \-> decide (grant | ask | hold)
//!   answered x MAX_ANSWERED_SHORTFALLS, then another report --> stopped, attempt stuck
//! ```
//!
//! Written new: searched the delegation sources of the 0055 reuse survey
//! (goose, codex-rs) for a worker-to-director permission request with a
//! bounded grant; codex-rs asks the user per command (its approval policy),
//! which kGraf already ports as the approval chain, and has no director
//! that re-dispatches.

use crate::challenge::{ContractControl, ContractReply};
use crate::challenge_tool::ContractLine;
use crate::run_state::OpenContract;
use kgraf_runtime::{RefusalDirectory, RefusalRecord, RefusedCall, WorkerResult};
use kgraf_session::skills::SkillDirectory;
use kgraf_tools::QualifiedName;
use kgraf_tools::permissions::{
    AllowSet, FILE_EDIT_PROGRAMS, GrantSubject, HardLimit, PermissionMode, UserRules, rule_on_grant,
};
use kgraf_tools::{
    Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput, UntrustedText,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;

pub use kgraf_runtime::SHORTFALL_TOOL;

/// Re-dispatches one contract may get from shortfalls, grants and
/// amendments together. A starting point, not a measured number: two covers
/// one grant followed by one amendment, and a third report is a sign the
/// contract needs the user, not another attempt.
pub const MAX_SHORTFALL_REDISPATCHES: u32 = 2;

/// The tag an earlier attempt's `instructions` report is quoted in, as data,
/// in the next attempt's brief. The worker's system prompt names it.
pub const SHORTFALL_TAG: &str = "worker_shortfall";

/// The longest `need` or `why` kept, in characters.
pub const MAX_SHORTFALL_FIELD_CHARS: usize = 1_000;

/// Reports one attempt may have answered without a decision (nothing
/// refused, ambiguous, a path in scope or owned elsewhere) before its next
/// report ends it as stuck. A starting point, not a measured number: two
/// answers give a worker that misread one refusal a second chance to read
/// the answer, and a third baseless report is a loop, not a misreading.
pub const MAX_ANSWERED_SHORTFALLS: usize = 2;

/// The longest input error quoted back to a worker, in characters.
const MAX_QUOTED_ERROR_CHARS: usize = 600;

/// What a worker says it lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortfallKind {
    /// A policy refused a call the brief needs.
    Permission,
    /// The brief is missing something, or is wrong.
    Instructions,
}

impl ShortfallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Instructions => "instructions",
        }
    }
}

/// What a permission report asks for. The scope chooses what a grant would
/// change (a program the shell allows, a path added to the write scope);
/// the mode alone chooses whether it is granted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestScope {
    /// A shell command's program (an exact `argv[0]`).
    #[default]
    Command,
    /// A repository path outside the contract's declared write scope.
    File,
    /// A tool refused by a skill's `allowed-tools` or an MCP rule.
    Tool,
}

impl RequestScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::File => "file",
            Self::Tool => "tool",
        }
    }
}

/// What the orchestrator may grant without asking: the user's mode and
/// allow set, read when the report arrives (and again when the mode
/// changes while a request waits), and whether a person attends the run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Authority {
    pub mode: PermissionMode,
    pub allow: AllowSet,
    /// The user's deny and ask rules: no mode lifts them, so no grant may
    /// be made against them.
    pub rules: UserRules,
    /// A person can answer (the run was started from the box).
    pub attended: bool,
}

/// One grant: a program the contract's shell allows, or a path added to its
/// write scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    Program(String),
    Path(String),
}

impl Grant {
    pub fn subject(&self) -> &str {
        match self {
            Self::Program(p) | Self::Path(p) => p,
        }
    }

    pub fn scope(&self) -> RequestScope {
        match self {
            Self::Program(_) => RequestScope::Command,
            Self::Path(_) => RequestScope::File,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Program(p) => format!("`{p}`"),
            Self::Path(p) => format!("writes to `{p}`"),
        }
    }
}

/// One permission granted to a contract: what, to which worker's report, why,
/// when, and by whom (`mode:<mode>`, `orchestrator` or `user`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    /// The program, or the path for a file-scope grant.
    pub program: String,
    #[serde(default)]
    pub scope: RequestScope,
    /// The worker whose report asked for it.
    pub worker: String,
    /// The worker's reason, scrubbed and capped.
    pub why: String,
    pub at_ms: i64,
    /// Who granted it. Records from before decision 0094 read as the
    /// operator's ceiling.
    #[serde(default = "operator_ceiling")]
    pub by: String,
}

fn operator_ceiling() -> String {
    "operator ceiling".to_owned()
}

/// How the orchestrator resolves one shortfall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortfallDecision {
    /// Grant and re-dispatch; `by` is `mode:<mode>` or `orchestrator`.
    Grant { grant: Grant, by: String },
    /// Amend the contract with the report and re-dispatch.
    Amend,
    /// Ask the user and wait, with this question.
    AskUser { grant: Grant, question: String },
    /// Hold the contract, with this reason.
    Hold { reason: String },
}

impl ShortfallDecision {
    /// For the trace and the worker's answer.
    pub fn describe(&self) -> String {
        match self {
            Self::Grant { grant, by } => {
                format!("granted {} ({by}); re-dispatched", grant.describe())
            }
            Self::Amend => "the contract was amended with the report; re-dispatched".to_owned(),
            Self::AskUser { question, .. } => format!("waiting for the user: {question}"),
            Self::Hold { reason } => format!("held: {reason}"),
        }
    }
}

/// What a permission report names, when it names exactly one thing: the
/// whole `need` is one token (quotes, backticks and a closing period
/// removed), with no space, comma or semicolon. Prose and lists name none.
pub fn program_of(need: &str) -> Option<String> {
    let p = need
        .trim()
        .trim_end_matches('.')
        .trim_matches(|c| c == '`' || c == '"' || c == '\'')
        .trim_end_matches('.');
    let clean = !p.is_empty() && !p.contains(|c: char| c.is_whitespace() || c == ',' || c == ';');
    clean.then(|| p.to_owned())
}

/// The repository path a file-scope report names: one clean token
/// ([`program_of`]), relative, with no `..`, and not a path no worker may
/// write.
///
/// `bypass_git_files`: the mode is Bypass, so `.gitattributes`,
/// `.gitmodules` and `.githooks` may be granted like any file (decision 0094).
pub fn path_of(need: &str, bypass_git_files: bool) -> Result<String, String> {
    let p = program_of(need).ok_or_else(|| "the report names no single path".to_owned())?;
    let p = p.trim_start_matches("./").to_owned();
    let path = std::path::Path::new(&p);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "`{p}` is not a repository path (it is absolute or climbs out with `..`)"
        ));
    }
    if let Some(why) = kgraf_core::paths::forbidden_reason(path)
        && !(bypass_git_files && kgraf_core::paths::is_bypass_git_file(path))
    {
        return Err(format!("`{p}` is not writable by a worker: {why}"));
    }
    Ok(p)
}

/// What a permission report was grounded in ([`ground`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grounding {
    /// One subject a policy refused this worker: a program (scope command),
    /// a repository path (file) or a tool (tool), with the refusal it saw.
    Refused { subject: String, reason: String },
    /// No unretried policy refusal matches: the answer for the worker.
    NothingRefused { answer: String },
    /// The need does not single out one refused subject: the answer for the
    /// worker, which is told to name exactly one.
    Ambiguous { answer: String },
}

/// The subject a refusal offers to a report of `scope`, if it offers one.
fn subject_of(scope: RequestScope, c: &RefusedCall) -> Option<String> {
    match (scope, &c.path) {
        (RequestScope::File, Some(p)) => Some(p.trim_start_matches("./").to_owned()),
        (RequestScope::Command, None) if c.tool == "shell" && c.capability != "shell" => {
            Some(c.capability.clone())
        }
        (RequestScope::Tool, None) if c.tool != "shell" => Some(c.tool.clone()),
        _ => None,
    }
}

/// How a refusal reads in an answer: what was refused and the scope a
/// report of it takes.
fn describe_refusal(c: &RefusedCall) -> String {
    match &c.path {
        Some(p) => format!("writes to `{p}` (scope file)"),
        None if c.tool == "shell" => format!("`{}` (scope command)", c.capability),
        None => format!("`{}` (scope tool)", c.tool),
    }
}

/// Whether `word` names `subject`: equal, or (a program) equal to its last
/// path component.
fn names(scope: RequestScope, subject: &str, word: &str) -> bool {
    let word = word.trim_start_matches("./");
    subject == word || (scope == RequestScope::Command && subject.rsplit('/').next() == Some(word))
}

/// The words of a prose `need` that could name a program, path or tool.
fn words(need: &str) -> Vec<&str> {
    need.split(|c: char| !(c.is_alphanumeric() || "._-/+@".contains(c)))
        .map(|w| w.trim_end_matches('.'))
        .filter(|w| !w.is_empty())
        .collect()
}

fn quoted_error(e: &str) -> String {
    let mut q: String = e.chars().take(MAX_QUOTED_ERROR_CHARS).collect();
    if q.len() < e.len() {
        q.push_str("...");
    }
    q
}

/// Ground a permission report in the worker's recorded policy refusals.
///
/// The candidates are the unretried refusals a report of `scope` could act
/// on. A `need` that is one clean token ([`program_of`]) must name one of
/// them; prose or a list picks the one candidate it mentions, or the only
/// candidate when it mentions none. Nothing to pick from, or a token naming
/// nothing refused, is [`Grounding::NothingRefused`]; several picked is
/// [`Grounding::Ambiguous`]. The worker's words never name a subject no
/// policy refused.
pub fn ground(scope: RequestScope, need: &str, record: &RefusalRecord) -> Grounding {
    let unretried: Vec<&RefusedCall> = record.refused.iter().filter(|c| !c.retried_ok).collect();
    // Distinct subjects in order, each with its newest refusal.
    let mut candidates: Vec<(String, &RefusedCall)> = Vec::new();
    for c in &unretried {
        if let Some(s) = subject_of(scope, c) {
            match candidates.iter_mut().find(|(have, _)| *have == s) {
                Some(slot) => slot.1 = c,
                None => candidates.push((s, c)),
            }
        }
    }
    let token = program_of(need).map(|t| t.trim_start_matches("./").to_owned());
    let picked: Vec<&(String, &RefusedCall)> = match &token {
        Some(t) => candidates
            .iter()
            .filter(|(s, _)| names(scope, s, t))
            .collect(),
        None => {
            let said = words(need);
            let mentioned: Vec<_> = candidates
                .iter()
                .filter(|(s, _)| said.iter().any(|w| names(scope, s, w)))
                .collect();
            if mentioned.is_empty() && candidates.len() == 1 {
                candidates.iter().collect()
            } else {
                mentioned
            }
        }
    };
    match picked.as_slice() {
        [(subject, c)] => Grounding::Refused {
            subject: subject.clone(),
            reason: c.reason.clone(),
        },
        [] if !candidates.is_empty() && token.is_none() => Grounding::Ambiguous {
            answer: format!(
                "need must name exactly one program, repository path or tool a policy refused; \
                 this attempt's refusals of scope {} are: {}. Report again naming one of them, \
                 or continue your work.",
                scope.as_str(),
                candidates
                    .iter()
                    .map(|(_, c)| describe_refusal(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        [] => {
            let mut answer = match &token {
                Some(t) => format!(
                    "Nothing was granted or held: no policy refused `{t}` (scope {}) in this \
                     attempt.",
                    scope.as_str()
                ),
                None => format!(
                    "Nothing was granted or held: no policy refused a call of scope {} in this \
                     attempt.",
                    scope.as_str()
                ),
            };
            let others: Vec<String> = unretried.iter().map(|c| describe_refusal(c)).collect();
            if !others.is_empty() {
                answer.push_str(&format!(
                    " The policy refusals on record are: {}.",
                    others.join(", ")
                ));
            }
            if let Some(e) = &record.last_input_error {
                answer.push_str(&format!(
                    " Your last failed call was refused for its input, which is not a \
                     permission refusal: {}",
                    quoted_error(e)
                ));
            }
            answer.push_str(" Continue your work; fix a malformed call and retry it.");
            Grounding::NothingRefused { answer }
        }
        several => Grounding::Ambiguous {
            answer: format!(
                "need names several refused subjects ({}); name exactly one program, \
                 repository path or tool in need.",
                several
                    .iter()
                    .map(|(_, c)| describe_refusal(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

/// Whether `mode` grants `grant` without the user, and in whose name.
fn granted_by(authority: &Authority, grant: &Grant) -> Option<String> {
    let by_mode = || Some(format!("mode:{}", authority.mode.as_str()));
    match (authority.mode, grant) {
        (PermissionMode::Bypass, _) => by_mode(),
        (PermissionMode::OrchestratorDecides, _) => Some("orchestrator".to_owned()),
        (PermissionMode::Auto, Grant::Program(p)) if authority.allow.allows_program(p) => by_mode(),
        (PermissionMode::Auto, Grant::Path(p)) if authority.allow.allows_path(p) => by_mode(),
        (PermissionMode::AcceptEdits, Grant::Path(_)) => by_mode(),
        (PermissionMode::AcceptEdits, Grant::Program(p))
            if FILE_EDIT_PROGRAMS.contains(&p.rsplit('/').next().unwrap_or(p)) =>
        {
            by_mode()
        }
        _ => None,
    }
}

/// How to allow a request the user was not there to answer.
fn how_to_allow(grant: &Grant) -> String {
    match grant {
        Grant::Program(p) => format!(
            "run again with `--allow {p}` (Auto), or choose a permission mode that grants it"
        ),
        Grant::Path(p) => {
            format!("run again with `--allow-path {p}` (Auto) or `--permission-mode accept-edits`")
        }
    }
}

/// The answer to a file-scope report whose path the contract may already
/// write, or that another contract of the run owns. Neither is granted in
/// any mode: the first needs no grant, and the second would break disjoint
/// writes. `None`: neither, and the report goes on to [`decide`].
pub fn path_answer<'a>(
    path: &str,
    contract: &OpenContract,
    others: impl IntoIterator<Item = &'a OpenContract>,
    refusal: &str,
) -> Option<String> {
    let same = |w: &crate::run_state::WriteTarget| w.path.trim_start_matches("./") == path;
    if contract.writes.iter().any(same) {
        return Some(format!(
            "Nothing was granted or held: `{path}` is already among this contract's files, so \
             you may write it. The refusal you saw was: {}. Continue your work.",
            quoted_error(refusal)
        ));
    }
    let owner = others
        .into_iter()
        .find(|o| o.contract != contract.contract && o.writes.iter().any(same))?;
    Some(format!(
        "Nothing was granted or held: `{path}` belongs to contract `{}`; do not edit it. Your \
         attempt is judged by its own acceptance check ({}), not by that file. Continue your \
         work within your declared files.",
        owner.contract,
        quoted_error(contract.acceptance.trim())
    ))
}

/// Resolve one shortfall against the contract's record and the user's
/// permission mode. Pure: nothing the worker wrote can change `authority`.
/// For a permission report, `need` is the subject [`ground`] found in the
/// worker's refusals, not the worker's words.
pub fn decide(
    kind: ShortfallKind,
    scope: RequestScope,
    need: &str,
    why: &str,
    contract: &OpenContract,
    authority: &Authority,
) -> ShortfallDecision {
    let hold = |reason: String| ShortfallDecision::Hold { reason };
    if contract.shortfall_redispatches >= MAX_SHORTFALL_REDISPATCHES {
        return hold(format!(
            "a worker reported a {} shortfall ({need}: {why}) after {} re-dispatches for \
             shortfalls; answer it and run again",
            kind.as_str(),
            contract.shortfall_redispatches
        ));
    }
    if kind == ShortfallKind::Instructions {
        return if contract.shortfall_notes.is_empty() {
            ShortfallDecision::Amend
        } else {
            hold(format!(
                "the instructions were reported insufficient again after an amendment \
                 ({need}: {why}); clarify the instruction and run again"
            ))
        };
    }
    let grant = match scope {
        RequestScope::Tool => {
            return hold(format!(
                "a worker needs the tool `{}` ({why}); a tool refused by a skill's \
                 allowed-tools or an MCP rule cannot be granted to one attempt: change the \
                 skill or the rule and run again",
                program_of(need).unwrap_or_default()
            ));
        }
        RequestScope::Command => match program_of(need) {
            Some(p) => Grant::Program(p),
            None => {
                return hold(format!(
                    "a worker reported a permission shortfall naming no command ({why})"
                ));
            }
        },
        RequestScope::File => match path_of(need, authority.mode == PermissionMode::Bypass) {
            Ok(p) => Grant::Path(p),
            Err(e) => return hold(format!("a worker asked to write outside its scope: {e}")),
        },
    };
    let already = match &grant {
        Grant::Program(p) => contract.granted_programs.contains(p),
        Grant::Path(p) => contract
            .grants
            .iter()
            .any(|g| g.scope == RequestScope::File && g.program == *p),
    };
    if already {
        return hold(format!(
            "{} was granted and an attempt still reported it short ({why}); the grant did not \
             help",
            grant.describe()
        ));
    }
    // The user's rules come before every mode, Bypass and Orchestrator
    // decides included: a grant against a deny rule would be refused again
    // on the next attempt, and one against an ask rule would skip a question
    // the user asked to be put.
    let subject = match &grant {
        Grant::Program(p) => GrantSubject::Program(p),
        Grant::Path(p) => GrantSubject::Path(p),
    };
    match rule_on_grant(&authority.rules, subject) {
        Some(HardLimit::Deny(rule)) => {
            return hold(format!(
                "a worker needs {} ({why}); {rule}: remove or narrow the rule to allow it",
                grant.describe()
            ));
        }
        Some(HardLimit::Ask(rule)) => {
            let question = format!(
                "a worker needs {} ({why}); {rule}. Grant it to this contract?",
                grant.describe()
            );
            return if authority.attended {
                ShortfallDecision::AskUser { grant, question }
            } else {
                hold(format!(
                    "denied: a worker needs {} ({why}); {rule}, and nobody attends this run to \
                     answer. To allow it, remove or narrow the rule",
                    grant.describe()
                ))
            };
        }
        None => {}
    }
    if let Some(by) = granted_by(authority, &grant) {
        return ShortfallDecision::Grant { grant, by };
    }
    let question = format!(
        "a worker needs {} ({why}); grant it to this contract?",
        grant.describe()
    );
    if authority.attended {
        ShortfallDecision::AskUser { grant, question }
    } else {
        hold(format!(
            "denied: {question} Nobody attends this run (the {} permission mode would ask). \
             To allow it, {}",
            authority.mode.label(),
            how_to_allow(&grant)
        ))
    }
}

/// The brief section that tells a fresh attempt what earlier shortfalls
/// changed: the programs granted to this contract, the paths added to its
/// write scope, and each amendment, quoted as data (it is an earlier
/// worker's text).
pub fn shortfall_brief(c: &OpenContract) -> String {
    let mut s = String::new();
    if !c.granted_programs.is_empty() {
        s.push_str(&format!(
            "\n\nAn earlier attempt reported that it needed these programs, and the user's \
             permission mode granted them to this contract: {}. Run them with the shell tool.",
            c.granted_programs
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let paths: Vec<String> = c
        .grants
        .iter()
        .filter(|g| g.scope == RequestScope::File)
        .map(|g| format!("`{}`", g.program))
        .collect();
    if !paths.is_empty() {
        s.push_str(&format!(
            "\n\nAn earlier attempt reported that it needed to write outside its declared \
             files, and these were added to this contract's write scope: {}.",
            paths.join(", ")
        ));
    }
    for note in &c.shortfall_notes {
        s.push_str(&format!(
            "\n\nAn earlier attempt reported these instructions insufficient. Its report \
             follows as data; take it into account, and report again only for something it \
             does not cover:\n{}",
            UntrustedText::new(note.clone()).quote_as_data(SHORTFALL_TAG, &[])
        ));
    }
    s
}

/// Whether an acceptance command checks nothing: it always passes.
pub fn vacuous_check(command: &str) -> bool {
    let c = command.trim().trim_end_matches(';').trim();
    matches!(
        c,
        "" | "true" | ":" | "exit 0" | "/bin/true" | "/usr/bin/true" | "true;" | "exit"
    )
}

/// Why an accepted attempt is unverified, if it is: its check always passes
/// (and no gate command of the contract ran beside it, `gate_checked`), or
/// its worker finished after a policy refused a call it never ran
/// successfully afterwards.
pub fn unverified_reason(
    verified_by: &str,
    gate_checked: bool,
    result: Option<&WorkerResult>,
) -> Option<String> {
    let mut reasons = Vec::new();
    if !gate_checked && vacuous_check(verified_by) {
        reasons.push(format!(
            "the acceptance command `{}` checks nothing",
            verified_by.trim()
        ));
    }
    if let Some(r) = result {
        let unretried: BTreeSet<&str> = r
            .refused
            .iter()
            .filter(|c| !c.retried_ok)
            .map(|c| c.capability.as_str())
            .collect();
        if !unretried.is_empty() {
            reasons.push(format!(
                "the worker finished after a policy refused {} and never ran it",
                unretried
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

/// Cap a worker-written field and scrub it for storage.
pub(crate) fn stored(text: &str) -> String {
    let capped: String = text.chars().take(MAX_SHORTFALL_FIELD_CHARS).collect();
    kgraf_secrets::scrub_persist_owned(capped)
}

/// `report_shortfall`: tell the orchestrator this worker cannot do its brief
/// as given.
pub struct ShortfallTool {
    line: ContractLine,
    refusals: Arc<RefusalDirectory>,
    skills: Option<Arc<SkillDirectory>>,
}

impl ShortfallTool {
    /// A tool that grounds each permission report in the reporting worker's
    /// refusals as mirrored into `refusals` (the directory every worker
    /// environment of the run is given).
    pub fn new(line: ContractLine, refusals: Arc<RefusalDirectory>) -> Self {
        Self {
            line,
            refusals,
            skills: None,
        }
    }

    /// Look up the reporting worker's active skills in `skills`, so a
    /// permission report a skill refuses carries that refusal to the
    /// orchestrator ([`skill_refusal`]).
    pub fn with_skills(mut self, skills: Arc<SkillDirectory>) -> Self {
        self.skills = Some(skills);
        self
    }
}

/// What the reporting worker's skills refuse of a permission report, if they
/// refuse it: a command's program as a `shell` subject, or a tool by name. A
/// file widening is a write scope, not a tool, so no skill is asked about it.
///
/// `need` is the grounded subject. When the live skills say nothing of it
/// (it does not parse as a tool name, or the session is gone), the worker's
/// newest unretried skill refusal of that same subject is the answer, so a
/// hold still names the skill. A skill refusal of another subject is not:
/// it says nothing about what this report asks for.
pub fn skill_refusal(
    skills: Option<&SkillDirectory>,
    worker: &str,
    kind: ShortfallKind,
    scope: RequestScope,
    need: &str,
    recorded: &[RefusedCall],
) -> Option<String> {
    if kind != ShortfallKind::Permission || scope == RequestScope::File {
        return None;
    }
    let need = need.trim();
    let live = skills.and_then(|skills| match scope {
        RequestScope::Command => {
            let shell = QualifiedName::builtin("shell").ok()?;
            skills.refusal_for(worker, &shell, Some(need))
        }
        RequestScope::Tool => {
            let tool = QualifiedName::parse(need).ok()?;
            skills.refusal_for(worker, &tool, None)
        }
        RequestScope::File => None,
    });
    live.or_else(|| {
        recorded
            .iter()
            .rev()
            .find(|c| {
                !c.retried_ok
                    && c.skill().is_some()
                    && subject_of(scope, c).is_some_and(|s| names(scope, &s, need))
            })
            .map(|c| {
                let reason = c.reason.trim();
                reason
                    .strip_prefix("ERROR: denied by ")
                    .unwrap_or(reason)
                    .to_owned()
            })
    })
}

/// The held reason for a permission report a skill refuses.
pub fn skill_hold_reason(refusal: &str) -> String {
    format!(
        "{refusal}; a permission grant cannot lift a skill's allowed-tools, so the contract is \
         held: change the skill's allowed-tools, or the instruction"
    )
}

/// `deny_unknown_fields`: the input is the report, and nothing else. A field
/// naming a mode, a ceiling or a setting is refused, not ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    kind: ShortfallKind,
    #[serde(default)]
    scope: RequestScope,
    need: String,
    why: String,
}

#[async_trait::async_trait]
impl Tool for ShortfallTool {
    fn name(&self) -> &str {
        SHORTFALL_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SHORTFALL_TOOL.into(),
            description: "Report that you cannot do your instruction as given. \
                Kind permission: a call you need was refused by policy; scope command (need \
                is exactly the refused command's program, argv[0]), file (need is exactly the \
                one repository path you were refused writing) or tool (need is exactly the \
                refused tool's name). Only a refusal that happened counts: a call that failed \
                on malformed input was not refused, so fix it and retry instead. Kind \
                instructions: the instruction is missing something or is wrong; need says \
                what. The orchestrator decides under the user's permission settings: it may \
                grant the permission or amend the instruction and start a fresh attempt, ask \
                the user, or hold the work, and then you stop; or it answers that nothing \
                refused you, and you continue. Do not guess or work around a refusal instead."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["permission", "instructions"]},
                    "scope": {"type": "string", "enum": ["command", "file", "tool"], "description": "For kind permission: what was refused. Default command."},
                    "need": {"type": "string", "description": "For kind permission: exactly one program, repository path or tool that was refused. For kind instructions: what the instruction lacks."},
                    "why": {"type": "string", "description": "Why the instruction cannot be done without it."}
                },
                "required": ["kind", "need", "why"],
                "additionalProperties": false
            }),
        }
    }

    /// Touches nothing in the worktree.
    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
            tool: SHORTFALL_TOOL.into(),
            reason: e.to_string(),
        })?;
        if input.need.trim().is_empty() || input.why.trim().is_empty() {
            return Err(ToolError::InvalidInput {
                tool: SHORTFALL_TOOL.into(),
                reason: "need and why must both say something".into(),
            });
        }
        let Some(handle) = self.line.get() else {
            return Ok(ToolOutput::error(
                "no orchestrator is attached to this worker; the report was not sent. Stop, \
                 and say in your answer what you need and why.",
            ));
        };
        // A permission report is grounded in what a policy refused this
        // worker, read from its loop's live record; the worker's words only
        // pick among the refusals.
        let (grounding, skill_refusal) = match input.kind {
            ShortfallKind::Permission => {
                let record = self.refusals.record_of(ctx.worker.as_str());
                let grounding = ground(input.scope, &input.need, &record);
                let skill = match &grounding {
                    Grounding::Refused { subject, .. } => skill_refusal(
                        self.skills.as_deref(),
                        ctx.worker.as_str(),
                        input.kind,
                        input.scope,
                        subject,
                        &record.refused,
                    ),
                    _ => None,
                };
                (Some(grounding), skill)
            }
            ShortfallKind::Instructions => (None, None),
        };
        let control = ContractControl::Shortfall {
            worker: ctx.worker.clone(),
            kind: input.kind,
            scope: input.scope,
            need: input.need,
            why: input.why,
            grounding,
            skill_refusal,
        };
        tokio::select! {
            reply = handle.contract(control) => Ok(match reply {
                Some(ContractReply::Shortfall(text)) => ToolOutput::text(format!(
                    "{text}. Stop now: end your turn with a one-line answer and no tool call."
                )),
                // Answered without a decision: the worker keeps working.
                Some(ContractReply::ShortfallAnswered(text)) => ToolOutput::text(text),
                Some(ContractReply::ShortfallInvalid(text)) => {
                    return Err(ToolError::InvalidInput {
                        tool: SHORTFALL_TOOL.into(),
                        reason: text,
                    });
                }
                Some(ContractReply::Refused(why)) => {
                    ToolOutput::error(format!("the report was refused: {why}"))
                }
                Some(other) => {
                    ToolOutput::error(format!("unexpected answer to a shortfall: {other:?}"))
                }
                None => ToolOutput::error("the run ended before the orchestrator answered"),
            }),
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled {
                tool: SHORTFALL_TOOL.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::TaskId;
    use kgraf_session::skills::{SkillCatalog, SkillSession};
    use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
    use kgraf_tools::permissions::Rule;
    use kgraf_tools::{DiscoveredTools, Exposure, ToolRegistry, ToolView};
    use std::sync::Mutex;

    fn contract() -> OpenContract {
        OpenContract::planned("c1", TaskId::new("t"), "do it", "it is done")
    }

    fn contract_writing(id: &str, paths: &[&str]) -> OpenContract {
        let mut c = OpenContract::planned(id, TaskId::new(id), "do it", "cargo test -p checkout");
        c.writes = paths
            .iter()
            .map(|p| crate::run_state::WriteTarget {
                path: (*p).into(),
                node: None,
            })
            .collect();
        c
    }

    /// A policy refusal of a shell program, as the worker loop records one.
    fn refused_program(program: &str, by: &str) -> RefusedCall {
        RefusedCall {
            tool: "shell".into(),
            capability: program.into(),
            reason: format!("ERROR: denied by {by}: {program} is not allowed"),
            retried_ok: false,
            by: by.into(),
            path: None,
        }
    }

    /// A write-scope refusal of `path`, as the worker loop records one.
    fn refused_path(path: &str) -> RefusedCall {
        RefusedCall {
            tool: "apply_patch".into(),
            capability: "apply_patch".into(),
            reason: format!("ERROR: refused: {path} is outside the declared write scope"),
            retried_ok: false,
            by: String::new(),
            path: Some(path.into()),
        }
    }

    fn record(refused: Vec<RefusedCall>) -> RefusalRecord {
        RefusalRecord {
            refused,
            last_input_error: None,
        }
    }

    /// A session whose worker `w1` activated `read-only-audit`, which allows
    /// only reading tools: the shape of a canary run where both the skill and
    /// the permission mode refused a shell call.
    fn restricted() -> (Arc<SkillSession>, Arc<SkillDirectory>, impl Sized) {
        let skill = serde_json::from_value(serde_json::json!({
            "name": "read-only-audit",
            "description": "audit",
            "frontmatter": {},
            "body": "read only",
            "files": [],
            "allowed_tools": ["read_file", "list_directory"],
            "disable_model_invocation": false,
            "user_invocable": true,
            "hash": "h",
            "sources": [],
        }))
        .expect("skill");
        let session = SkillSession::new(Arc::new(
            SkillCatalog::from_skills([skill]).expect("catalog"),
        ));
        let worker = session.attach("w1");
        session.activate("w1", "read-only-audit").expect("activate");
        let directory = SkillDirectory::new();
        directory.register(&session);
        (session, directory, worker)
    }

    /// The report the real tool sends, and what it gives the worker back,
    /// for `input` from worker `w1` whose loop recorded `refusals`; the
    /// stand-in orchestrator answers with `reply`.
    async fn drive(
        refusals: RefusalRecord,
        skills: Option<Arc<SkillDirectory>>,
        input: serde_json::Value,
        reply: fn(&ContractControl) -> ContractReply,
    ) -> (ContractControl, Result<ToolOutput, ToolError>) {
        let directory = RefusalDirectory::new();
        // In a run the worker loop writes the record (the runtime's tests
        // drive that path); here it is set as that loop would leave it.
        directory.set_record("w1", refusals);
        let sent = Arc::new(Mutex::new(None));
        let seen = sent.clone();
        let line = ContractLine::new();
        line.bind(crate::OrchestratorHandle::answering_contracts(move |c| {
            let r = reply(&c);
            *seen.lock().expect("lock") = Some(c);
            r
        }))
        .expect("bind");
        let mut tool = ShortfallTool::new(line, directory);
        if let Some(s) = skills {
            tool = tool.with_skills(s);
        }
        let mut registry = ToolRegistry::new();
        registry
            .register_builtin(Arc::new(tool), Exposure::Prefix)
            .expect("register");
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &registry,
            discovered: &discovered,
        };
        let ctx = ToolContext {
            worker: "w1".into(),
            task: "t".into(),
            workspace_root: std::env::temp_dir(),
            cancel: kgraf_tools::CancellationToken::new(),
            progress: kgraf_tools::ProgressSink::default(),
        };
        let mut log = MemoryApprovalLog::default();
        let Verdict::Run(approved) = ApprovalChain::new(vec![])
            .review(
                view,
                PendingCall {
                    call_id: "c".into(),
                    tool: SHORTFALL_TOOL.into(),
                    input,
                    worker: ctx.worker.clone(),
                    task: ctx.task.clone(),
                },
                &mut log,
            )
            .await
            .expect("review")
        else {
            panic!("the report is approved");
        };
        let out = registry.execute(approved, &ctx).await;
        let control = sent
            .lock()
            .expect("lock")
            .take()
            .expect("a report was sent");
        (control, out)
    }

    fn grounding_of(c: &ContractControl) -> Option<Grounding> {
        match c {
            ContractControl::Shortfall { grounding, .. } => grounding.clone(),
            _ => None,
        }
    }

    /// Answers a grounded report as the run loop's no-decision path does,
    /// and a refused one with a stand-in decision.
    fn loop_like(c: &ContractControl) -> ContractReply {
        match grounding_of(c) {
            Some(Grounding::NothingRefused { answer }) => ContractReply::ShortfallAnswered(answer),
            Some(Grounding::Ambiguous { answer }) => ContractReply::ShortfallInvalid(answer),
            _ => ContractReply::Shortfall("held: stand-in".into()),
        }
    }

    // ---------------------------------------------------------------- evidence

    /// A malformed apply_patch is not a refusal: the worker that read it as
    /// one and asked for write access is answered with its own input error,
    /// told nothing was granted or held, and keeps working.
    #[tokio::test]
    async fn d4_a_report_after_a_malformed_patch_is_answered_with_the_input_error() {
        let mut r = record(vec![]);
        r.last_input_error = Some(
            "ERROR: invalid call: invalid input for tool `apply_patch`: invalid patch: The first \
             line of the patch must be '*** Begin Patch'; fix the call and retry. This is not a \
             permission refusal."
                .into(),
        );
        let (control, out) = drive(
            r,
            None,
            serde_json::json!({
                "kind": "permission",
                "scope": "file",
                "need": "write access to discount_flat/src/lib.rs via apply_patch tool",
                "why": "the patch was denied"
            }),
            loop_like,
        )
        .await;
        let Some(Grounding::NothingRefused { answer }) = grounding_of(&control) else {
            panic!("{control:?}");
        };
        assert!(answer.contains("Begin Patch"), "{answer}");
        assert!(answer.contains("not a permission refusal"), "{answer}");
        let out = out.expect("answered");
        assert!(!out.is_error);
        let text = out.render();
        assert!(text.contains("Nothing was granted or held"), "{text}");
        assert!(!text.contains("Stop now"), "the worker continues: {text}");
    }

    /// A list of sibling contracts' files is not one subject: with two of
    /// them refused, the worker is told to name exactly one (invalid input);
    /// with one refused, the report is grounded in it, and that file belongs
    /// to another contract, so the worker is told not to edit it.
    #[tokio::test]
    async fn d2_a_list_of_paths_is_ambiguous_or_grounds_the_one_refused() {
        let need = "checkout/src/lib.rs, pricing_engine/src/lib.rs, shipping_standard/src/lib.rs, \
                    tax_us/src/lib.rs";
        let input = serde_json::json!({
            "kind": "permission", "scope": "file", "need": need, "why": "integration"
        });
        let (control, out) = drive(
            record(vec![
                refused_path("checkout/src/lib.rs"),
                refused_path("pricing_engine/src/lib.rs"),
            ]),
            None,
            input.clone(),
            loop_like,
        )
        .await;
        assert!(
            matches!(grounding_of(&control), Some(Grounding::Ambiguous { .. })),
            "{control:?}"
        );
        let err = out.expect_err("invalid input");
        assert!(
            matches!(&err, ToolError::InvalidInput { reason, .. } if reason.contains("exactly one")),
            "{err}"
        );

        let (control, _) = drive(
            record(vec![refused_path("checkout/src/lib.rs")]),
            None,
            input,
            loop_like,
        )
        .await;
        let Some(Grounding::Refused { subject, reason }) = grounding_of(&control) else {
            panic!("{control:?}");
        };
        assert_eq!(subject, "checkout/src/lib.rs");
        let mine = contract_writing("discount_flat", &["discount_flat/src/lib.rs"]);
        let sibling = contract_writing("checkout", &["checkout/src/lib.rs"]);
        let answer = path_answer(&subject, &mine, [&mine, &sibling], &reason).expect("answered");
        assert!(
            answer.contains("belongs to contract `checkout`"),
            "{answer}"
        );
        assert!(answer.contains("do not edit it"), "{answer}");
        assert!(answer.contains("cargo test -p checkout"), "{answer}");
    }

    /// The prose need that was held as "writes to `write`" is grounded in
    /// the path the scope refused, never in its first word; without a
    /// refusal it is answered, never held.
    #[tokio::test]
    async fn d8_prose_grounds_the_refused_path_and_nothing_refused_is_answered() {
        let input = serde_json::json!({
            "kind": "permission", "scope": "file",
            "need": "write access to checkout/src/lib.rs", "why": "wire it in"
        });
        let (control, _) = drive(
            record(vec![refused_path("checkout/src/lib.rs")]),
            None,
            input.clone(),
            loop_like,
        )
        .await;
        assert_eq!(
            grounding_of(&control),
            Some(Grounding::Refused {
                subject: "checkout/src/lib.rs".into(),
                reason: refused_path("checkout/src/lib.rs").reason,
            })
        );
        let (control, out) = drive(record(vec![]), None, input, loop_like).await;
        assert!(
            matches!(
                grounding_of(&control),
                Some(Grounding::NothingRefused { .. })
            ),
            "{control:?}"
        );
        assert!(!out.expect("answered").is_error);
    }

    /// Orchestrator decides once granted `running`, the first word of a
    /// prose need. Grounded, the grant is the refused program; with nothing
    /// refused there is nothing to grant.
    #[tokio::test]
    async fn orchestrator_decides_grants_the_refused_program_not_a_word_of_prose() {
        let need = "running arbitrary Python code (e.g., python3 -c ...)";
        let input = serde_json::json!({"kind": "permission", "need": need, "why": "the check"});
        let (control, _) = drive(
            record(vec![refused_program("python3", "access")]),
            None,
            input.clone(),
            loop_like,
        )
        .await;
        let Some(Grounding::Refused { subject, .. }) = grounding_of(&control) else {
            panic!("{control:?}");
        };
        assert_eq!(subject, "python3");
        assert_eq!(
            permission(
                RequestScope::Command,
                &subject,
                &authority(PermissionMode::OrchestratorDecides, false)
            ),
            grant_program("python3", "orchestrator")
        );
        let (control, _) = drive(record(vec![]), None, input, loop_like).await;
        assert!(
            matches!(
                grounding_of(&control),
                Some(Grounding::NothingRefused { .. })
            ),
            "{control:?}"
        );
    }

    /// Bypass once granted a program the user's ask rule covers, and the
    /// next worker met the rule again. The rule now holds it with nobody
    /// attending (naming the rule) and asks with someone attending; a deny
    /// rule holds it in every mode.
    #[tokio::test]
    async fn an_ask_rule_blocks_a_bypass_grant() {
        let (control, _) = drive(
            record(vec![refused_program("python3", "ask-policy")]),
            None,
            serde_json::json!({
                "kind": "permission",
                "need": "shell(python3) to run python3 check.py for acceptance",
                "why": "acceptance"
            }),
            loop_like,
        )
        .await;
        let Some(Grounding::Refused { subject, .. }) = grounding_of(&control) else {
            panic!("{control:?}");
        };
        assert_eq!(subject, "python3");
        let rules = UserRules {
            deny: vec![],
            ask: vec![Rule::parse("shell(python3*)").expect("rule")],
        };
        let bypass = |attended| Authority {
            mode: PermissionMode::Bypass,
            allow: AllowSet::default(),
            rules: rules.clone(),
            attended,
        };
        let d = permission(RequestScope::Command, &subject, &bypass(false));
        let ShortfallDecision::Hold { reason } = &d else {
            panic!("{d:?}");
        };
        assert!(reason.contains("ask rule `shell(python3*)`"), "{reason}");
        assert!(
            matches!(
                permission(RequestScope::Command, &subject, &bypass(true)),
                ShortfallDecision::AskUser { .. }
            ),
            "attended: the user is asked"
        );
        for mode in [
            PermissionMode::Bypass,
            PermissionMode::OrchestratorDecides,
            PermissionMode::Auto,
        ] {
            let a = Authority {
                mode,
                allow: AllowSet::default()
                    .with_programs(["python3"])
                    .expect("allow"),
                rules: UserRules {
                    deny: vec![Rule::parse("shell(python3*)").expect("rule")],
                    ask: vec![],
                },
                attended: true,
            };
            let d = permission(RequestScope::Command, "python3", &a);
            assert!(
                matches!(&d, ShortfallDecision::Hold { reason } if reason.contains("deny rule")),
                "{mode:?}: {d:?}"
            );
        }
    }

    /// A path another contract owns is answered, not granted, in Bypass and
    /// Orchestrator decides alike: disjoint writes come before any mode. A
    /// path the contract already writes is answered too.
    #[test]
    fn a_sibling_path_is_rejected_in_bypass_and_an_own_path_needs_no_grant() {
        let mine = contract_writing("discount_flat", &["discount_flat/src/lib.rs"]);
        let sibling = contract_writing("checkout", &["checkout/src/lib.rs"]);
        let others = [mine.clone(), sibling];
        let answer =
            path_answer("checkout/src/lib.rs", &mine, &others, "refused").expect("answered");
        assert!(
            answer.contains("belongs to contract `checkout`"),
            "{answer}"
        );
        // Without the owner check, Bypass would grant it: the check is what
        // stops it, not the mode.
        assert!(matches!(
            permission(
                RequestScope::File,
                "checkout/src/lib.rs",
                &authority(PermissionMode::Bypass, false)
            ),
            ShortfallDecision::Grant { .. }
        ));
        let own =
            path_answer("discount_flat/src/lib.rs", &mine, &others, "fenced").expect("answered");
        assert!(own.contains("you may write it"), "{own}");
        assert_eq!(path_answer("docs/new.md", &mine, &others, "refused"), None);
    }

    /// A prose need about a tool a skill refused grounds in that tool, and
    /// the hold names the skill, from the live skills or, when they say
    /// nothing, from the recorded refusal.
    #[tokio::test]
    async fn a_prose_need_a_skill_refused_is_held_naming_the_skill() {
        let (_session, skills, _w) = restricted();
        let skill_refusal_of_patch = RefusedCall {
            tool: "apply_patch".into(),
            capability: "apply_patch".into(),
            reason: "ERROR: denied by skill:read-only-audit: `apply_patch` is not in the \
                     allowed-tools of skill `read-only-audit`"
                .into(),
            retried_ok: false,
            by: "skill:read-only-audit".into(),
            path: None,
        };
        let input = serde_json::json!({
            "kind": "permission", "scope": "tool",
            "need": "I need the apply_patch tool to write the answer file",
            "why": "the instruction asks for a file"
        });
        for live in [Some(skills.clone()), None] {
            let (control, _) = drive(
                record(vec![skill_refusal_of_patch.clone()]),
                live.clone(),
                input.clone(),
                loop_like,
            )
            .await;
            let ContractControl::Shortfall {
                grounding,
                skill_refusal,
                ..
            } = &control
            else {
                panic!("{control:?}");
            };
            assert!(
                matches!(grounding, Some(Grounding::Refused { subject, .. }) if subject == "apply_patch"),
                "{control:?}"
            );
            let refusal = skill_refusal.clone().expect("the skill is named");
            let held = skill_hold_reason(&refusal);
            assert!(
                held.contains("read-only-audit"),
                "{}: {held}",
                live.is_some()
            );
        }
    }

    // ---------------------------------------------------------- grounding

    #[test]
    fn ground_picks_only_among_unretried_refusals_of_the_scope() {
        let mut retried = refused_program("curl", "access");
        retried.retried_ok = true;
        let r = record(vec![
            refused_program("python3", "access"),
            retried,
            refused_path("src/a.rs"),
        ]);
        // A clean token must name a refusal of the scope.
        assert!(matches!(
            ground(RequestScope::Command, "python3", &r),
            Grounding::Refused { subject, .. } if subject == "python3"
        ));
        assert!(matches!(
            ground(RequestScope::Command, "`python3`.", &r),
            Grounding::Refused { subject, .. } if subject == "python3"
        ));
        let Grounding::NothingRefused { answer } = ground(RequestScope::Command, "curl", &r) else {
            panic!("a retried refusal is not a refusal");
        };
        assert!(answer.contains("`curl`"), "{answer}");
        assert!(
            answer.contains("writes to `src/a.rs` (scope file)"),
            "{answer}"
        );
        // The scope is the worker's to name; another scope's refusal is
        // listed, not taken.
        assert!(matches!(
            ground(RequestScope::Tool, "apply_patch", &r),
            Grounding::NothingRefused { .. }
        ));
        assert!(matches!(
            ground(RequestScope::File, "./src/a.rs", &r),
            Grounding::Refused { subject, .. } if subject == "src/a.rs"
        ));
        // The only candidate of a scope grounds prose that names none.
        assert!(matches!(
            ground(RequestScope::File, "write access to my file", &r),
            Grounding::Refused { subject, .. } if subject == "src/a.rs"
        ));
        // A program refused by its full path is named by its last part.
        let full = record(vec![refused_program("/usr/bin/python3", "access")]);
        assert!(matches!(
            ground(RequestScope::Command, "python3", &full),
            Grounding::Refused { subject, .. } if subject == "/usr/bin/python3"
        ));
    }

    // ------------------------------------------------ skills and the modes

    #[test]
    fn a_permission_report_a_skill_refuses_carries_the_skills_refusal() {
        let (_session, skills, _w) = restricted();
        let skills = Some(&*skills);
        let command = skill_refusal(
            skills,
            "w1",
            ShortfallKind::Permission,
            RequestScope::Command,
            "wc",
            &[],
        )
        .expect("the skill refuses shell");
        assert!(command.contains("read-only-audit"), "{command}");
        let tool = skill_refusal(
            skills,
            "w1",
            ShortfallKind::Permission,
            RequestScope::Tool,
            "apply_patch",
            &[],
        )
        .expect("the skill refuses apply_patch");
        assert!(tool.contains("apply_patch"), "{tool}");
        assert_eq!(
            skill_refusal(
                skills,
                "w1",
                ShortfallKind::Permission,
                RequestScope::Tool,
                "read_file",
                &[],
            ),
            None,
            "a tool the skill allows is the mode's question"
        );
        assert_eq!(
            skill_refusal(
                skills,
                "w2",
                ShortfallKind::Permission,
                RequestScope::Command,
                "wc",
                &[],
            ),
            None,
            "another worker is not restricted"
        );
        assert_eq!(
            skill_refusal(
                skills,
                "w1",
                ShortfallKind::Instructions,
                RequestScope::Command,
                "wc",
                &[],
            ),
            None
        );
        // A recorded skill refusal of another subject says nothing of this one.
        let mut other = refused_program("git", "skill:read-only-audit");
        other.reason = "ERROR: denied by skill:read-only-audit: no".into();
        assert_eq!(
            skill_refusal(
                None,
                "w1",
                ShortfallKind::Permission,
                RequestScope::Command,
                "python3",
                &[other.clone()],
            ),
            None
        );
        assert_eq!(
            skill_refusal(
                None,
                "w1",
                ShortfallKind::Permission,
                RequestScope::Command,
                "git",
                &[other],
            )
            .as_deref(),
            Some("skill:read-only-audit: no")
        );
        let held = skill_hold_reason(&command);
        assert!(
            held.contains("read-only-audit") && held.contains("cannot lift"),
            "{held}"
        );
    }

    fn authority(mode: PermissionMode, attended: bool) -> Authority {
        Authority {
            mode,
            allow: AllowSet::default(),
            rules: UserRules::default(),
            attended,
        }
    }

    fn auto(programs: &[&str]) -> Authority {
        Authority {
            mode: PermissionMode::Auto,
            allow: AllowSet::default()
                .with_programs(programs.iter().copied())
                .expect("allow"),
            rules: UserRules::default(),
            attended: true,
        }
    }

    fn permission(scope: RequestScope, need: &str, a: &Authority) -> ShortfallDecision {
        decide(
            ShortfallKind::Permission,
            scope,
            need,
            "the task needs it",
            &contract(),
            a,
        )
    }

    fn grant_program(p: &str, by: &str) -> ShortfallDecision {
        ShortfallDecision::Grant {
            grant: Grant::Program(p.into()),
            by: by.into(),
        }
    }

    /// Each mode's answer to one grounded command shortfall and one file
    /// shortfall, attended.
    #[test]
    fn each_mode_grants_asks_or_holds_a_scripted_shortfall() {
        use PermissionMode::*;
        let cmd = |a: &Authority| permission(RequestScope::Command, "python3", a);
        let file = |a: &Authority| permission(RequestScope::File, "docs/out.md", a);
        let asks = |d: &ShortfallDecision| matches!(d, ShortfallDecision::AskUser { .. });

        assert!(asks(&cmd(&authority(Manual, true))));
        assert!(asks(&file(&authority(Manual, true))));

        assert!(asks(&cmd(&authority(AcceptEdits, true))));
        assert_eq!(
            file(&authority(AcceptEdits, true)),
            ShortfallDecision::Grant {
                grant: Grant::Path("docs/out.md".into()),
                by: "mode:accept_edits".into()
            }
        );
        assert_eq!(
            permission(
                RequestScope::Command,
                "mkdir",
                &authority(AcceptEdits, true)
            ),
            grant_program("mkdir", "mode:accept_edits")
        );

        assert_eq!(
            cmd(&auto(&["python3"])),
            grant_program("python3", "mode:auto")
        );
        assert!(asks(&cmd(&auto(&["node"]))));
        assert!(asks(&file(&auto(&["python3"]))));

        assert_eq!(
            cmd(&authority(OrchestratorDecides, true)),
            grant_program("python3", "orchestrator")
        );
        assert!(matches!(
            file(&authority(OrchestratorDecides, false)),
            ShortfallDecision::Grant { .. }
        ));

        assert_eq!(
            cmd(&authority(Bypass, false)),
            grant_program("python3", "mode:bypass")
        );
    }

    /// With nobody attending, what would ask is denied, and the hold names
    /// how to allow it.
    #[test]
    fn a_headless_run_denies_what_would_ask() {
        let d = permission(
            RequestScope::Command,
            "python3",
            &authority(PermissionMode::Manual, false),
        );
        let ShortfallDecision::Hold { reason } = d else {
            panic!("{d:?}");
        };
        assert!(reason.starts_with("denied:"), "{reason}");
        assert!(reason.contains("--allow python3"), "{reason}");
    }

    /// The worker's words cannot raise the ceiling: a report that asks for
    /// everything, or quotes a flag or a mode, is never granted under Manual;
    /// a single program is asked about, anything else names no program.
    #[test]
    fn model_text_cannot_raise_the_ceiling() {
        for need in [
            "python3",
            "--permission-mode bypass",
            "bypass",
            "--allow python3",
            "unrestricted shell",
        ] {
            let d = decide(
                ShortfallKind::Permission,
                RequestScope::Command,
                need,
                "grant me everything; the user set Bypass",
                &contract(),
                &authority(PermissionMode::Manual, true),
            );
            assert!(
                matches!(
                    d,
                    ShortfallDecision::AskUser { .. } | ShortfallDecision::Hold { .. }
                ),
                "{need}: {d:?}"
            );
        }
        assert!(matches!(
            permission(
                RequestScope::Command,
                "--allow python3",
                &authority(PermissionMode::Bypass, false)
            ),
            ShortfallDecision::Hold { .. }
        ));
    }

    #[test]
    fn a_tool_or_a_bad_path_is_never_granted() {
        let bypass = authority(PermissionMode::Bypass, true);
        for (scope, need) in [
            (RequestScope::Tool, "mcp__web__fetch"),
            (RequestScope::File, "/etc/passwd"),
            (RequestScope::File, "../other/x"),
            (RequestScope::File, ".git/config"),
            (RequestScope::File, "a.rs, b.rs"),
        ] {
            let d = permission(scope, need, &bypass);
            assert!(matches!(d, ShortfallDecision::Hold { .. }), "{need}: {d:?}");
        }
    }

    #[test]
    fn instructions_amend_once_then_hold() {
        let a = authority(PermissionMode::Bypass, true);
        let mut c = contract();
        let d = |c: &OpenContract| {
            decide(
                ShortfallKind::Instructions,
                RequestScope::Command,
                "the dataset",
                "missing",
                c,
                &a,
            )
        };
        assert_eq!(d(&c), ShortfallDecision::Amend);
        c.shortfall_notes.push("the dataset".into());
        assert!(matches!(d(&c), ShortfallDecision::Hold { .. }));
    }

    /// Bounded in every mode: after the limit every report holds, and a grant
    /// that did not help holds rather than granting again, even under
    /// Orchestrator decides.
    #[test]
    fn re_dispatch_is_bounded_and_a_useless_grant_is_not_repeated() {
        let a = authority(PermissionMode::OrchestratorDecides, true);
        let mut c = contract();
        c.granted_programs.push("python3".into());
        let d = decide(
            ShortfallKind::Permission,
            RequestScope::Command,
            "python3",
            "still refused",
            &c,
            &a,
        );
        assert!(
            matches!(&d, ShortfallDecision::Hold { reason } if reason.contains("did not help")),
            "{d:?}"
        );
        let mut c = contract();
        c.shortfall_redispatches = MAX_SHORTFALL_REDISPATCHES;
        let d = decide(
            ShortfallKind::Permission,
            RequestScope::Command,
            "python3",
            "x",
            &c,
            &a,
        );
        assert!(matches!(d, ShortfallDecision::Hold { .. }), "{d:?}");
    }

    #[test]
    fn a_vacuous_check_or_an_unretried_refusal_is_unverified() {
        assert!(vacuous_check("true"));
        assert!(vacuous_check("  exit 0 "));
        assert!(!vacuous_check("test -f /app/answer.txt"));
        let mut r = WorkerResult::bare("t", "w", kgraf_runtime::WorkerOutcome::Completed);
        assert_eq!(unverified_reason("cargo test", false, Some(&r)), None);
        // A remediation's gate commands ran beside a `true` acceptance.
        assert_eq!(unverified_reason("true", true, Some(&r)), None);
        r.refused.push(refused_program("python3", "access"));
        let reason = unverified_reason("cargo test", false, Some(&r)).expect("unverified");
        assert!(reason.contains("`python3`"), "{reason}");
        r.refused[0].retried_ok = true;
        assert_eq!(unverified_reason("cargo test", false, Some(&r)), None);
        assert!(unverified_reason("true", false, None).is_some());
    }

    /// Model text cannot set a mode or a setting through the tool: the input
    /// is the report and nothing else, so an extra field is refused.
    #[test]
    fn the_tool_input_refuses_a_field_beyond_the_report() {
        for extra in ["ceiling", "mode", "permission_mode", "bypass", "grounding"] {
            let mut v = serde_json::json!({
                "kind": "permission",
                "need": "python3",
                "why": "x",
            });
            v[extra] = serde_json::json!("bypass");
            let err = serde_json::from_value::<Input>(v).err().expect("refused");
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
        let schema = ShortfallTool::new(ContractLine::new(), RefusalDirectory::new())
            .definition()
            .input_schema;
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn an_old_grant_record_reads_as_the_operator_ceiling() {
        let g: GrantRecord = serde_json::from_value(serde_json::json!({
            "program": "python3", "worker": "w", "why": "x", "at_ms": 1
        }))
        .expect("old record");
        assert_eq!(g.by, "operator ceiling");
        assert_eq!(g.scope, RequestScope::Command);
    }
}
