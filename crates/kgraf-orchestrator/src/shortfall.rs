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
//!   once, additively. One that states an environment fact (a missing
//!   module, no network, a command believed not allowed) in an unattended
//!   Bypass run is answered with the facts instead, and the worker keeps
//!   working ([`environment_answer`]).
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
use crate::coordination::{ClosedContract, Closure};
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
    /// Every program one command runs that still lacks a grant, the one
    /// the report named first (`curl`, `env` for `env curl x`): a grant of
    /// one of them alone would leave the command refused (decision 0094,
    /// amended 2026-10-06).
    Programs(Vec<String>),
    Path(String),
}

impl Grant {
    pub fn subject(&self) -> &str {
        match self {
            Self::Program(p) | Self::Path(p) => p,
            Self::Programs(ps) => ps.first().map(String::as_str).unwrap_or_default(),
        }
    }

    /// The programs a program grant names; empty for a path.
    pub fn programs(&self) -> Vec<&str> {
        match self {
            Self::Program(p) => vec![p.as_str()],
            Self::Programs(ps) => ps.iter().map(String::as_str).collect(),
            Self::Path(_) => Vec::new(),
        }
    }

    /// The programs other than [`Self::subject`] a grant names.
    pub fn with(&self) -> Vec<String> {
        match self {
            Self::Programs(ps) => ps.iter().skip(1).cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// A program grant of `subject` and `with`, or a path grant.
    pub fn of(scope: RequestScope, subject: &str, with: &[String]) -> Self {
        match scope {
            RequestScope::File => Self::Path(subject.to_owned()),
            _ if with.is_empty() => Self::Program(subject.to_owned()),
            _ => {
                let mut ps = vec![subject.to_owned()];
                ps.extend(with.iter().filter(|p| *p != subject).cloned());
                Self::Programs(ps)
            }
        }
    }

    pub fn scope(&self) -> RequestScope {
        match self {
            Self::Program(_) | Self::Programs(_) => RequestScope::Command,
            Self::Path(_) => RequestScope::File,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Program(p) => format!("`{p}`"),
            Self::Programs(ps) => ps
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(" and "),
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
    /// `with`: the other programs the refused command runs that a grant must
    /// also name (`env` for `env curl x` grounded in `curl`), innermost
    /// first ([`RefusedCall::needs`]).
    Refused {
        subject: String,
        reason: String,
        with: Vec<String>,
    },
    /// No unretried policy refusal matches: the answer for the worker.
    NothingRefused { answer: String },
    /// The need does not single out one refused subject: the answer for the
    /// worker, which is told to name exactly one.
    Ambiguous { answer: String },
}

/// The subject a refusal offers to a report of `scope`, if it offers one.
/// A refusal of the whole shell tool (a skill's `allowed-tools`, a user rule
/// with no pattern) offers the tool to scope tool and the program the call
/// ran to scope command: both name what was refused.
fn subjects_of(scope: RequestScope, c: &RefusedCall) -> Vec<String> {
    // A command that runs several programs offers each one a grant must
    // name (decision 0094, amended 2026-10-06): a worker refused `env curl
    // x` may name `curl` or `env`.
    let mut out: Vec<String> = subject_of(scope, c).into_iter().collect();
    if scope == RequestScope::Command && c.tool == "shell" && c.path.is_none() {
        for p in &c.needs {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

/// Whether the refusal is of a shell script refused whole, recorded against
/// its shell with the programs it held ([`RefusedCall::held`]).
fn is_script_refusal(c: &RefusedCall) -> bool {
    c.tool == "shell" && !c.whole_tool && c.path.is_none() && !c.held.is_empty()
}

fn subject_of(scope: RequestScope, c: &RefusedCall) -> Option<String> {
    match (scope, &c.path) {
        (RequestScope::File, Some(p)) => Some(p.trim_start_matches("./").to_owned()),
        // A script refused whole offers the programs inside it
        // ([`subjects_of`]), never its shell: a grant of `sh` runs none of
        // them, so it would be granted and refused again.
        (RequestScope::Command, None) if is_script_refusal(c) => None,
        (RequestScope::Command, None) if c.tool == "shell" && c.capability != "shell" => {
            Some(c.capability.clone())
        }
        (RequestScope::Tool, None) if c.tool != "shell" || c.whole_tool => Some(c.tool.clone()),
        _ => None,
    }
}

/// How a refusal reads in an answer: what was refused and the scope a
/// report of it takes.
fn describe_refusal(c: &RefusedCall) -> String {
    match &c.path {
        Some(p) => format!("writes to `{p}` (scope file)"),
        None if c.tool == "shell" && c.whole_tool => format!(
            "the tool `shell` (scope tool), running `{}` (scope command)",
            c.capability
        ),
        None if c.script_without_grant() => format!(
            "a shell script (`{}`) that is not judged command by command, so no grant covers \
             it: its commands run as separate calls",
            c.capability
        ),
        None if is_script_refusal(c) => format!(
            "a shell script (`{}`; scope command, naming {}, which a grant names)",
            c.capability,
            c.needs
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None if c.tool == "shell" && c.needs.len() > 1 => format!(
            "`{}` (scope command; it also runs {}, which a grant names with it)",
            c.capability,
            c.needs
                .iter()
                .filter(|p| **p != c.capability)
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
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
        let offered = subjects_of(scope, c);
        // One candidate per refusal: the first subject it offers, and the
        // others it needs only when the worker named one of them.
        if let Some(s) = offered.first().cloned() {
            match candidates.iter_mut().find(|(have, _)| *have == s) {
                Some(slot) => slot.1 = c,
                None => candidates.push((s, c)),
            }
        }
    }
    let token = program_of(need).map(|t| t.trim_start_matches("./").to_owned());
    let offers =
        |c: &RefusedCall, word: &str| subjects_of(scope, c).iter().any(|s| names(scope, s, word));
    let picked: Vec<&(String, &RefusedCall)> = match &token {
        Some(t) => candidates.iter().filter(|(_, c)| offers(c, t)).collect(),
        None => {
            let said = words(need);
            let mentioned: Vec<_> = candidates
                .iter()
                .filter(|(_, c)| said.iter().any(|w| offers(c, w)))
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
            with: if scope == RequestScope::Command {
                c.needs.iter().filter(|p| *p != subject).cloned().collect()
            } else {
                Vec::new()
            },
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
            // A worker that reports the shell tool when a policy refused one
            // command it ran has the scope wrong, not the refusal.
            if scope == RequestScope::Tool
                && unretried
                    .iter()
                    .any(|c| c.tool == "shell" && c.path.is_none() && !c.whole_tool)
            {
                answer.push_str(
                    " A refused shell command is reported with scope command, naming its \
                     program.",
                );
            }
            // A worker that names the shell of a refused script has the
            // subject wrong: a grant of the shell runs nothing inside it.
            if scope == RequestScope::Command
                && unretried.iter().any(|c| is_script_refusal(c))
                && token.as_deref().is_some_and(|t| {
                    kgraf_tools::permissions::SCRIPT_SHELLS
                        .contains(&t.rsplit('/').next().unwrap_or(t))
                })
            {
                answer.push_str(
                    " A shell script is not granted by naming its shell. Name a program inside \
                     it that was refused, or split it into separate commands.",
                );
            }
            if let Some(e) = &record.last_input_error {
                answer.push_str(&format!(
                    " Your last failed call was refused for its input, which is not a \
                     permission refusal: {}",
                    quoted_error(e)
                ));
            }
            // Retry advice only fits a call that failed on its input.
            if record.last_input_error.is_some() {
                answer.push_str(" Continue your work; fix a malformed call and retry it.");
            } else {
                answer.push_str(" Continue your work.");
            }
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
        (PermissionMode::Auto, Grant::Program(_) | Grant::Programs(_))
            if grant
                .programs()
                .iter()
                .all(|p| authority.allow.allows_program(p)) =>
        {
            by_mode()
        }
        (PermissionMode::Auto, Grant::Path(p)) if authority.allow.allows_path(p) => by_mode(),
        (PermissionMode::AcceptEdits, Grant::Path(_)) => by_mode(),
        (PermissionMode::AcceptEdits, Grant::Program(_) | Grant::Programs(_))
            if grant
                .programs()
                .iter()
                .all(|p| FILE_EDIT_PROGRAMS.contains(&p.rsplit('/').next().unwrap_or(p))) =>
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
        Grant::Programs(ps) => format!(
            "run again with {} (Auto), or choose a permission mode that grants them",
            ps.iter()
                .map(|p| format!("`--allow {p}`"))
                .collect::<Vec<_>>()
                .join(" ")
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
///
/// `judge`: what the verifier says judges the attempt
/// ([`crate::Verifier::judge`]), quoted as it is. The contract's own
/// Acceptance line is not quoted in its place: a verifier that runs the run's
/// command does not run it, and an answer must not name a judge the verifier
/// does not apply. With `None` the answer names no judge.
pub fn path_answer<'a>(
    path: &str,
    contract: &OpenContract,
    others: impl IntoIterator<Item = &'a OpenContract>,
    refusal: &str,
    judge: Option<&str>,
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
        "Nothing was granted or held: `{path}` belongs to contract `{}`; do not edit it.{} \
         Continue your work within your declared files.",
        owner.contract,
        judge_sentence(judge)
    ))
}

/// Characters of a verifier's judge text kept in an answer: it names a
/// command, its selection and the files it waits on, longer than an error
/// quote.
const MAX_JUDGE_CHARS: usize = 1_500;

/// ` <judge>` with a leading space, or nothing.
fn judge_sentence(judge: Option<&str>) -> String {
    let Some(j) = judge.map(str::trim).filter(|j| !j.is_empty()) else {
        return String::new();
    };
    let mut out: String = j.chars().take(MAX_JUDGE_CHARS).collect();
    if out.len() < j.len() {
        out.push_str("...");
    }
    format!(" {out}")
}

/// The answer to an `instructions` report that asks for a change to a file
/// another contract of the run owns, and names none of this contract's own
/// files. Amending this contract's own instruction cannot change that file,
/// and a worker re-dispatched on it would be told to do what it cannot, so the
/// worker is answered instead and keeps working. `None`: the report names no
/// such file, and goes on to [`decide`]. `judge`: as for [`path_answer`].
pub fn instructions_answer<'a>(
    need: &str,
    why: &str,
    contract: &OpenContract,
    others: impl IntoIterator<Item = &'a OpenContract>,
    judge: Option<&str>,
) -> Option<String> {
    let named: Vec<String> = format!("{need} {why}")
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '(' | ')' | ',' | ';' | '<' | '>')
        })
        .map(|t| {
            let t = t.trim_end_matches(['.', ':']);
            // `path:line:col` names the path.
            let t = match t.split_once(':') {
                Some((path, rest)) if rest.starts_with(|c: char| c.is_ascii_digit()) => path,
                _ => t,
            };
            t.trim_start_matches("./").to_owned()
        })
        .filter(|t| t.contains('/') || t.contains('.'))
        .collect();
    let names = |path: &str| {
        let path = path.trim_start_matches("./");
        named
            .iter()
            .any(|t| t == path || t.ends_with(&format!("/{path}")))
    };
    if contract.writes.iter().any(|w| names(&w.path)) {
        return None;
    }
    let (holder, path) = others
        .into_iter()
        .filter(|o| o.contract != contract.contract)
        .find_map(|o| {
            o.writes
                .iter()
                .find(|w| names(&w.path))
                .map(|w| (o, &w.path))
        })?;
    Some(format!(
        "Nothing was amended: `{path}` belongs to contract `{}`; do not edit it and do not ask \
         for it to change. Keep the public shape that `{path}` uses of your items as the \
         existing code defines it, and change your own files so that it compiles as it is.{}",
        holder.contract,
        judge_sentence(judge)
    ))
}

/// A fact of the environment that an `instructions` report gives as its
/// reason, found by [`environment_fact`]. Such a report is not a gap in the
/// instruction: amending the instruction cannot install a module or open a
/// network, and a worker that believed a command was refused never tried it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentFact {
    /// A module, package or program is missing (`ModuleNotFoundError`,
    /// `command not found`).
    MissingModule,
    /// The worker believes it has no network.
    Network,
    /// The worker believes a command it needs is not allowed to run.
    BelievedForbidden,
}

impl EnvironmentFact {
    /// For the answer and the trace.
    pub fn label(self) -> &'static str {
        match self {
            Self::MissingModule => "a missing module, package or program",
            Self::Network => "no network",
            Self::BelievedForbidden => "a command believed not allowed",
        }
    }

    fn remedy(self) -> &'static str {
        match self {
            Self::MissingModule => {
                "Install what is missing (for example `pip install <package>`, or the \
                 project's own package manager), then run your command again."
            }
            Self::Network => {
                "Run the command that needs the network and read its result; do not assume \
                 the network is off."
            }
            Self::BelievedForbidden => {
                "Run the command you believed was not allowed and read its result."
            }
        }
    }
}

/// Words that mark each [`EnvironmentFact`], lowercase. Kept to phrases a
/// report of that fact uses and a gap in an instruction does not: a word such
/// as "package" or "unavailable" alone also names what an instruction lacks.
const MISSING_MODULE_MARKS: &[&str] = &[
    "modulenotfounderror",
    "no module named",
    "importerror:",
    "cannot find module",
    "command not found",
    "not installed",
];
const NETWORK_MARKS: &[&str] = &[
    "no network",
    "network access",
    "network is off",
    "network off",
    "without network",
    "no internet",
    "internet access",
    "web access",
];
const FORBIDDEN_MARKS: &[&str] = &[
    "shell allowlist",
    "not on the allowlist",
    "not allowed to run",
    "not allowed to install",
    "not permitted to run",
    "not permitted to install",
    "does not permit",
    "requires approval",
    "need approval",
    "needs approval",
    "would be denied",
    "cannot install",
    "can't install",
    "unable to install",
];

/// The environment fact an `instructions` report's `need` and `why` give,
/// if any (see [`EnvironmentFact`]). A missing module is checked first: its
/// error text is the most specific.
pub fn environment_fact(need: &str, why: &str) -> Option<EnvironmentFact> {
    let text = format!("{need} {why}").to_lowercase();
    let has = |marks: &[&str]| marks.iter().any(|m| text.contains(m));
    if has(MISSING_MODULE_MARKS) {
        Some(EnvironmentFact::MissingModule)
    } else if has(NETWORK_MARKS) {
        Some(EnvironmentFact::Network)
    } else if has(FORBIDDEN_MARKS) {
        Some(EnvironmentFact::BelievedForbidden)
    } else {
        None
    }
}

/// The answer to an `instructions` report that states an environment fact
/// ([`environment_fact`]) in a run nobody attends whose mode is Bypass: there
/// every command no `forbid` rule refuses runs without approval, so the
/// remedy is the worker's own to try. The worker is told the facts and keeps
/// working; nothing is amended, granted or held. Holding instead stopped a
/// run whose worker never tried to install the module it lacked. An answer,
/// not a grant (decision 0093): a command a policy then refuses is recorded
/// and can be reported as a permission shortfall, decided on that record.
///
/// `None`: the report states no such fact, a person attends (who can answer
/// the question a hold puts), or the mode does not run every command, and
/// the report goes on to [`decide`].
pub fn environment_answer(need: &str, why: &str, authority: &Authority) -> Option<String> {
    if authority.attended || authority.mode != PermissionMode::Bypass {
        return None;
    }
    let fact = environment_fact(need, why)?;
    Some(format!(
        "Nothing was amended or held: this report states an environment fact ({}), not a gap \
         in the instruction. This run's permission mode is Bypass: any command runs without \
         approval, except the few the command policy forbids. The shell tool's description \
         says where commands run and whether the network there is open. {} If a policy \
         refuses the command, the refusal is recorded: report it as kind permission, naming \
         the program. If the command runs and fails, its output is the environment's answer: \
         work from it, and report kind instructions only for what the instruction itself \
         lacks.",
        fact.label(),
        fact.remedy()
    ))
}

/// Resolve one shortfall against the contract's record and the user's
/// permission mode. Pure: nothing the worker wrote can change `authority`.
/// For a permission report, `need` is the subject [`ground`] found in the
/// worker's refusals, not the worker's words.
#[cfg(test)]
pub fn decide(
    kind: ShortfallKind,
    scope: RequestScope,
    need: &str,
    why: &str,
    contract: &OpenContract,
    authority: &Authority,
) -> ShortfallDecision {
    decide_in_plan(kind, scope, need, &[], why, contract, authority, None, &[])
}

/// [`decide`] with the run's open contracts, when they are known. A file a
/// worker needs that no open contract writes is a plan-shape error: the plan
/// has no contract for that file. An unattended hold then says so and asks
/// for a re-plan, instead of pointing at `--allow-path`, which would let one
/// attempt write a file the plan never assigned. With `plan` as `None` the
/// hold keeps the permission wording.
///
/// `finished`: the run's closed contracts. An unattended run that needs a
/// file no open contract writes, but a verified and integrated contract of
/// the run listed in its writes, is granted the path: the file's only planned
/// writer is done, so it has no concurrent writer, and the asking contract's
/// result is verified as usual. A path no contract listed keeps the hold.
///
/// `with`: for a command report, the other programs the refused command
/// runs ([`Grounding::Refused`]); the grant names each of them that the
/// contract and the mode do not already grant, with `need`, so the command
/// runs on the next attempt (decision 0094, amended 2026-10-06).
#[allow(clippy::too_many_arguments)]
pub fn decide_in_plan(
    kind: ShortfallKind,
    scope: RequestScope,
    need: &str,
    with: &[String],
    why: &str,
    contract: &OpenContract,
    authority: &Authority,
    plan: Option<&[OpenContract]>,
    finished: &[ClosedContract],
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
                 allowed-tools, an MCP rule or a rule on every call of the tool cannot be \
                 granted to one attempt: change the skill or the rule and run again",
                program_of(need).unwrap_or_default()
            ));
        }
        RequestScope::Command => match program_of(need) {
            Some(p) => {
                // The other programs the command runs that still lack a
                // grant: not the contract's, and not the mode's own.
                let mode_grants = |q: &str| match authority.mode {
                    PermissionMode::Auto => authority.allow.allows_program(q),
                    PermissionMode::AcceptEdits => {
                        FILE_EDIT_PROGRAMS.contains(&q.rsplit('/').next().unwrap_or(q))
                    }
                    _ => false,
                };
                let lacking: Vec<String> = with
                    .iter()
                    .filter(|q| {
                        **q != p && !contract.granted_programs.contains(q) && !mode_grants(q)
                    })
                    .cloned()
                    .collect();
                if contract.granted_programs.contains(&p) && !lacking.is_empty() {
                    Grant::of(RequestScope::Command, &lacking[0], &lacking[1..])
                } else {
                    Grant::of(RequestScope::Command, &p, &lacking)
                }
            }
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
        Grant::Program(_) | Grant::Programs(_) => grant
            .programs()
            .iter()
            .all(|p| contract.granted_programs.iter().any(|g| g == p)),
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
    let subjects: Vec<GrantSubject<'_>> = match &grant {
        Grant::Program(_) | Grant::Programs(_) => grant
            .programs()
            .into_iter()
            .map(GrantSubject::Program)
            .collect(),
        Grant::Path(p) => vec![GrantSubject::Path(p)],
    };
    // Deny wins over ask, over every program the grant names.
    let met = subjects
        .iter()
        .filter_map(|s| rule_on_grant(&authority.rules, *s))
        .min_by_key(|l| matches!(l, HardLimit::Ask(_)));
    match met {
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
    let unowned_file = match (&grant, plan) {
        (Grant::Path(p), Some(plan)) => (!plan.iter().any(|c| {
            c.writes
                .iter()
                .any(|w| w.path.trim_start_matches("./") == p)
        }))
        .then_some(p),
        _ => None,
    };
    let finished_writer = unowned_file.and_then(|p| {
        finished
            .iter()
            .filter(|c| c.closure == Closure::Verified)
            .find(|c| c.writes.iter().any(|w| w.trim_start_matches("./") == p))
    });
    if authority.attended {
        ShortfallDecision::AskUser { grant, question }
    } else if let (Some(path), Some(writer)) = (unowned_file, finished_writer) {
        ShortfallDecision::Grant {
            by: format!(
                "`{path}` was written by finished contract `{}` and no open contract writes \
                 it, so it was handed to this contract",
                writer.contract
            ),
            grant,
        }
    } else if let Some(path) = unowned_file {
        hold(format!(
            "a worker needs to write `{path}` ({why}), and no open contract of this plan writes \
             it (a contract that finished may have written it; its writes closed with it). \
             That is a plan-shape error, not a missing permission: a path grant would let one \
             attempt write a file the plan never assigned. The plan needs a contract that lists \
             `{path}` in its writes; re-plan, then run again"
        ))
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

/// The reason code of a contract left unverified because its narrowed cargo
/// check selected no test target and no dependent check ran beside it
/// (decision 0100, the zero-test rule). The verifier marks the passing
/// verdict's `by` with `[scoped_check_vacuous: ...]`; [`accepted_standing`]
/// reads the mark. The code is the hook a later executable per-contract
/// acceptance (`OpenContract::gate_commands`) replaces: a contract with gate
/// commands is judged by them and never carries it.
pub const SCOPED_CHECK_VACUOUS: &str = "scoped_check_vacuous";

/// The mark a passing verdict's `by` carries when the contract's own checks
/// (decision 0100, `OpenContract::contract_checks`) ran because the run's
/// check tested nothing, and passed: `[contract_check_passed: <commands>]`.
/// [`accepted_standing`] counts it as a check beside the vacuous one.
pub const CONTRACT_CHECK_PASSED: &str = "contract_check_passed";

/// Whether a verdict's `by` carries [`CONTRACT_CHECK_PASSED`].
pub fn contract_check_passed(verified_by: &str) -> bool {
    verified_by.contains(&format!("[{CONTRACT_CHECK_PASSED}:"))
}

/// The zero-test mark in a verdict's `by`, with what it names.
fn scoped_check_vacuous(verified_by: &str) -> Option<&str> {
    let start = verified_by.find(&format!("[{SCOPED_CHECK_VACUOUS}:"))?;
    let rest = &verified_by[start + SCOPED_CHECK_VACUOUS.len() + 2..];
    Some(rest.trim_end().trim_end_matches(']').trim())
}

/// Whether an acceptance command checks nothing: it always passes.
pub fn vacuous_check(command: &str) -> bool {
    let c = command.trim().trim_end_matches(';').trim();
    matches!(
        c,
        "" | "true" | ":" | "exit 0" | "/bin/true" | "/usr/bin/true" | "true;" | "exit"
    )
}

/// Why a contract's own check (decision 0100, `ContractSpec::check`) is
/// refused at intake, or `None` when it is taken. The planner writes the
/// command, so it is judged as a worker's shell call would be under the
/// user's permissions: it must check something ([`vacuous_check`]); no
/// program in it may meet a deny or ask rule of the user's (those bind in
/// every mode); and past the rules the mode decides. Bypass and Orchestrator
/// decides take it; Auto takes it when every program is in the allow set;
/// Manual and Accept edits would ask, and an intake check has nobody to ask,
/// so they take it only when every program is in the allow set the user
/// wrote.
pub fn check_refusal(
    command: &str,
    writes: &[String],
    settings: &kgraf_tools::permissions::PermissionSettings,
) -> Option<String> {
    use kgraf_tools::permissions::{HardLimit, PermissionMode, hard_limit, simple_commands};
    if let Some(why) = own_check_vacuous(command, writes) {
        return Some(format!(
            "the contract's check `{}` checks nothing: {why}; give a command that runs a test, a \
             build or an assertion over the files the contract writes, or none",
            command.trim()
        ));
    }
    let argv = vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()];
    let mut programs = Vec::new();
    // Entry 0 is the wrapper shell; the rest are the script's own commands.
    for words in simple_commands(&argv).into_iter().skip(1) {
        let call = kgraf_tools::approval::PendingCall {
            call_id: String::new(),
            tool: "shell".into(),
            input: serde_json::json!({ "command": words }),
            worker: kgraf_core::AgentId::new("contract-check"),
            task: kgraf_core::TaskId::new("contract-check"),
        };
        if let Some(HardLimit::Deny(reason) | HardLimit::Ask(reason)) =
            hard_limit(&settings.rules, &call, None, None)
        {
            return Some(format!(
                "the contract's check `{}` runs what your rules refuse or ask about: {reason}",
                command.trim()
            ));
        }
        if let Some(p) = words.first() {
            programs.push(p.clone());
        }
    }
    match settings.mode {
        PermissionMode::Bypass | PermissionMode::OrchestratorDecides => None,
        mode => {
            let missing: Vec<&String> = programs
                .iter()
                .filter(|p| !settings.allow.allows_program(p))
                .collect();
            (!missing.is_empty()).then(|| {
                format!(
                    "the contract's check `{}` runs {} outside your allow set, and your \
                     permission mode ({}) would ask before running it",
                    command.trim(),
                    missing
                        .iter()
                        .map(|p| format!("`{p}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    mode.as_str()
                )
            })
        }
    }
}

/// Why a contract's own check (decision 0100) checks nothing, or `None` when
/// its exit status can fail because a test run, a build, or an assertion over
/// a file the contract writes (`writes`) failed (`own_check.rs`). Listing,
/// printing, existence tests, no-ops, version requests and a status masked
/// by `|| true`, `; true` or `exit 0` never count.
pub fn own_check_vacuous(command: &str, writes: &[String]) -> Option<String> {
    if vacuous_check(command) {
        return Some("it always passes".to_owned());
    }
    crate::own_check::vacuous(command, writes)
}

/// Whether `contract`'s own checks may count as a real check of its work: it
/// has some, and none of them checks nothing. An older record, or a copy,
/// may carry a check intake would refuse today; it never overrules a vacuous
/// verdict or a worker's shortfall.
pub fn own_checks_real(contract: &OpenContract) -> bool {
    let writes: Vec<String> = contract.writes.iter().map(|w| w.path.clone()).collect();
    !contract.contract_checks.is_empty()
        && contract
            .contract_checks
            .iter()
            .all(|c| own_check_vacuous(c, &writes).is_none())
}

/// Whether a pass verdict `by` was checked by `contract`'s own check: the
/// verifier marked it as run and passed, and the check tests something. A
/// vacuous own check never overrules a vacuous verdict or a shortfall.
pub fn own_check_counts(contract: &OpenContract, by: &str) -> bool {
    contract_check_passed(by) && own_checks_real(contract)
}

/// What an accepted attempt's check and its worker's refusals say about it
/// (decision 0093 point 8, amended 2026-10-05).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcceptedStanding {
    /// Why kGraf could not confirm the work, if it could not: reported as
    /// `unverified <id>: <reason>`.
    pub unverified: Option<String>,
    /// The refusals the worker never retried, on an attempt whose check did
    /// confirm the work: a fact the run reports as a notice, not a doubt
    /// about the check.
    pub refusal_notice: Option<String>,
}

/// How an accepted attempt stands: unverified, or verified with its
/// unretried refusals kept as a fact.
///
/// `verified_by` is the passing verdict's `by`; `gate_checked` says a gate
/// command of the contract ran beside it.
///
/// - **Unverified** when the check confirms nothing: it always passes
///   ([`vacuous_check`]) and no gate command ran. An unretried refusal is
///   then part of the reason: with no check to say otherwise, a worker that
///   went on without the refused call may have guessed (the case that caused
///   the rule: a worker refused `python3` wrote an empty answer and a `true`
///   check accepted it).
/// - **Verified, with a fact** when a real check passed and the worker never
///   ran a refused call afterwards. The check runs on the committed change
///   whatever the worker did, so a call the worker did not make does not
///   weaken it. The usual case is a refused self-check (`grep` of the
///   worker's own edit, then the acceptance command). A check deferred to the
///   root gate counts as real: the run cannot complete until that gate
///   passes. The refusal is still reported, as a notice and a trace `notice`
///   event.
///
/// The verb follows the worker's outcome, so an attempt cancelled after its
/// report (a held shortfall verified in scope) is never said to have
/// finished.
pub fn accepted_standing(
    contract: &str,
    verified_by: &str,
    gate_checked: bool,
    result: Option<&WorkerResult>,
) -> AcceptedStanding {
    let refused = result.and_then(|r| {
        // A refusal that named a path (a write scope, a rule on a file
        // tool) is named by it: the tool itself may have run before it.
        let unretried: BTreeSet<String> = r
            .refused
            .iter()
            .filter(|c| !c.retried_ok)
            .map(|c| match &c.path {
                Some(path) => format!("`{}` on `{path}`", c.capability),
                None => format!("`{}`", c.capability),
            })
            .collect();
        (!unretried.is_empty()).then(|| {
            format!(
                "the worker {} after a policy refused {} and never ran it",
                outcome_verb(&r.outcome),
                unretried.into_iter().collect::<Vec<_>>().join(", ")
            )
        })
    });
    let by = verified_by.trim();
    if !gate_checked && let Some(what) = scoped_check_vacuous(by) {
        let reason = format!(
            "{SCOPED_CHECK_VACUOUS}: the narrowed check selected no recognized test ({what}) \
             and no dependent check ran beside it; the contract stays unverified until the root \
             gate passes on a tip that holds its merge"
        );
        return AcceptedStanding {
            unverified: Some(match refused {
                Some(r) => format!("{reason}; {r}"),
                None => reason,
            }),
            refusal_notice: None,
        };
    }
    // A contract's own-check mark is no check by itself: the caller counts
    // it in `gate_checked` only when the check is real.
    let base = verified_by
        .find(&format!(" [{CONTRACT_CHECK_PASSED}:"))
        .map_or(verified_by, |at| &verified_by[..at]);
    if gate_checked || !vacuous_check(base) {
        return AcceptedStanding {
            unverified: None,
            refusal_notice: refused
                .map(|r| format!("contract {contract}: verified by `{by}`; {r}")),
        };
    }
    let checks_nothing = format!("the acceptance command `{by}` checks nothing");
    AcceptedStanding {
        unverified: Some(match refused {
            Some(r) => format!("{checks_nothing}; {r}"),
            None => checks_nothing,
        }),
        refusal_notice: None,
    }
}

/// How a worker's run ended, as the past-tense verb of an unverified reason
/// or a refusal notice.
fn outcome_verb(outcome: &kgraf_runtime::WorkerOutcome) -> &'static str {
    use kgraf_runtime::WorkerOutcome as O;
    match outcome {
        O::Completed => "finished",
        O::HandedBack { .. } => "handed its work back",
        O::BudgetExhausted => "ran out of rounds",
        O::Stalled { .. } => "stalled",
        O::Cancelled => "was cancelled",
        O::Failed { .. } => "failed",
    }
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

/// Ground a permission report of `worker` ([`ground`]) and find what its
/// skills refuse of the grounded subject ([`skill_refusal`]).
///
/// One more case grounds: a report of scope tool naming a tool the worker's
/// live skills refuse, one of whose calls another policy refused this
/// attempt. A skill activated in the same round as that call does not judge
/// it (the round's calls are reviewed before any of them runs), so the
/// record names the other policy; any grant would meet the skill on the next
/// attempt, so the report grounds in that refusal and carries the skill's.
pub fn ground_report(
    skills: Option<&SkillDirectory>,
    worker: &str,
    scope: RequestScope,
    need: &str,
    record: &RefusalRecord,
) -> (Grounding, Option<String>) {
    let grounding = ground(scope, need, record);
    if let Grounding::Refused { subject, .. } = &grounding {
        let skill = skill_refusal(
            skills,
            worker,
            ShortfallKind::Permission,
            scope,
            subject,
            &record.refused,
        );
        return (grounding, skill);
    }
    if scope != RequestScope::Tool {
        return (grounding, None);
    }
    let Some(tool) = program_of(need) else {
        return (grounding, None);
    };
    let refused = record
        .refused
        .iter()
        .rev()
        .find(|c| !c.retried_ok && c.path.is_none() && c.tool == tool);
    let live = skills.and_then(|skills| {
        let name = QualifiedName::parse(&tool).ok()?;
        skills.refusal_for(worker, &name, None)
    });
    match (refused, live) {
        (None, Some(refusal)) if matches!(grounding, Grounding::NothingRefused { .. }) => {
            (restriction_answer(&tool, &refusal, record), None)
        }
        (Some(c), Some(refusal)) => (
            Grounding::Refused {
                subject: tool,
                reason: c.reason.clone(),
                with: Vec::new(),
            },
            Some(refusal),
        ),
        _ => (grounding, None),
    }
}

/// The answer to a report of scope tool that names a tool no call of which was
/// refused this attempt, while the worker's live skills would refuse it. No
/// grant or hold follows (a grant is grounded only in a recorded refusal); the
/// words say what restricts the tool, and drop retry advice when no call failed.
fn restriction_answer(tool: &str, refusal: &str, record: &RefusalRecord) -> Grounding {
    let mut answer = format!(
        "Nothing was granted or held: no call of `{tool}` was refused in this attempt, but an \
         active restriction would refuse one: {}. A permission grant cannot lift a skill's \
         allowed-tools. If your instruction cannot be done without `{tool}`, make the call: a \
         refusal is recorded and can then be reported. Otherwise continue with the tools the \
         skill allows.",
        refusal.trim().trim_end_matches('.')
    );
    if let Some(e) = &record.last_input_error {
        answer.push_str(&format!(
            " Your last failed call was refused for its input, which is not a permission \
             refusal: {}",
            quoted_error(e)
        ));
    }
    Grounding::NothingRefused { answer }
}

/// The held reason for a permission report grounded in a refused package
/// download (decision 0087, amended 2026-10-09): the command policy refuses
/// it whatever is granted, so the contract is held with the refusal, which
/// says what allows the work, instead of being granted `program`.
pub fn package_fetch_hold(program: &str, refusal: &str) -> String {
    let why = refusal
        .split_once("forbidden by command policy: ")
        .map_or(refusal, |(_, why)| why);
    format!(
        "a worker of this contract needs `{program}` to download packages, which no grant \
         allows: {why}"
    )
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
                let (grounding, skill) = ground_report(
                    self.skills.as_deref(),
                    ctx.worker.as_str(),
                    input.scope,
                    &input.need,
                    &record,
                );
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
            whole_tool: false,
            held: Vec::new(),
            needs: Vec::new(),
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
            whole_tool: false,
            held: Vec::new(),
            needs: Vec::new(),
        }
    }

    fn record(refused: Vec<RefusedCall>) -> RefusalRecord {
        RefusalRecord {
            refused,
            last_input_error: None,
        }
    }

    /// A session whose worker `w1` activated `read-only-audit`, which allows
    /// only reading tools: the shape of a run where both the skill and
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
        let Some(Grounding::Refused {
            subject, reason, ..
        }) = grounding_of(&control)
        else {
            panic!("{control:?}");
        };
        assert_eq!(subject, "checkout/src/lib.rs");
        let mine = contract_writing("discount_flat", &["discount_flat/src/lib.rs"]);
        let sibling = contract_writing("checkout", &["checkout/src/lib.rs"]);
        let judge = "Your attempt is judged by `cargo test -p discount_flat`.";
        let answer = path_answer(&subject, &mine, [&mine, &sibling], &reason, Some(judge))
            .expect("answered");
        assert!(
            answer.contains("belongs to contract `checkout`"),
            "{answer}"
        );
        assert!(answer.contains("do not edit it"), "{answer}");
        assert!(answer.contains(judge), "{answer}");
    }

    /// A new workspace: the answer named the contract's Acceptance line
    /// (`[ -f Cargo.toml ] && ...`) as the judge, while the verifier judged
    /// the attempt by the run's cargo command. The answer quotes only what the
    /// verifier says judges the attempt, and names no judge when it says
    /// nothing.
    #[test]
    fn a_path_answer_names_the_verifier_s_judge_never_the_acceptance_line() {
        let mut mine = contract_writing("c0", &["Cargo.toml", "wcl/Cargo.toml"]);
        mine.acceptance = "[ -f Cargo.toml ] && [ -f wcl/Cargo.toml ]".into();
        let owner = contract_writing("c2", &["wcl/src/main.rs"]);
        let judge = "Your attempt is judged by `cargo test -p wcl --offline`. It needs the work \
                     of contracts c2 (`wcl/src/main.rs` of c2), not merged yet: if it fails on \
                     your attempt and also on the tree you were given, it is deferred to the \
                     root gate.";
        let answer = path_answer("wcl/src/main.rs", &mine, [&mine, &owner], "r", Some(judge))
            .expect("answered");
        assert!(answer.contains(judge), "{answer}");
        assert!(!answer.contains("[ -f Cargo.toml ]"), "{answer}");
        assert!(!answer.contains("its own acceptance check"), "{answer}");
        let silent =
            path_answer("wcl/src/main.rs", &mine, [&mine, &owner], "r", None).expect("answered");
        assert!(!silent.contains("judged"), "{silent}");
        assert!(!silent.contains("[ -f Cargo.toml ]"), "{silent}");
        assert!(silent.contains("Continue your work"), "{silent}");
        let asked = instructions_answer(
            "wcl/src/main.rs must exist",
            "",
            &mine,
            [&mine, &owner],
            Some(judge),
        )
        .expect("answered");
        assert!(asked.contains(judge), "{asked}");
        assert!(!asked.contains("[ -f Cargo.toml ]"), "{asked}");
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
                with: Vec::new(),
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
            path_answer("checkout/src/lib.rs", &mine, &others, "refused", None).expect("answered");
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
        let own = path_answer("discount_flat/src/lib.rs", &mine, &others, "fenced", None)
            .expect("answered");
        assert!(own.contains("you may write it"), "{own}");
        assert_eq!(
            path_answer("docs/new.md", &mine, &others, "refused", None),
            None
        );
    }

    /// A prose need about a tool a skill refused grounds in that tool, and
    /// the hold names the skill, from the live skills or, when they say
    /// nothing, from the recorded refusal.
    #[tokio::test]
    async fn a_prose_need_a_skill_refused_is_held_naming_the_skill() {
        let (_session, skills, _w) = restricted();
        // Recorded as the worker loop records the text it saw.
        let skill_refusal_of_patch = kgraf_runtime::refused_call(
            "apply_patch",
            &serde_json::json!({"patch": "*** Begin Patch\n*** Add File: a.md\n+a\n*** End Patch"}),
            kgraf_tools::SKILLS_POLICY,
            "ERROR: denied by skills:allowed-tools: tool `apply_patch` is not permitted by skill \
             `read-only-audit`'s allowed-tools",
            None,
        );
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

    /// The text a worker sees when the active skill refuses the shell tool,
    /// recorded as its loop records it.
    fn skill_refused_shell(command: serde_json::Value) -> RefusedCall {
        kgraf_runtime::refused_call(
            "shell",
            &serde_json::json!({ "command": command }),
            kgraf_tools::SKILLS_POLICY,
            "ERROR: denied by skills:allowed-tools: tool `shell` is not permitted by skill \
             `read-only-audit`'s allowed-tools; `shell-command-policy` would also have asked to \
             run it",
            None,
        )
    }

    /// Decision 0053: a skill refused the shell tool and the worker reports
    /// the tool, as its refusal names it. The report grounds in that refusal
    /// of scope tool and carries the skill's refusal, so the contract is held
    /// naming the skill; the program the call ran grounds a report of scope
    /// command the same way. From the live skills, and, with the session
    /// gone, from the recorded refusal.
    #[tokio::test]
    async fn a_skill_refusal_of_the_shell_tool_is_held_naming_the_skill() {
        let (_session, skills, _w) = restricted();
        let refusals = record(vec![skill_refused_shell(serde_json::json!([
            "sh",
            "-c",
            "wc -l notes/beta.txt"
        ]))]);
        for (scope, need) in [("tool", "shell"), ("command", "sh")] {
            for live in [Some(skills.clone()), None] {
                let input = serde_json::json!({
                    "kind": "permission", "scope": scope, "need": need,
                    "why": "the instruction asks for wc -l"
                });
                let (control, _) = drive(refusals.clone(), live.clone(), input, loop_like).await;
                let ContractControl::Shortfall {
                    grounding,
                    skill_refusal,
                    ..
                } = &control
                else {
                    panic!("{control:?}");
                };
                assert!(
                    matches!(grounding, Some(Grounding::Refused { subject, .. }) if subject == need),
                    "{scope} {need} (live {}): {control:?}",
                    live.is_some()
                );
                let refusal = skill_refusal.clone().unwrap_or_else(|| {
                    panic!("{scope} {need} (live {}): no skill named", live.is_some())
                });
                let held = skill_hold_reason(&refusal);
                assert!(held.contains("read-only-audit"), "{held}");
            }
        }
    }

    /// A round where the worker activated the skill and ran a shell
    /// call in the same round: the call was judged before the skill applied,
    /// so the command policy's ask, denied unattended, is what the record
    /// holds (`sh`, scope command). A report of the shell tool is still held
    /// naming the skill, which refuses every shell call from now on. Without
    /// an active skill the same report grounds nothing, and the answer says
    /// how a refused command is reported.
    #[tokio::test]
    async fn a_shell_report_after_a_same_round_skill_activation_is_held_naming_the_skill() {
        let (_session, skills, _w) = restricted();
        let asked = kgraf_runtime::refused_call(
            "shell",
            &serde_json::json!({"command": ["sh", "-c", "wc -l notes/beta.txt"]}),
            "user",
            "ERROR: denied by user: Run `sh -c 'wc -l notes/beta.txt'`? It is not on the build \
             and test allowlist, which runs without approval: cargo build. Nobody is attached \
             to answer, and no persisted allow rule for this project covers it",
            None,
        );
        assert!(!asked.whole_tool);
        assert_eq!(asked.capability, "sh");
        let input = serde_json::json!({
            "kind": "permission", "scope": "tool", "need": "shell",
            "why": "the instruction asks for wc -l via the shell tool"
        });
        let (control, _) = drive(
            record(vec![asked.clone()]),
            Some(skills.clone()),
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
            matches!(grounding, Some(Grounding::Refused { subject, .. }) if subject == "shell"),
            "{control:?}"
        );
        assert!(
            skill_refusal
                .as_deref()
                .is_some_and(|r| r.contains("read-only-audit")),
            "{control:?}"
        );
        let (control, _) = drive(record(vec![asked]), None, input, loop_like).await;
        let ContractControl::Shortfall {
            grounding: Some(Grounding::NothingRefused { answer }),
            skill_refusal: None,
            ..
        } = &control
        else {
            panic!("no skill, nothing of scope tool refused: {control:?}");
        };
        // The refusal was a script: the answer names the program inside it
        // for a grant, not the shell.
        assert!(
            answer.contains("a shell script (`sh`; scope command, naming `wc`"),
            "{answer}"
        );
        assert!(answer.contains("reported with scope command"), "{answer}");
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

    /// A shell script refused whole, as the worker loop records one: the
    /// refusal is against the shell, and `needs` holds the programs inside
    /// it that a grant names (empty when kGraf cannot read the script
    /// command by command).
    fn refused_script(held: &[&str], needs: &[&str]) -> RefusedCall {
        let mut c = refused_program("sh", "");
        c.reason = "ERROR: refused: the call may write anywhere (it declares no paths)".into();
        c.held = held.iter().map(|s| (*s).to_owned()).collect();
        c.needs = needs.iter().map(|s| (*s).to_owned()).collect();
        c
    }

    /// A script refused whole is grounded in the programs inside it, never in
    /// its shell, and the decision grants those programs together.
    #[test]
    fn a_script_is_grounded_in_the_programs_inside_it_not_the_shell() {
        let r = record(vec![refused_script(&["mkdir", "cp"], &["mkdir", "cp"])]);
        // The shell is not a subject: a grant of it runs nothing inside.
        let Grounding::NothingRefused { answer } = ground(RequestScope::Command, "sh", &r) else {
            panic!("`sh` must not ground a script refusal");
        };
        assert!(
            answer.contains("not granted by naming its shell"),
            "{answer}"
        );
        assert!(answer.contains("`mkdir`, `cp`"), "{answer}");
        // Either program grounds it, with the other beside it.
        for named in ["mkdir", "cp"] {
            let Grounding::Refused { subject, with, .. } = ground(RequestScope::Command, named, &r)
            else {
                panic!("{named} is inside the refused script");
            };
            assert_eq!(subject, "mkdir");
            assert_eq!(with, vec!["cp".to_owned()]);
        }
        // The grant names both programs and never the shell.
        let a = authority(PermissionMode::OrchestratorDecides, true);
        let Grounding::Refused { subject, with, .. } = ground(RequestScope::Command, "mkdir", &r)
        else {
            panic!("grounded above");
        };
        let d = decide_in_plan(
            ShortfallKind::Permission,
            RequestScope::Command,
            &subject,
            &with,
            "the script needs them",
            &contract(),
            &a,
            None,
            &[],
        );
        let ShortfallDecision::Grant { grant, .. } = d else {
            panic!("the programs are granted: {d:?}");
        };
        assert_eq!(grant.programs(), vec!["mkdir", "cp"]);
    }

    /// Counter-case: a refusal of `sh` that held no script (it ran a file) is
    /// still grounded in `sh`, and a script kGraf cannot read grounds nothing
    /// and is told to split.
    #[test]
    fn a_refusal_of_the_shell_alone_still_grounds_and_an_unreadable_script_does_not() {
        let plain = record(vec![refused_script(&[], &[])]);
        assert!(matches!(
            ground(RequestScope::Command, "sh", &plain),
            Grounding::Refused { subject, .. } if subject == "sh"
        ));
        let unreadable = record(vec![refused_script(&["mkdir", "cp"], &[])]);
        for named in ["sh", "mkdir"] {
            let Grounding::NothingRefused { answer } =
                ground(RequestScope::Command, named, &unreadable)
            else {
                panic!("{named} grounds nothing for an unreadable script");
            };
            assert!(answer.contains("no grant covers it"), "{answer}");
        }
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
        let other = kgraf_runtime::refused_call(
            "shell",
            &serde_json::json!({"command": ["git", "status"]}),
            kgraf_tools::SKILLS_POLICY,
            "ERROR: denied by skills:allowed-tools: tool `shell` is not permitted by skill \
             `read-only-audit`'s allowed-tools",
            None,
        );
        assert_eq!(
            skill_refusal(
                None,
                "w1",
                ShortfallKind::Permission,
                RequestScope::Command,
                "python3",
                std::slice::from_ref(&other),
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
            Some(
                "skills:allowed-tools: tool `shell` is not permitted by skill \
                 `read-only-audit`'s allowed-tools"
            )
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
    fn prose_naming_none_of_several_refused_subjects_is_ambiguous_not_a_pick() {
        let r = record(vec![
            refused_program("python3", "access"),
            refused_program("curl", "access"),
        ]);
        let Grounding::Ambiguous { answer } =
            ground(RequestScope::Command, "let me run something please", &r)
        else {
            panic!("expected an ambiguous report");
        };
        assert!(answer.contains("need must name exactly one"), "{answer}");
        assert!(
            answer.contains("python3") && answer.contains("curl"),
            "{answer}"
        );
    }

    #[test]
    fn a_bypass_only_git_file_is_granted_in_bypass_mode_and_held_in_every_other_mode() {
        let in_bypass = permission(
            RequestScope::File,
            ".gitattributes",
            &authority(PermissionMode::Bypass, true),
        );
        assert!(
            matches!(&in_bypass, ShortfallDecision::Grant { grant: Grant::Path(p), .. } if p == ".gitattributes"),
            "{in_bypass:?}"
        );
        for mode in [PermissionMode::Auto, PermissionMode::Manual] {
            let d = permission(RequestScope::File, ".gitattributes", &authority(mode, true));
            assert!(
                matches!(&d, ShortfallDecision::Hold { reason } if reason.contains("not writable")),
                "{d:?}"
            );
        }
    }

    #[test]
    fn only_an_earlier_grant_of_the_same_path_in_file_scope_counts_as_already_granted() {
        let record = |program: &str, scope: RequestScope| GrantRecord {
            program: program.into(),
            scope,
            worker: "w1".into(),
            why: "x".into(),
            at_ms: 0,
            by: "user".into(),
        };
        let a = authority(PermissionMode::Manual, true);
        let decide_file = |c: &OpenContract| {
            decide(
                ShortfallKind::Permission,
                RequestScope::File,
                "src/new.rs",
                "needs it",
                c,
                &a,
            )
        };
        let mut c = contract();
        c.grants.push(record("src/new.rs", RequestScope::File));
        assert!(
            matches!(&decide_file(&c), ShortfallDecision::Hold { reason } if reason.contains("did not help")),
            "the same path granted before holds"
        );
        let mut c = contract();
        c.grants.push(record("src/other.rs", RequestScope::File));
        assert!(
            matches!(decide_file(&c), ShortfallDecision::AskUser { .. }),
            "another path granted before does not hold this one"
        );
        let mut c = contract();
        c.grants.push(record("src/new.rs", RequestScope::Command));
        assert!(
            matches!(decide_file(&c), ShortfallDecision::AskUser { .. }),
            "a command grant of the same text is not a path grant"
        );
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

    /// The two reports of a paid run whose worker never tried `pip`: each
    /// states an environment fact, and in an unattended Bypass run each is
    /// answered with the facts (mode, where the shell says commands run, how
    /// to install), naming the fact, instead of being amended or held.
    #[test]
    fn an_unattended_bypass_environment_report_is_answered_with_the_facts() {
        let reports = [
            (
                "The required Python dependencies (datasets and transformers) and access to the \
                 Hugging Face dataset/tokenizer are unavailable in this workspace.",
                "The script was created, but running it failed immediately with \
                 ModuleNotFoundError: No module named 'datasets'.",
                EnvironmentFact::MissingModule,
            ),
            (
                "A way to access the Hugging Face README/dataset and execute the requested \
                 standalone script",
                "The available tools provide no web access. The shell allowlist does not \
                 permit running a standalone Python script.",
                EnvironmentFact::Network,
            ),
            (
                "permission to run python3",
                "the shell allowlist does not permit running a standalone script",
                EnvironmentFact::BelievedForbidden,
            ),
        ];
        let unattended = authority(PermissionMode::Bypass, false);
        for (need, why, fact) in reports {
            assert_eq!(environment_fact(need, why), Some(fact), "{need}");
            let answer = environment_answer(need, why, &unattended).expect("answered");
            assert!(answer.contains("Nothing was amended or held"), "{answer}");
            assert!(answer.contains(fact.label()), "{answer}");
            assert!(answer.contains("permission mode is Bypass"), "{answer}");
            assert!(
                answer.contains("any command runs without approval"),
                "{answer}"
            );
        }
        let answer = environment_answer(reports[0].0, reports[0].1, &unattended).expect("answer");
        assert!(answer.contains("pip install"), "{answer}");
    }

    /// The answer is given only where it is true and nobody can answer a
    /// hold: a person attending is asked as before, and outside Bypass a
    /// command may need approval, so the report goes on to `decide`.
    #[test]
    fn an_environment_report_is_not_answered_attended_or_outside_bypass() {
        let need = "the datasets module";
        let why = "ModuleNotFoundError: No module named 'datasets'";
        assert!(environment_answer(need, why, &authority(PermissionMode::Bypass, true)).is_none());
        for mode in [
            PermissionMode::Manual,
            PermissionMode::Auto,
            PermissionMode::AcceptEdits,
            PermissionMode::OrchestratorDecides,
        ] {
            assert!(
                environment_answer(need, why, &authority(mode, false)).is_none(),
                "{mode:?}"
            );
        }
    }

    /// A genuine gap in the instruction is not an environment fact, even
    /// when it names a package or something unavailable: it still amends
    /// once and then holds in an unattended Bypass run.
    #[test]
    fn a_genuine_instruction_gap_still_amends_then_holds() {
        let gaps = [
            (
                "which package name to publish under",
                "the instruction names two and they differ",
            ),
            (
                "the expected output format",
                "the instruction does not say whether answer.txt holds a number or a sentence",
            ),
            (
                "the input file",
                "data.csv named in the instruction is unavailable in the repository",
            ),
        ];
        let unattended = authority(PermissionMode::Bypass, false);
        for (need, why) in gaps {
            assert_eq!(environment_fact(need, why), None, "{need}: {why}");
            assert!(environment_answer(need, why, &unattended).is_none());
            let mut c = contract();
            let d = |c: &OpenContract| {
                decide(
                    ShortfallKind::Instructions,
                    RequestScope::Command,
                    need,
                    why,
                    c,
                    &unattended,
                )
            };
            assert_eq!(d(&c), ShortfallDecision::Amend);
            c.shortfall_notes.push(format!("{need}: {why}"));
            assert!(matches!(d(&c), ShortfallDecision::Hold { .. }));
        }
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

    /// Decision 0100's zero-test rule: a passing narrowed cargo check the
    /// verifier marked as selecting no test leaves the contract unverified
    /// under its own reason code; a gate command beside it judges instead.
    #[test]
    fn a_narrowed_check_that_selected_no_test_is_unverified_with_its_code() {
        let r = WorkerResult::bare("t", "w", kgraf_runtime::WorkerOutcome::Completed);
        let by = "cargo test -p alpha [scoped_check_vacuous: package alpha has no recognized test target]";
        let s = accepted_standing("c1", by, false, Some(&r));
        let reason = s.unverified.expect("unverified");
        assert!(reason.starts_with("scoped_check_vacuous: "), "{reason}");
        assert!(
            reason.contains("package alpha has no recognized test target"),
            "{reason}"
        );
        assert_eq!(
            accepted_standing("c1", by, true, Some(&r)),
            AcceptedStanding::default()
        );
        assert_eq!(
            accepted_standing("c1", "cargo test -p alpha", false, Some(&r)),
            AcceptedStanding::default()
        );
    }

    /// A vacuous check is unverified, with an unretried refusal in its
    /// reason; a real check that passed is verified, and an unretried
    /// refusal beside it is a notice, never dropped (decision 0093, amended
    /// 2026-10-05). The shape seen in a real run: a worker refused
    /// `grep` of its own edit ran `cargo test`, which passed, and the
    /// contract was reported unverified.
    #[test]
    fn a_vacuous_check_is_unverified_and_a_refusal_beside_a_real_check_is_a_fact() {
        assert!(vacuous_check("true"));
        assert!(vacuous_check("  exit 0 "));
        assert!(!vacuous_check("test -f /app/answer.txt"));
        let mut r = WorkerResult::bare("t", "w", kgraf_runtime::WorkerOutcome::Completed);
        let clean = accepted_standing("c1", "cargo test", false, Some(&r));
        assert_eq!(clean, AcceptedStanding::default());
        // A remediation's gate commands ran beside a `true` acceptance.
        assert_eq!(
            accepted_standing("c1", "true", true, Some(&r)),
            AcceptedStanding::default()
        );
        assert!(
            accepted_standing("c1", "true", false, None)
                .unverified
                .is_some_and(|u| u.contains("checks nothing"))
        );

        r.refused.push(refused_program("grep", "user"));
        let real = accepted_standing("c1", "cargo test", false, Some(&r));
        assert_eq!(real.unverified, None, "{real:?}");
        assert_eq!(
            real.refusal_notice.as_deref(),
            Some(
                "contract c1: verified by `cargo test`; the worker finished after a policy \
                 refused `grep` and never ran it"
            )
        );
        // Deferred to the root gate, which must pass before the run completes.
        let deferred = accepted_standing(
            "c1",
            "cargo test (deferred to the root gate)",
            false,
            Some(&r),
        );
        assert_eq!(deferred.unverified, None, "{deferred:?}");
        assert!(deferred.refusal_notice.is_some());

        let vacuous = accepted_standing("c1", "true", false, Some(&r));
        assert_eq!(vacuous.refusal_notice, None);
        assert_eq!(
            vacuous.unverified.as_deref(),
            Some(
                "the acceptance command `true` checks nothing; the worker finished after a \
                 policy refused `grep` and never ran it"
            )
        );

        // A refusal retried successfully is neither.
        r.refused[0].retried_ok = true;
        assert_eq!(
            accepted_standing("c1", "cargo test", false, Some(&r)),
            AcceptedStanding::default()
        );
    }

    /// An attempt cancelled after its report (a held shortfall verified in
    /// scope) is never said to have finished.
    #[test]
    fn a_refusal_names_how_the_worker_ended() {
        let mut r = WorkerResult::bare("t", "w", kgraf_runtime::WorkerOutcome::Cancelled);
        r.refused.push(refused_path("Cargo.toml"));
        let s = accepted_standing("c10", "cargo test", false, Some(&r));
        let notice = s.refusal_notice.expect("a notice");
        assert!(
            notice.contains(
                "the worker was cancelled after a policy refused `apply_patch` on `Cargo.toml`"
            ),
            "{notice}"
        );
        assert!(!notice.contains("finished"), "{notice}");
        r.outcome = kgraf_runtime::WorkerOutcome::BudgetExhausted;
        let u = accepted_standing("c10", "true", false, Some(&r))
            .unverified
            .expect("unverified");
        assert!(u.contains("the worker ran out of rounds after"), "{u}");
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

    #[test]
    fn an_instructions_report_about_a_file_another_contract_owns_is_answered() {
        let own = contract_writing("c3", &["tax_us/src/lib.rs"]);
        let owner = contract_writing("c5", &["pricing_engine/src/lib.rs"]);
        let answer = instructions_answer(
            "The stub code in pricing_engine/src/lib.rs must be updated to use TaxUs correctly \
             (e.g. `TaxUs { order: default_order() }`)",
            "my struct has a field now",
            &own,
            [&own, &owner],
            None,
        )
        .expect("answered");
        assert!(
            answer.contains("`pricing_engine/src/lib.rs` belongs to contract `c5`")
                && answer.contains("Keep the public shape"),
            "{answer}"
        );
    }

    #[test]
    fn an_instructions_report_is_amended_when_it_names_no_foreign_file_or_its_own() {
        let own = contract_writing("c3", &["tax_us/src/lib.rs"]);
        let owner = contract_writing("c5", &["pricing_engine/src/lib.rs"]);
        let ask = |need: &str| instructions_answer(need, "", &own, [&own, &owner], None);
        assert_eq!(ask("the dataset is missing"), None);
        assert_eq!(
            ask("a file nobody owns, other/src/lib.rs, must change"),
            None
        );
        assert_eq!(
            ask("tax_us/src/lib.rs and pricing_engine/src/lib.rs must change"),
            None,
            "a report that names its own file is about its own work"
        );
        // `path:line:col` and a trailing period still name the path.
        assert!(ask("see pricing_engine/src/lib.rs:5:18.").is_some());
    }

    /// A file no contract of the plan writes is a plan-shape error: the hold
    /// asks for a re-plan and does not suggest `--allow-path`. A file another
    /// contract writes, and a plan not known, keep the permission wording.
    #[test]
    fn a_hold_for_a_file_no_contract_writes_asks_for_a_replan() {
        let a = authority(PermissionMode::Manual, false);
        let mine = contract_writing("c1", &["src/lib.rs"]);
        let plan = [mine.clone(), contract_writing("c2", &["src/other.rs"])];
        let ask = |need: &str, plan: Option<&[OpenContract]>| {
            decide_in_plan(
                ShortfallKind::Permission,
                RequestScope::File,
                need,
                &[],
                "the build needs it",
                &mine,
                &a,
                plan,
                &[],
            )
        };
        let ShortfallDecision::Hold { reason } = ask("docs/new.md", Some(&plan)) else {
            panic!("expected a hold");
        };
        assert!(
            reason.contains("no open contract of this plan writes it"),
            "{reason}"
        );
        assert!(reason.contains("re-plan"), "{reason}");
        assert!(reason.contains("`docs/new.md` in its writes"), "{reason}");
        assert!(!reason.contains("--allow-path"), "{reason}");
        // The plan is not known: the permission wording stays.
        let ShortfallDecision::Hold { reason } = ask("docs/new.md", None) else {
            panic!("expected a hold");
        };
        assert!(reason.contains("--allow-path docs/new.md"), "{reason}");
        // Attended: the user is asked, as before.
        let attended = authority(PermissionMode::Manual, true);
        let d = decide_in_plan(
            ShortfallKind::Permission,
            RequestScope::File,
            "docs/new.md",
            &[],
            "x",
            &mine,
            &attended,
            Some(&plan),
            &[],
        );
        assert!(matches!(d, ShortfallDecision::AskUser { .. }), "{d:?}");
    }

    /// The shared manifest case: the one contract that listed the root
    /// manifest finished and was integrated, no open contract writes it, and
    /// a later contract needs it. Nobody attends, so the path is granted
    /// (loudly, naming the finished writer). A path no contract ever listed
    /// is still held, a finished contract that was only superseded does not
    /// count, an attended run is still asked, and a user rule still wins.
    #[test]
    fn a_file_only_a_finished_contract_wrote_is_handed_to_the_asking_contract() {
        let a = authority(PermissionMode::Manual, false);
        let asking = contract_writing("c2", &["wcl/Cargo.toml", "wcl/src/main.rs"]);
        let plan = [asking.clone()];
        let done = ClosedContract::of(
            &contract_writing("c1", &["Cargo.toml", "wcl-core/src/lib.rs"]),
            Closure::Verified,
        );
        let ask = |need: &str, a: &Authority, finished: &[ClosedContract]| {
            decide_in_plan(
                ShortfallKind::Permission,
                RequestScope::File,
                need,
                &[],
                "the acceptance needs the member",
                &asking,
                a,
                Some(&plan),
                finished,
            )
        };
        let finished = [done.clone()];
        let ShortfallDecision::Grant { grant, by } = ask("Cargo.toml", &a, &finished) else {
            panic!("expected a grant");
        };
        assert_eq!(grant, Grant::Path("Cargo.toml".into()));
        assert!(by.contains("finished contract `c1`"), "{by}");
        assert!(by.contains("no open contract writes it"), "{by}");
        // A path no contract listed keeps the plan-shape hold.
        let d = ask("docs/new.md", &a, &finished);
        assert!(
            matches!(&d, ShortfallDecision::Hold { reason } if reason.contains("re-plan")),
            "{d:?}"
        );
        // A contract that was split into children did not integrate its own writes.
        let superseded = [ClosedContract::of(
            &contract_writing("c1", &["Cargo.toml"]),
            Closure::Superseded,
        )];
        let d = ask("Cargo.toml", &a, &superseded);
        assert!(matches!(d, ShortfallDecision::Hold { .. }), "{d:?}");
        // Attended: the user is asked, as before.
        let attended = authority(PermissionMode::Manual, true);
        let d = ask("Cargo.toml", &attended, &finished);
        assert!(matches!(d, ShortfallDecision::AskUser { .. }), "{d:?}");
    }

    /// A user deny or ask rule on the path comes before the handover.
    #[test]
    fn a_user_rule_wins_over_the_handover_of_a_finished_contracts_file() {
        let asking = contract_writing("c2", &["wcl/Cargo.toml"]);
        let plan = [asking.clone()];
        let finished = [ClosedContract::of(
            &contract_writing("c1", &["Cargo.toml"]),
            Closure::Verified,
        )];
        for (deny, ask) in [
            (
                vec![Rule::parse("apply_patch(Cargo.toml)").expect("rule")],
                vec![],
            ),
            (
                vec![],
                vec![Rule::parse("apply_patch(Cargo.toml)").expect("rule")],
            ),
        ] {
            let a = Authority {
                mode: PermissionMode::Manual,
                allow: AllowSet::default(),
                rules: UserRules { deny, ask },
                attended: false,
            };
            let d = decide_in_plan(
                ShortfallKind::Permission,
                RequestScope::File,
                "Cargo.toml",
                &[],
                "x",
                &asking,
                &a,
                Some(&plan),
                &finished,
            );
            assert!(
                matches!(&d, ShortfallDecision::Hold { reason } if reason.contains("rule")),
                "{d:?}"
            );
        }
    }

    /// Soak 3: the worker read the skill's reply and reported the shell before
    /// any call. Nothing was refused, so nothing is granted or held, but the
    /// answer names the restriction that would refuse the call and gives no
    /// advice about a malformed call, since none failed.
    #[tokio::test]
    async fn a_report_of_a_tool_the_active_skill_would_refuse_names_the_skill() {
        let (_session, skills, _w) = restricted();
        let input = serde_json::json!({
            "kind": "permission", "scope": "tool", "need": "shell",
            "why": "the instruction asks for a grep"
        });
        let (control, out) = drive(record(vec![]), Some(skills.clone()), input, loop_like).await;
        let Some(Grounding::NothingRefused { answer }) = grounding_of(&control) else {
            panic!("{control:?}");
        };
        assert!(answer.contains("Nothing was granted or held"), "{answer}");
        assert!(answer.contains("read-only-audit"), "{answer}");
        assert!(answer.contains("`shell`"), "{answer}");
        assert!(answer.contains("cannot lift"), "{answer}");
        assert!(!answer.contains("malformed"), "no call failed: {answer}");
        // No refusal is carried, so no hold can follow from it.
        let ContractControl::Shortfall { skill_refusal, .. } = &control else {
            panic!("{control:?}");
        };
        assert!(skill_refusal.is_none(), "{control:?}");
        let out = out.expect("answered");
        assert!(!out.is_error);
        // A failed input is still named, with the retry advice, beside the restriction.
        let mut r = record(vec![]);
        r.last_input_error = Some("ERROR: invalid call: bad patch".into());
        let input = serde_json::json!({
            "kind": "permission", "scope": "tool", "need": "shell", "why": "x"
        });
        let (control, _) = drive(r, Some(skills), input, loop_like).await;
        let Some(Grounding::NothingRefused { answer }) = grounding_of(&control) else {
            panic!("{control:?}");
        };
        assert!(answer.contains("bad patch"), "{answer}");
    }

    /// With no skill involved and no failed call, the answer to a report that
    /// names nothing refused does not tell the worker to fix a malformed call.
    #[test]
    fn nothing_refused_and_no_failed_call_gives_no_retry_advice() {
        let Grounding::NothingRefused { answer } =
            ground(RequestScope::Command, "curl", &record(vec![]))
        else {
            panic!("nothing was refused");
        };
        assert!(answer.ends_with(" Continue your work."), "{answer}");
        assert!(!answer.contains("malformed"), "{answer}");
    }
}
