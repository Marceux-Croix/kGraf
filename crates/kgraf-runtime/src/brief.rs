//! What a worker is given, and what it returns.

use crate::thresholds::ContextConfig;
use kgraf_core::{AgentId, CallId, NodeId, TaskId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The smallest step budget: one working round plus the final round the loop holds
/// back for an answer.
pub const MIN_STEP_BUDGET: u32 = 2;

/// A worker's assignment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerBrief {
    pub task: TaskId,
    pub worker: AgentId,
    /// The instruction, verbatim. Pinned in the private remainder and never
    /// compacted or paraphrased (0057).
    pub instruction: String,
    /// The worker's own git worktree (disjoint writes).
    pub workspace_root: PathBuf,
    /// The model to call. Must be one the configured provider serves.
    pub model: String,
    /// Graph nodes whose slices form the private remainder (0043). The shared
    /// prefix is assembled by the runtime, not listed here.
    pub slices: Vec<NodeId>,
    /// Skills activated for this worker, by name.
    pub skills: Vec<String>,
    /// Model rounds allowed, including the final answer round.
    pub step_budget: u32,
    /// Context thresholds for the model (0057 addendum 2).
    pub context: ContextConfig,
    /// The paths, relative to the worktree root, the worker may write. A call
    /// declaring a write outside it is refused before it runs. `None` sets no
    /// scope; writes under `.git` or `.kgraf` are refused either way.
    #[serde(default)]
    pub write_scope: Option<Vec<PathBuf>>,
}

/// A brief that cannot be run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BriefError {
    #[error("the instruction is empty")]
    EmptyInstruction,
    #[error("no model is named")]
    EmptyModel,
    #[error("a step budget of {budget} leaves no working round before the answer round")]
    StepBudgetTooSmall { budget: u32 },
    #[error("a handoff lists nothing remaining; that is a completion")]
    HandoffWithoutRemainder,
}

impl WorkerBrief {
    /// Check the brief before a worker is spawned on it.
    pub fn validate(&self) -> Result<(), BriefError> {
        if self.instruction.trim().is_empty() {
            return Err(BriefError::EmptyInstruction);
        }
        if self.model.trim().is_empty() {
            return Err(BriefError::EmptyModel);
        }
        if self.step_budget < MIN_STEP_BUDGET {
            return Err(BriefError::StepBudgetTooSmall {
                budget: self.step_budget,
            });
        }
        Ok(())
    }
}

/// What a worker hands back when its context reaches the stop point (0057: the
/// orchestrator re-splits the remainder). A worker that cannot hand back
/// compacts once instead, and hands back at a second overflow (0095).
///
/// Structured so the orchestrator can re-plan from it without reading the worker's
/// transcript: what is done, what is left, which files the worktree already holds
/// changes to, and how full the context was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// Items finished, each a short statement the verifier can check.
    pub done: Vec<String>,
    /// Items left, each small enough to brief a fresh worker with.
    pub remaining: Vec<String>,
    /// Paths changed in the worktree so far, relative to its root.
    pub files_touched: Vec<PathBuf>,
    /// Context tokens in use when the worker stopped.
    pub context_tokens: u64,
}

impl Handoff {
    /// A handoff with nothing remaining is a completion, not a handoff; reporting
    /// it as one would hide that the work is done.
    pub fn validate(&self) -> Result<(), BriefError> {
        if self.remaining.iter().all(|r| r.trim().is_empty()) {
            return Err(BriefError::HandoffWithoutRemainder);
        }
        Ok(())
    }
}

/// How a worker's run ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkerOutcome {
    /// The model ended its turn with an answer.
    Completed,
    /// The context reached the worker stop point; the remainder is handed back.
    HandedBack { handoff: Handoff },
    /// The step budget ran out.
    BudgetExhausted,
    /// The same tool call repeated inside the budget (0056 stall detection).
    Stalled { call_digest: String, repeats: u32 },
    /// The worker was cancelled.
    Cancelled,
    /// The run failed.
    Failed { error: String },
}

/// Word cap the return instructions ask a worker to keep its rationale
/// (`Rationale`'s three fields, together) under. A starting point, not a
/// measured number: it bounds what a dependent contract's brief later quotes.
pub const RATIONALE_WORD_CAP: usize = 250;

/// Appended where the word cap cut a rationale field.
const RATIONALE_CUT_MARK: &str = " [rationale cut at the word cap]";
/// The markers a worker's final answer wraps a rationale in (`parse_rationale`).
/// Plain text, not a tool call: reasoning blocks never cross models or vendors
/// (0005), so a rationale has to be something the model writes into its
/// ordinary answer, in the model's own words.
pub(crate) const RATIONALE_OPEN: &str = "<<<RATIONALE>>>";
pub(crate) const RATIONALE_CLOSE: &str = "<<<END RATIONALE>>>";
pub(crate) const DECISIONS_LABEL: &str = "Decisions:";
pub(crate) const ALTERNATIVES_LABEL: &str = "Alternatives rejected:";
pub(crate) const ASSUMPTIONS_LABEL: &str = "Assumptions about counterparts:";

/// A worker's own account of its reasons, in prose: what it decided and why,
/// what it rejected and why, and what it assumed the contracts it depends on
/// or that depend on it deliver or consume (0026: labeled as derived text
/// when it crosses to another worker; see `producer_rationale_brief` in
/// `kgraf-orchestrator`, which quotes it as data, never as instructions).
///
/// This is model-written prose, not a reasoning block: decision 0005 forbids
/// replaying a reasoning block to another model, but a written rationale may
/// cross vendors because it is the same kind of text as any other agent
/// conclusion (0026).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rationale {
    pub decisions: String,
    pub alternatives_rejected: String,
    pub counterpart_assumptions: String,
}

impl Rationale {
    fn is_empty(&self) -> bool {
        self.decisions.trim().is_empty()
            && self.alternatives_rejected.trim().is_empty()
            && self.counterpart_assumptions.trim().is_empty()
    }

    /// Trims the three fields, in order, to stay within `RATIONALE_WORD_CAP`
    /// words total, marking a cut rather than silently dropping the excess
    /// (fail loud: silent degradation is a defect).
    fn capped(mut self) -> Self {
        let mut budget = RATIONALE_WORD_CAP;
        for field in [
            &mut self.decisions,
            &mut self.alternatives_rejected,
            &mut self.counterpart_assumptions,
        ] {
            if budget == 0 {
                // The cap was spent exactly by an earlier field, or a cut
                // spent it: a later field with text is marked, never blanked.
                if !field.trim().is_empty() {
                    *field = RATIONALE_CUT_MARK.trim().to_owned();
                }
                continue;
            }
            let words: Vec<&str> = field.split_whitespace().collect();
            if words.len() > budget {
                let mut kept = words[..budget].join(" ");
                kept.push_str(RATIONALE_CUT_MARK);
                *field = kept;
                budget = 0;
            } else {
                budget -= words.len();
            }
        }
        self
    }
}

/// Pulls a worker's self-reported rationale out of its final answer, if it
/// gave one: the return instructions ask for a block between `<<<RATIONALE>>>`
/// and `<<<END RATIONALE>>>`, with the three labels above. `None` when the
/// block is absent, malformed, or every field is blank -- recorded as absent,
/// never fabricated by kGraf.
pub fn parse_rationale(answer: &str) -> Option<Rationale> {
    let start = answer.find(RATIONALE_OPEN)? + RATIONALE_OPEN.len();
    let rest = &answer[start..];
    let end = rest.find(RATIONALE_CLOSE)?;
    let body = &rest[..end];

    let labels = [
        (DECISIONS_LABEL, 0usize),
        (ALTERNATIVES_LABEL, 1usize),
        (ASSUMPTIONS_LABEL, 2usize),
    ];
    let mut spans: Vec<(usize, usize, usize)> = labels
        .iter()
        .filter_map(|(label, idx)| body.find(label).map(|pos| (pos, pos + label.len(), *idx)))
        .collect();
    spans.sort_by_key(|(pos, _, _)| *pos);

    let mut fields = [String::new(), String::new(), String::new()];
    for (i, (_, text_start, idx)) in spans.iter().enumerate() {
        let text_end = spans
            .get(i + 1)
            .map(|(pos, _, _)| *pos)
            .unwrap_or(body.len());
        fields[*idx] = body[*text_start..text_end].trim().to_owned();
    }

    let rationale = Rationale {
        decisions: fields[0].clone(),
        alternatives_rejected: fields[1].clone(),
        counterpart_assumptions: fields[2].clone(),
    };
    if rationale.is_empty() {
        None
    } else {
        Some(rationale.capped())
    }
}

/// A worker's structured result: small, for the orchestrator to act on.
///
/// Token usage is not carried. The ledger holds it per call, and `calls` names the
/// rows, so there is one place to look.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub task: TaskId,
    pub worker: AgentId,
    pub outcome: WorkerOutcome,
    /// The model's final answer, or empty when there is none.
    pub answer: String,
    /// Paths changed in the worktree, relative to its root.
    pub changed_paths: Vec<PathBuf>,
    /// The worktree commit that holds the change, if the worker committed.
    pub commit: Option<String>,
    /// Every provider call this run made, in order.
    pub calls: Vec<CallId>,
    /// Model rounds used.
    pub steps: u32,
    /// The shared prefix digest every call of this run was checked against
    /// (0043). Workers on one vendor and model report the same digest.
    #[serde(default)]
    pub prefix_digest: String,
    /// Tool calls the approval chain denied. Counted apart from stall detection:
    /// a denied call did nothing, so repeating one is not a stall.
    #[serde(default)]
    pub denied: u32,
    /// Trace writes the ledger refused (time to first token, a tool round),
    /// each with its call and reason. The run goes on, since a trace field is
    /// not spend, but the loss is reported, never swallowed (every token is attributed, fail loud).
    #[serde(default)]
    pub trace_failures: Vec<String>,
    /// The worker's own account of its reasons, parsed from its final answer
    /// (`parse_rationale`), if it gave one. `None` when it did not: recorded
    /// as absent, never fabricated. Serde-defaulted so a record from before
    /// this field existed still loads.
    #[serde(default)]
    pub rationale: Option<Rationale>,
    /// Calls a policy refused (the write scope, the command policy, a skill's
    /// `allowed-tools`, an unattended approval, a fence), each with whether a
    /// later call of the same capability ran without error (decision 0093).
    /// An attempt that finished with an unretried refusal is reported
    /// unverified. Serde-defaulted so an older record still loads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<crate::shortfall::RefusedCall>,
    /// The runtime's compaction notice, when this worker compacted (decision
    /// 0095). Set by the loop alone, never from model output; the orchestrator
    /// reads it as "the contract was larger than planned". Serde-defaulted so
    /// an older record still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<crate::compaction::CompactionNotice>,
}

impl WorkerResult {
    /// A result that carries an outcome and nothing else: no answer, changes,
    /// commit or calls, and no rounds used. For a worker that never ran or
    /// never reported; set the fields it does have with struct update.
    pub fn bare(
        task: impl Into<TaskId>,
        worker: impl Into<AgentId>,
        outcome: WorkerOutcome,
    ) -> Self {
        Self {
            task: task.into(),
            worker: worker.into(),
            outcome,
            answer: String::new(),
            changed_paths: Vec::new(),
            commit: None,
            calls: Vec::new(),
            steps: 0,
            prefix_digest: String::new(),
            denied: 0,
            trace_failures: Vec::new(),
            rationale: None,
            refused: Vec::new(),
            compaction: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief() -> WorkerBrief {
        WorkerBrief {
            task: "t1".into(),
            worker: "w1".into(),
            instruction: "rename foo to bar".into(),
            workspace_root: PathBuf::from("/tmp/wt"),
            model: "m".into(),
            slices: vec![NodeId::new("n1")],
            skills: vec![],
            step_budget: 10,
            context: ContextConfig::for_model(200_000).expect("valid"),
            write_scope: None,
        }
    }

    #[test]
    fn a_complete_brief_validates() {
        brief().validate().expect("valid");
    }

    #[test]
    fn empty_instruction_empty_model_and_a_budget_without_a_working_round_are_refused() {
        let mut b = brief();
        b.instruction = "  ".into();
        assert_eq!(b.validate(), Err(BriefError::EmptyInstruction));
        let mut b = brief();
        b.model = String::new();
        assert_eq!(b.validate(), Err(BriefError::EmptyModel));
        let mut b = brief();
        b.step_budget = 1;
        assert_eq!(
            b.validate(),
            Err(BriefError::StepBudgetTooSmall { budget: 1 })
        );
        b.step_budget = MIN_STEP_BUDGET;
        assert!(b.validate().is_ok());
    }

    #[test]
    fn a_handoff_carries_done_remaining_files_and_tokens_and_must_leave_something() {
        let handoff = Handoff {
            done: vec!["parser ported".into()],
            remaining: vec!["wire parser into CLI".into()],
            files_touched: vec![PathBuf::from("src/parser.rs")],
            context_tokens: 80_000,
        };
        handoff.validate().expect("valid");
        let outcome = WorkerOutcome::HandedBack {
            handoff: handoff.clone(),
        };
        let text = serde_json::to_string(&outcome).expect("ser");
        assert!(text.contains(r#""outcome":"handed_back""#));
        assert!(text.contains("wire parser into CLI"));
        assert_eq!(
            serde_json::from_str::<WorkerOutcome>(&text).expect("de"),
            outcome
        );
        let empty = Handoff {
            remaining: vec![" ".into()],
            ..handoff
        };
        assert_eq!(empty.validate(), Err(BriefError::HandoffWithoutRemainder));
    }

    #[test]
    fn a_result_round_trips_through_json() {
        let result = WorkerResult {
            changed_paths: vec![PathBuf::from("src/lib.rs")],
            calls: vec![CallId::new("c1")],
            steps: 4,
            prefix_digest: "d".into(),
            ..WorkerResult::bare(
                "t1",
                "w1",
                WorkerOutcome::Stalled {
                    call_digest: "abc".into(),
                    repeats: 3,
                },
            )
        };
        let text = serde_json::to_string(&result).expect("ser");
        assert!(text.contains(r#""outcome":"stalled""#));
        let back: WorkerResult = serde_json::from_str(&text).expect("de");
        assert_eq!(back, result);
    }

    /// A record saved before `rationale` existed still loads, as `None`
    /// (serde-default keeps old records readable).
    #[test]
    fn a_result_recorded_before_rationale_existed_loads_with_none() {
        let old = r#"{
            "task": "t1",
            "worker": "w1",
            "outcome": {"outcome": "completed"},
            "answer": "done",
            "changed_paths": [],
            "commit": null,
            "calls": [],
            "steps": 1,
            "prefix_digest": "d",
            "denied": 0,
            "trace_failures": []
        }"#;
        let back: WorkerResult = serde_json::from_str(old).expect("de");
        assert_eq!(back.rationale, None);
    }

    #[test]
    fn a_rationale_block_is_parsed_out_of_the_final_answer() {
        let answer = "Here is my answer.\n\n<<<RATIONALE>>>\n\
            Decisions: used a fold, not a loop.\n\
            Alternatives rejected: a second pass, too slow.\n\
            Assumptions about counterparts: the consumer reads the field verbatim.\n\
            <<<END RATIONALE>>>";
        let r = parse_rationale(answer).expect("parsed");
        assert_eq!(r.decisions, "used a fold, not a loop.");
        assert_eq!(r.alternatives_rejected, "a second pass, too slow.");
        assert_eq!(
            r.counterpart_assumptions,
            "the consumer reads the field verbatim."
        );
    }

    /// A worker that omits the block is recorded as absent, never fabricated.
    #[test]
    fn an_answer_without_the_block_parses_to_none() {
        assert_eq!(parse_rationale("just an ordinary answer"), None);
        assert_eq!(
            parse_rationale("<<<RATIONALE>>>\n   \n<<<END RATIONALE>>>"),
            None
        );
    }

    #[test]
    fn a_rationale_over_the_word_cap_is_cut_and_marked_not_silently_dropped() {
        let long = "word ".repeat(RATIONALE_WORD_CAP + 50);
        let answer = format!(
            "<<<RATIONALE>>>\nDecisions: {long}\nAlternatives rejected: none.\n\
             Assumptions about counterparts: none.\n<<<END RATIONALE>>>"
        );
        let r = parse_rationale(&answer).expect("parsed");
        assert!(r.decisions.ends_with("[rationale cut at the word cap]"));
        // The later fields had text; they are marked as cut, not blanked.
        assert_eq!(r.alternatives_rejected, "[rationale cut at the word cap]");
        assert_eq!(r.counterpart_assumptions, "[rationale cut at the word cap]");
    }

    #[test]
    fn a_field_that_spends_the_cap_exactly_does_not_blank_the_next_one() {
        let exact = "word ".repeat(RATIONALE_WORD_CAP);
        let answer = format!(
            "<<<RATIONALE>>>\nDecisions: {exact}\nAlternatives rejected: a real alternative.\n\
             <<<END RATIONALE>>>"
        );
        let r = parse_rationale(&answer).expect("parsed");
        assert_eq!(r.decisions.split_whitespace().count(), RATIONALE_WORD_CAP);
        assert!(!r.decisions.contains("cut"), "the first field fits");
        assert_eq!(r.alternatives_rejected, "[rationale cut at the word cap]");
        assert_eq!(r.counterpart_assumptions, "", "an empty field stays empty");
    }
}
