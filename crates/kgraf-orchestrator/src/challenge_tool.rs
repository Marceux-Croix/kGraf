//! The worker's side of a challenge (0029): the built-in `challenge_contract`
//! tool, through which a running worker sends back an interface contract it
//! believes is wrong, and reads how the orchestrator resolved it (0035-0037).
//!
//! - **Who challenges is not the worker's word.** The tool sends
//!   `ContractControl::WorkerChallenge` with the calling worker's agent id from
//!   the tool context; the orchestrator derives the contract and the vendor from
//!   the attempt that worker runs. A model cannot claim another lineage.
//! - **The answer is the recorded resolution**: a rejection names its artifact
//!   or says it is judgment (0037); an upheld challenge says the contract was
//!   overturned; a re-raise the record cannot settle says it went to the user.
//! - **Late-bound.** Worker environments are assembled before the orchestrator
//!   exists (their tool list is part of the prefix it sizes against), so the tool
//!   holds a [`ContractLine`] that the host binds to the orchestrator's handle
//!   once it is built. A call on an unbound line is an error the model sees,
//!   never a silent success.
//! - **One definition for every worker.** The tool is registered in every
//!   environment of a run with contract checks, so every worker on one vendor and
//!   model keeps a byte-identical prefix (0043).
//!
//! Written new: challenges are kGraf's own design (0055). Searched:
//! the delegation sources of the 0055 reuse survey (goose, codex-rs) have no
//! worker-to-director dispute channel.

use crate::challenge::{ContractControl, ContractReply, Evidence, Resolution};
use crate::orchestrator::OrchestratorHandle;
use kgraf_tools::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use std::sync::{Arc, OnceLock};

/// The tool's name, as workers call it.
pub const CHALLENGE_TOOL: &str = "challenge_contract";

/// The orchestrator a run's challenge tools reach: bound once, after the
/// orchestrator is built. Cloning shares the binding.
#[derive(Clone, Default)]
pub struct ContractLine(Arc<OnceLock<OrchestratorHandle>>);

impl ContractLine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the line. A second binding is refused: one run's tools answer to one
    /// orchestrator.
    pub fn bind(&self, handle: OrchestratorHandle) -> Result<(), String> {
        self.0
            .set(handle)
            .map_err(|_| "the contract line is already bound to an orchestrator".to_owned())
    }

    pub fn is_bound(&self) -> bool {
        self.0.get().is_some()
    }
}

/// `challenge_contract`: challenge an interface contract (0029).
pub struct ChallengeTool {
    line: ContractLine,
}

impl ChallengeTool {
    pub fn new(line: ContractLine) -> Self {
        Self { line }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    interface: String,
    claim: String,
    #[serde(default = "no_evidence")]
    evidence: Evidence,
}

fn no_evidence() -> Evidence {
    Evidence::None
}

/// The reply as the worker reads it.
fn render(reply: ContractReply) -> ToolOutput {
    match reply {
        ContractReply::Resolved(Resolution::Rejected { basis, reason }) => {
            ToolOutput::text(Resolution::rejection_text(&basis, &reason))
        }
        ContractReply::Resolved(Resolution::SelfOverturn { artifact, reason }) => {
            ToolOutput::text(format!(
                "upheld: the orchestrator overturned the contract on {artifact}: {reason}. \
                 The contract is under review; finish only work that does not depend on it."
            ))
        }
        ContractReply::Resolved(Resolution::Escalated { .. }) => ToolOutput::text(
            "escalated to the user, who decides; the contract's parties are held until then",
        ),
        ContractReply::Refused(why) => ToolOutput::error(format!("challenge refused: {why}")),
        other => ToolOutput::error(format!("unexpected answer to a challenge: {other:?}")),
    }
}

#[async_trait::async_trait]
impl Tool for ChallengeTool {
    fn name(&self) -> &str {
        CHALLENGE_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: CHALLENGE_TOOL.into(),
            description: "Challenge an interface contract named in your brief when you believe \
                the contract itself is wrong, not your work. The orchestrator first checks your \
                evidence against the record. Evidence kinds: not_consulted (item: a repository \
                file the contract was written without), recompute (the contract's own checks \
                disagree with it: they are run again), quote (artifact: a repository file; \
                text: an exact passage of it the contract contradicts), none (judgment). The \
                answer names the artifact that settled it, or says it is judgment; re-raising a \
                rejection escalates to the user only with new evidence."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "interface": {"type": "string", "description": "The interface id."},
                    "claim": {"type": "string", "description": "What is wrong, in prose."},
                    "evidence": {
                        "type": "object",
                        "properties": {
                            "kind": {"enum": ["not_consulted", "recompute", "quote", "none"]},
                            "item": {"type": "string"},
                            "artifact": {"type": "string"},
                            "text": {"type": "string"}
                        },
                        "required": ["kind"]
                    }
                },
                "required": ["interface", "claim"],
                "additionalProperties": false
            }),
        }
    }

    /// Touches no file of the worktree: the orchestrator reads the artifacts,
    /// in the repository, and runs any recompute in its own sandbox.
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
            tool: CHALLENGE_TOOL.into(),
            reason: e.to_string(),
        })?;
        let Some(handle) = self.line.0.get() else {
            return Ok(ToolOutput::error(
                "no orchestrator is attached to this worker; the challenge was not sent",
            ));
        };
        let control = ContractControl::WorkerChallenge {
            worker: ctx.worker.clone(),
            interface: input.interface,
            claim: input.claim,
            evidence: input.evidence,
        };
        // A recompute runs the contract's checks again, which can take minutes;
        // the worker's cancellation ends the wait.
        tokio::select! {
            reply = handle.contract(control) => Ok(match reply {
                Some(r) => render(r),
                None => ToolOutput::error("the run ended before the orchestrator answered"),
            }),
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled {
                tool: CHALLENGE_TOOL.into(),
            }),
        }
    }
}
