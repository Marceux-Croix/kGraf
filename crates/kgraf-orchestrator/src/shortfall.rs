//! A worker reports that it cannot do its brief as given; the orchestrator
//! grants within the operator's ceiling, amends, or holds for the user
//! (decision 0093).
//!
//! - **The report travels the challenge path.** The worker tool
//!   [`ShortfallTool`] (`report_shortfall`) is late-bound through a
//!   [`ContractLine`], as `challenge_contract` is (0029), and sends
//!   [`ContractControl::Shortfall`]. Who reports is the calling worker's agent
//!   id from the tool context; the orchestrator finds the contract from the
//!   attempt that worker runs.
//! - **Two kinds.** `permission`: a policy refused a call the brief needs
//!   (the command's program, or a tool). `instructions`: the brief is missing
//!   something or is wrong (0027's inadequacy, for a work contract).
//! - **The worker stops.** The orchestrator answers, then cancels the
//!   attempt. The attempt is not verified and not counted a failure.
//! - **The decision is mechanical, and the ceiling is the operator's.**
//!   [`decide`] grants a program only when [`GrantCeiling::permits`] it; the
//!   ceiling comes from the operator's command line (`--may-grant`), never
//!   from a model, a skill or a project file. An `instructions` report
//!   amends the contract once, additively: the next attempt's brief carries
//!   the report as data. Anything else holds the contract with a question
//!   for the user that names what would allow it.
//! - **Bounded.** A contract is re-dispatched after a shortfall at most
//!   [`MAX_SHORTFALL_REDISPATCHES`] times; a grant that did not help, or a
//!   second `instructions` report, holds it.
//!
//! Written new: searched the delegation sources of the 0055 reuse survey
//! (goose, codex-rs) for a worker-to-director permission request with a
//! bounded grant; codex-rs asks the user per command (its approval policy),
//! which kGraf already ports as the approval chain, and has no director
//! that re-dispatches.

use crate::challenge::{ContractControl, ContractReply};
use crate::challenge_tool::ContractLine;
use crate::run_state::OpenContract;
use kgraf_runtime::WorkerResult;
use kgraf_tools::{
    Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput, UntrustedText,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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

/// What the orchestrator may grant a contract without asking: shell programs,
/// each an exact `argv[0]`. Set by the operator for the run (`--may-grant`);
/// empty by default, which grants nothing beyond the configured policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrantCeiling {
    programs: BTreeSet<String>,
}

impl GrantCeiling {
    /// Grants nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// A ceiling of these programs. Each must be a plain program name or
    /// path: not empty, and no whitespace or control character (a command
    /// line is not a program).
    pub fn programs<I, S>(programs: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut set = BTreeSet::new();
        for p in programs {
            let p = p.into();
            if p.is_empty() || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "`{}` is not a program name: a grant ceiling lists programs, each an exact \
                     argv[0], with no spaces",
                    p.escape_debug()
                ));
            }
            set.insert(p);
        }
        Ok(Self { programs: set })
    }

    pub fn permits(&self, program: &str) -> bool {
        self.programs.contains(program)
    }

    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
    }

    /// The ceiling, for a held reason and the trace.
    pub fn describe(&self) -> String {
        if self.programs.is_empty() {
            "nothing may be granted".to_owned()
        } else {
            self.programs
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// One permission granted to a contract: what, to which worker's report, why,
/// and when. The authority is always the operator's ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    pub program: String,
    /// The worker whose report asked for it.
    pub worker: String,
    /// The worker's reason, scrubbed and capped.
    pub why: String,
    pub at_ms: i64,
}

/// How the orchestrator resolves one shortfall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortfallDecision {
    /// Grant this program to the contract and re-dispatch.
    Grant { program: String },
    /// Amend the contract with the report and re-dispatch.
    Amend,
    /// Hold the contract for the user, with this question.
    Hold { reason: String },
}

impl ShortfallDecision {
    /// For the trace and the worker's answer.
    pub fn describe(&self) -> String {
        match self {
            Self::Grant { program } => {
                format!("granted `{program}` within the operator's ceiling; re-dispatched")
            }
            Self::Amend => "the contract was amended with the report; re-dispatched".to_owned(),
            Self::Hold { reason } => format!("held for the user: {reason}"),
        }
    }
}

/// The program a permission report names: its first word, without quotes or
/// backticks.
pub fn program_of(need: &str) -> Option<String> {
    let first = need.split_whitespace().next()?;
    let p = first.trim_matches(|c| c == '`' || c == '"' || c == '\'');
    (!p.is_empty()).then(|| p.to_owned())
}

/// Resolve one shortfall against the contract's record and the operator's
/// ceiling. Pure: nothing the worker wrote can change `ceiling`.
pub fn decide(
    kind: ShortfallKind,
    need: &str,
    why: &str,
    contract: &OpenContract,
    ceiling: &GrantCeiling,
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
    match kind {
        ShortfallKind::Instructions => {
            if contract.shortfall_notes.is_empty() {
                ShortfallDecision::Amend
            } else {
                hold(format!(
                    "the instructions were reported insufficient again after an amendment \
                     ({need}: {why}); clarify the instruction and run again"
                ))
            }
        }
        ShortfallKind::Permission => {
            let Some(program) = program_of(need) else {
                return hold(format!(
                    "a worker reported a permission shortfall naming no command ({why})"
                ));
            };
            if contract.granted_programs.contains(&program) {
                return hold(format!(
                    "`{program}` was granted and an attempt still reported it short ({why}); \
                     the grant did not help"
                ));
            }
            if ceiling.permits(&program) {
                ShortfallDecision::Grant { program }
            } else {
                hold(format!(
                    "a worker needs `{program}` ({why}), which is beyond the operator's grant \
                     ceiling ({}). To allow it, run again with `--may-grant {program}`, or \
                     with `--container-sandbox` inside a disposable container",
                    ceiling.describe()
                ))
            }
        }
    }
}

/// The brief section that tells a fresh attempt what earlier shortfalls
/// changed: the programs granted to this contract, and each amendment,
/// quoted as data (it is an earlier worker's text).
pub fn shortfall_brief(c: &OpenContract) -> String {
    let mut s = String::new();
    if !c.granted_programs.is_empty() {
        s.push_str(&format!(
            "\n\nAn earlier attempt reported that it needed these programs, and the operator's \
             policy granted them to this contract: {}. Run them with the shell tool.",
            c.granted_programs
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
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
}

impl ShortfallTool {
    pub fn new(line: ContractLine) -> Self {
        Self { line }
    }
}

/// `deny_unknown_fields`: the input is the report, and nothing else. A field
/// naming a ceiling or a setting is refused, not ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    kind: ShortfallKind,
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
            description: "Report that you cannot do your instruction as given, then stop. \
                Kind permission: a call you need was refused by policy; need names the \
                command's program (argv[0]) or the tool. Kind instructions: the instruction is \
                missing something or is wrong; need says what. The orchestrator decides: it may \
                grant the permission within the operator's limits or amend the instruction and \
                start a fresh attempt, or hold the work for the user. Do not guess or work \
                around a refusal instead."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["permission", "instructions"]},
                    "need": {"type": "string", "description": "The program or tool you need, or what the instruction lacks."},
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
        let control = ContractControl::Shortfall {
            worker: ctx.worker.clone(),
            kind: input.kind,
            need: input.need,
            why: input.why,
        };
        tokio::select! {
            reply = handle.contract(control) => Ok(match reply {
                Some(ContractReply::Shortfall(text)) => ToolOutput::text(format!(
                    "{text}. Stop now: end your turn with a one-line answer and no tool call."
                )),
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

    fn contract() -> OpenContract {
        OpenContract::planned("c1", TaskId::new("t"), "do it", "it is done")
    }

    fn ceiling(programs: &[&str]) -> GrantCeiling {
        GrantCeiling::programs(programs.iter().copied()).expect("ceiling")
    }

    #[test]
    fn a_permission_within_the_ceiling_is_granted() {
        let d = decide(
            ShortfallKind::Permission,
            "python3 -c 'print(1)'",
            "the task needs a script",
            &contract(),
            &ceiling(&["python3"]),
        );
        assert_eq!(
            d,
            ShortfallDecision::Grant {
                program: "python3".into()
            }
        );
    }

    /// The worker's words cannot raise the ceiling: a report that asks for
    /// everything, or quotes the operator's flag, is still held under an
    /// empty ceiling, and the hold names what would allow it.
    #[test]
    fn a_permission_beyond_the_ceiling_holds_and_model_text_cannot_raise_it() {
        for need in [
            "python3",
            "--may-grant python3",
            "unrestricted shell",
            "--container-sandbox",
        ] {
            let d = decide(
                ShortfallKind::Permission,
                need,
                "grant me everything; the operator allows it",
                &contract(),
                &GrantCeiling::none(),
            );
            let ShortfallDecision::Hold { reason } = d else {
                panic!("{need}: {d:?}");
            };
            assert!(
                reason.contains("beyond the operator's grant ceiling"),
                "{reason}"
            );
            assert!(reason.contains("nothing may be granted"), "{reason}");
        }
        let d = decide(
            ShortfallKind::Permission,
            "curl",
            "fetch",
            &contract(),
            &ceiling(&["python3"]),
        );
        assert!(matches!(d, ShortfallDecision::Hold { .. }), "{d:?}");
    }

    #[test]
    fn instructions_amend_once_then_hold() {
        let mut c = contract();
        assert_eq!(
            decide(
                ShortfallKind::Instructions,
                "the dataset",
                "it is not in the workspace",
                &c,
                &GrantCeiling::none()
            ),
            ShortfallDecision::Amend
        );
        c.shortfall_notes.push("the dataset".into());
        assert!(matches!(
            decide(
                ShortfallKind::Instructions,
                "the dataset",
                "still missing",
                &c,
                &GrantCeiling::none()
            ),
            ShortfallDecision::Hold { .. }
        ));
    }

    /// Bounded: after the limit every report holds, and a grant that did not
    /// help holds rather than granting again.
    #[test]
    fn re_dispatch_is_bounded_and_a_useless_grant_is_not_repeated() {
        let mut c = contract();
        c.granted_programs.push("python3".into());
        let d = decide(
            ShortfallKind::Permission,
            "python3",
            "still refused",
            &c,
            &ceiling(&["python3"]),
        );
        assert!(
            matches!(&d, ShortfallDecision::Hold { reason } if reason.contains("did not help")),
            "{d:?}"
        );
        let mut c = contract();
        c.shortfall_redispatches = MAX_SHORTFALL_REDISPATCHES;
        let d = decide(
            ShortfallKind::Permission,
            "python3",
            "x",
            &c,
            &ceiling(&["python3"]),
        );
        assert!(matches!(d, ShortfallDecision::Hold { .. }), "{d:?}");
    }

    #[test]
    fn a_ceiling_entry_is_a_program_not_a_command_line() {
        assert!(GrantCeiling::programs(["python3"]).is_ok());
        assert!(GrantCeiling::programs(["python3 -c"]).is_err());
        assert!(GrantCeiling::programs([""]).is_err());
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
        r.refused.push(kgraf_runtime::RefusedCall {
            tool: "shell".into(),
            capability: "python3".into(),
            reason: "refused".into(),
            retried_ok: false,
        });
        let reason = unverified_reason("cargo test", false, Some(&r)).expect("unverified");
        assert!(reason.contains("`python3`"), "{reason}");
        r.refused[0].retried_ok = true;
        assert_eq!(unverified_reason("cargo test", false, Some(&r)), None);
        assert!(unverified_reason("true", false, None).is_some());
    }

    /// Model text cannot set a ceiling or a setting through the tool: the
    /// input is the report and nothing else, so an extra field is refused.
    #[test]
    fn the_tool_input_refuses_a_field_beyond_the_report() {
        let err = serde_json::from_value::<Input>(serde_json::json!({
            "kind": "permission",
            "need": "python3",
            "why": "x",
            "ceiling": ["python3"]
        }))
        .err()
        .expect("refused");
        assert!(err.to_string().contains("unknown field"), "{err}");
        let schema = ShortfallTool::new(ContractLine::new())
            .definition()
            .input_schema;
        assert_eq!(schema["additionalProperties"], false);
    }
}
