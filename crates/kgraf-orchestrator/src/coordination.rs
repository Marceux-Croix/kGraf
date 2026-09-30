//! The coordination state a run record carries beside its contracts: quality
//! gates (`gates.rs`), the worker-to-worker mailbox and read subscriptions
//! (`envelope.rs`), each open contract's wait state (`waits.rs`), and what
//! continuous merge-back put on the run's integration branch.
//!
//! One field on [`crate::RunStateRecord`], defaulted, so a record written before
//! it existed still loads, and so the parallel streams editing the record touch one
//! line of it.

use crate::envelope::Mailbox;
use crate::gates::GateNode;
use crate::waits::WaitState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What merging a verified attempt (or the bus's conversation files) into the
/// run's integration branch did (0018: continuous merge-back).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Integration {
    /// The workspaces are plain directories: there is no branch to merge into.
    Unversioned,
    /// The attempt changed nothing; the tip did not move.
    Empty,
    /// Merged; `commit` is the new integration tip.
    Merged { commit: String },
    /// The attempt conflicts with the tip. A resolver worker gets first crack
    /// at it (decision 0073, superseding this variant's
    /// original "never auto-resolved" rule from 0018); only when the resolver
    /// also fails is the contract dispatched again from the new tip.
    Conflict {
        tip: String,
        paths: Vec<String>,
        detail: String,
        /// The attempt's own commit, kept reachable (never garbage collected)
        /// until a resolver supersedes it or the contract gives up and falls
        /// back to a plain re-dispatch (0073). `None` for a workspace
        /// provider that keeps nothing across a conflict, which simply gets
        /// no resolver.
        #[serde(default)]
        commit: Option<String>,
    },
    /// Merge-back refused the attempt (protected paths, oversized files, ...).
    Refused { detail: String },
}

impl Integration {
    /// Whether the attempt's work is on the integration branch (or there is none).
    pub fn accepted(&self) -> bool {
        matches!(
            self,
            Integration::Unversioned | Integration::Empty | Integration::Merged { .. }
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Integration::Unversioned => "unversioned",
            Integration::Empty => "empty",
            Integration::Merged { .. } => "merged",
            Integration::Conflict { .. } => "conflict",
            Integration::Refused { .. } => "refused",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Coordination {
    /// Quality gates, one per major node (0018, 0019).
    pub gates: Vec<GateNode>,
    /// Why the run has no gates, when it has none: said, never skipped.
    pub gate_notice: Option<String>,
    /// Every worker-to-worker message of the run.
    pub mailbox: Mailbox,
    /// Per contract, the paths its workers were observed to read (0033).
    pub subscriptions: BTreeMap<String, BTreeSet<String>>,
    /// Per open contract (and running gate), why it is where it is.
    pub waits: BTreeMap<String, WaitState>,
    /// Messages the fan-out bound refused (counted).
    pub refused_messages: u64,
    /// Per verified contract, how its attempt reached the integration branch.
    /// A gate pins a commit only when every contract it covers is here.
    pub integration: BTreeMap<String, Integration>,
    /// The integration tip after the last merge this run made.
    pub integration_tip: Option<String>,
    /// The integration tip when the run planned its gates, before any of the
    /// plan landed: the commit a failing gate below the root is compared
    /// with ([`crate::gates::record_inconclusive`]). `None` for unversioned
    /// workspaces and for a record written before it was kept; such a run's
    /// gates are judged as they stand.
    pub base_commit: Option<String>,
    /// The project's validation commands run once at `base_commit`, the
    /// first time a gate below the root failed.
    pub base_check: Option<BaseCheck>,
    /// Merge-back conflicts and refusals, each re-dispatched.
    pub integration_rejections: u64,
    /// Conversation-file commits that did not reach the integration branch.
    pub comms_commit_failures: u64,
    /// The user's order (0014): contracts dispatched first, in this order, as
    /// they become eligible. Empty: the orchestrator sequences everything.
    pub user_order: Vec<String>,
    /// Conflicts speculative merge found between in-flight contracts before
    /// either landed (0074), each once per pair and kind. Kept after the
    /// contracts close (0028: nothing is pruned).
    pub speculative_conflicts: Vec<crate::speculation::SpeculativeConflict>,
    /// What speculative merge has cost this run.
    pub speculation: crate::speculation::SpeculationCost,
    /// Contracts that left `open_contracts`, verified or superseded by a split,
    /// with their brief kept: search reaches them (0032: "contract: ... including
    /// superseded ones"), and the text surface shows their brief.
    pub closed: Vec<ClosedContract>,
}

/// The validation commands' outcome at the run's base commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseCheck {
    pub commit: String,
    pub passed: bool,
    /// What ran, as the gate runner says.
    pub by: String,
}

/// How a contract left the open set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Closure {
    Verified,
    /// Its handoff was re-split into children, which replaced it.
    Superseded,
}

/// A verified contract's own account of its reasons, labeled with the model
/// and worker that wrote it (decision
/// 0026: derived text is labeled as another model's, never presented as the
/// reader's own reasoning). Carried on `ClosedContract` so a dependent
/// contract, dispatched after this one closes, can quote it -- the
/// conclusions cross vendors; the reasoning block that produced them never
/// does (0005).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerRationale {
    /// The route (vendor:model) the winning attempt ran on.
    pub model: String,
    /// The worker id of the winning attempt.
    pub worker: String,
    pub decisions: String,
    pub alternatives_rejected: String,
    pub counterpart_assumptions: String,
}

/// A contract no longer open, with what search and the text surface need.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedContract {
    pub contract: String,
    pub closure: Closure,
    /// The brief's instruction, verbatim.
    pub instruction: String,
    pub acceptance: String,
    #[serde(default)]
    pub workers: Vec<String>,
    #[serde(default)]
    pub worker_routes: Vec<String>,
    /// The winning attempt's own account of its reasons, when it gave one
    /// `None` for a closure that never
    /// carried a `WorkerResult` (`Superseded`), for a worker that omitted the
    /// block, and for every record from before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<ProducerRationale>,
}

impl ClosedContract {
    pub fn of(c: &crate::run_state::OpenContract, closure: Closure) -> Self {
        Self {
            contract: c.contract.clone(),
            closure,
            instruction: c.instruction.clone(),
            acceptance: c.acceptance.clone(),
            workers: c.workers.iter().map(ToString::to_string).collect(),
            worker_routes: c.worker_routes.clone(),
            rationale: None,
        }
    }

    /// Attaches the producer's rationale, if there is one. A builder rather
    /// than a constructor argument so the two `Superseded` call sites (which
    /// have no `WorkerResult` to draw one from) are unaffected.
    pub fn with_rationale(mut self, rationale: Option<ProducerRationale>) -> Self {
        self.rationale = rationale;
        self
    }
}
