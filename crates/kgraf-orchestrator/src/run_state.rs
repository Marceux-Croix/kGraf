//! The run-state record (decision 0056).
//!
//! On restart the orchestrator reads this record before it re-plans: open
//! contracts, admitted leases, the last verified commit, and the next planned
//! dispatch. It lives in the graph store (see `store.rs`); this type is its
//! shape and its validation.
//!
//! Two fields come from decision 0057. The **original instruction** is kept
//! verbatim, because every resume re-grounds on it, never on a summary. The last
//! commit is recorded as **verified**, naming the verifier, because progress claims
//! are checked against the verifier and not self-reported.
//!
//! Schema 2 (this build) adds what a resume needs so that no paid call repeats: each
//! open contract carries its whole brief, its lineage and the workers dispatched on
//! it (whose transcripts the resume continues), and the record carries the verified
//! and split contracts, the persisted steering notes, the re-grounding verdicts and
//! the serialization counters the acceptance report shows. Schema 1 was never written
//! outside tests, so it is not read.

use crate::plan::ContractSpec;
use crate::port::InboundKind;
use kgraf_core::{AgentId, NodeId, RunId, TaskId};
use kgraf_tools::UntrustedText;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The record format version this build reads and writes.
pub const RUN_STATE_SCHEMA_VERSION: u32 = 2;

/// One declared write: a file, or a sub-file node in it (0050).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WriteTarget {
    /// Relative to the repository root.
    pub path: String,
    /// The hierarchical node id, for a sub-file declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
}

/// Where an open contract stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContractStatus {
    /// Planned, not dispatched.
    #[default]
    Planned,
    /// Workers are running on it (or were, when the process died).
    Dispatched,
    /// Not dispatched until the user acts: a failed re-grounding, exhausted
    /// retries, or a readiness failure that waiting cannot fix.
    Held { reason: String },
}

/// A contract a worker has been, or will be, given and that is not yet verified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenContract {
    /// The contract's id.
    pub contract: String,
    pub task: TaskId,
    /// The workers dispatched on it, one per parallel attempt, in attempt order.
    #[serde(default)]
    pub workers: Vec<AgentId>,
    /// The route of each worker in `workers`: a resumed worker must run on the
    /// route whose prefix its transcript was recorded with.
    #[serde(default)]
    pub worker_routes: Vec<String>,
    /// The route whose model stated this contract's handshake (0030: each
    /// party states its own assumptions). The contract's first attempt runs on
    /// it, so the model that stated the assumptions is the one that works to
    /// them; a pin or a recorded worker route comes first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_route: Option<String>,
    /// What must hold for the contract to be met.
    pub acceptance: String,
    /// The brief's instruction, verbatim.
    pub instruction: String,
    /// The declared write set.
    #[serde(default)]
    pub writes: Vec<WriteTarget>,
    /// Contracts that must be verified first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Graph nodes for the worker's private slices, as last resolved. Resolved
    /// again from `slice_names` at each dispatch: a node id hashes the
    /// symbol's signature, so a predecessor's merge that renames a parameter
    /// gives the symbol a new id.
    #[serde(default)]
    pub slices: Vec<NodeId>,
    /// The slice names the plan gave, resolved at dispatch against the graph
    /// as it is then. A name that no longer resolves is an empty lookup,
    /// traced, and gives no slice. Empty: `slices` is used as recorded, less
    /// any node the graph no longer holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slice_names: Vec<String>,
    /// The file a crate-relative slice name (`crate::X`) resolves from: the
    /// contract's first declared write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice_from: Option<String>,
    /// `slices` are the top-level symbols of the declared write files, given
    /// because no named slice resolved (or none was named), not slices the
    /// plan named. Counted apart in the run report.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub slices_from_writes: bool,
    /// Tools the brief needs; readiness checks the route discovered them.
    #[serde(default)]
    pub tools: Vec<String>,
    /// A pinned route, or the one assigned at dispatch (`route_pinned` says
    /// which): recorded here either way so a handoff child (`resplit`) can
    /// read its parent's route after the parent's own `worker_routes` are
    /// gone.
    #[serde(default)]
    pub route: Option<String>,
    /// Whether `route` is a genuine pin (an explicit plan route, or a
    /// handshake's negotiated route folded in here) rather than the value
    /// `try_dispatch` recorded after its own fresh pick. Only a genuine pin
    /// blocks `route_base`'s auto-pick and the retry-diversity offset
    /// (decision 0085): without this flag, the first dispatch's own
    /// bookkeeping write to `route` (`if c.route.is_none() { c.route =
    /// Some(routes[0]) }`) looked exactly like a pin from then on, so an
    /// unpinned contract's retry after a failed flight silently kept
    /// landing on the same route that had just failed it, no matter the
    /// routing policy (found live in the 2026-09-28 distributed-efficacy
    /// benchmark's `gate_remediation_exhausted` runs).
    ///
    /// A record written before this field existed reads `false`. On resume, a
    /// planned contract with a `route` that never dispatched (and is not a
    /// resolver) is taken as pinned, since only the plan could have set it;
    /// one that dispatched before keeps reading as an assignment.
    #[serde(default)]
    pub route_pinned: bool,
    /// Parallel attempts for the verifier to select among (0009).
    #[serde(default = "one")]
    pub attempts: u32,
    /// Contract ids from the root to this one, inclusive (delegation trace).
    #[serde(default)]
    pub lineage: Vec<String>,
    /// The instruction of each contract in `lineage`, for the cycle guard.
    #[serde(default)]
    pub lineage_tasks: Vec<String>,
    /// The worktree inherited from a handed-back parent, if any.
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub status: ContractStatus,
    /// Attempts that ended without a verified result.
    #[serde(default)]
    pub failures: u32,
    /// The commit a fresh attempt starts from, when it must not be the current
    /// integration tip: a remediation starts from the commit its gate failed on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Each dispatched worker's worktree as the workspace provider recorded it
    /// ([`crate::Workspaces::handle`]; for git worktrees, the gitlink and base),
    /// by worker id. A resumed worker's worktree is reopened from this, never
    /// re-read from the worker-writable tree.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub worktrees: BTreeMap<String, String>,
    /// Verifications started on this contract, all attempts together. Each
    /// verification is numbered from this persisted counter, so verifying a
    /// worktree again (after a resume, or after a voided pass) never reuses the
    /// number, and so the attempt ref, of an earlier one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub verifications: u32,
    /// Consecutive dispatches whose worker environment or worktree could not
    /// be provided.
    /// Not model attempts: they never count toward `failures`. Retried with
    /// backoff; past the bound the contract is held, saying why.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub environment_failures: u32,
    /// Consecutive rounds whose verifier could not run. The verifier's fault,
    /// not the model's: never counted toward `failures`. Retried with backoff;
    /// past the bound the contract is held, saying why.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub verifier_failures: u32,
    /// Workers replaced because their worktree could not be provided (a
    /// refused reopen): part of the next worker's id, so it gets a fresh
    /// worktree. Not a model failure.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub renewals: u32,
    /// No dispatch before this time (the environment backoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<i64>,
    /// Redirect notes sent to a running worker on this contract, in order
    /// (decision 0067, point 3). A retry or a handoff child's brief carries
    /// the original instruction plus these, so a redirect steers every future
    /// attempt, not only the one that was live when it landed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<String>,
    /// Sessions of this contract the user stopped directly (decision 0067,
    /// point 2). While one is listed, the contract stays held for the user:
    /// a process resume does not release it and the run does not end on it;
    /// only `resume_session` removes an entry. A typed fact, never inferred
    /// from the hold's reason text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped_by_user: Vec<AgentId>,
    /// This contract IS a merge-conflict resolver (decision
    /// 0073): `Some(original)` names the contract it resolves. A resolver is
    /// ordinary in every other respect (dispatched, verified, merged like any
    /// contract); this is what tells `finalize` to close `original` on
    /// success instead of itself, and never to retry a resolver in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_for: Option<String>,
    /// The conflicted attempt's original workspace, kept by the provider
    /// (0073) and discarded once this resolver settles, whichever way.
    /// `resolver_for`'s counterpart: set together, cleared together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_source: Option<PathBuf>,
    /// What kind of work this contract mainly asks for (decision 0075),
    /// assigned by the orchestrator at decomposition (`plan.rs`). `None` is
    /// routed as [`kgraf_core::model_profile::TaskKind::Implementation`], with
    /// that default traced rather than silent; Rotation policy ignores this
    /// field entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_kind: Option<kgraf_core::model_profile::TaskKind>,
    /// Why the verifier refused the last failed round of attempts (decision
    /// 0084): a scope violation, the acceptance command's failing output. The
    /// next fresh attempt's brief carries it as data, so a retry learns what
    /// the refused one did wrong. Scrubbed for storage and capped
    /// ([`MAX_REFUSAL_CHARS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refusal: Option<String>,
    /// Commands an attempt must pass besides the run's own check: a gate
    /// remediation carries the commands of the gate it remediates, so a fix
    /// that the run's check already passed without is not accepted until the
    /// failing gate's own commands pass in its worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_commands: Vec<String>,
    /// Shell programs granted to this contract after a worker's permission
    /// shortfall, each under the user's permission mode (decisions 0093 and
    /// 0094). Every
    /// fresh attempt's shell allows them, and its brief names them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub granted_programs: Vec<String>,
    /// Who asked for each grant, and why (decision 0093).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<crate::shortfall::GrantRecord>,
    /// Amendments from `instructions` shortfalls (decision 0027's inadequacy,
    /// recorded, never a silent edit): each is quoted as data in the next
    /// attempt's brief. Scrubbed and capped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortfall_notes: Vec<String>,
    /// Re-dispatches from shortfalls so far, bounded by
    /// `MAX_SHORTFALL_REDISPATCHES`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shortfall_redispatches: u32,
    /// Workers whose attempts ended in a shortfall report: when one finishes,
    /// its attempt is neither verified nor counted a failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortfall_workers: Vec<AgentId>,
    /// One entry per shortfall answered to its worker without a decision
    /// (nothing refused it, ambiguous, a path in scope or owned elsewhere),
    /// bounded per worker by `MAX_ANSWERED_SHORTFALLS`; a worker's entries
    /// go when its attempt finishes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortfall_answers: Vec<AgentId>,
    /// Workers stopped for reporting past that bound: when one finishes, its
    /// attempt is stuck, counted a failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortfall_stuck: Vec<AgentId>,
    /// Set when a shortfall is held for the user: the held reason, applied
    /// once the reporting attempt has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortfall_hold: Option<String>,
    /// A permission request waiting for the user (decision 0094): the
    /// contract is held until the user grants or denies it, or until a mode
    /// change grants or refuses it. Persisted, so a resumed run asks again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission: Option<PendingPermission>,
}

/// One permission request waiting for the user (decision 0094).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPermission {
    /// The desk's id for it, unique within the run.
    pub id: u64,
    pub scope: crate::shortfall::RequestScope,
    /// The program or path asked for.
    pub subject: String,
    /// The worker whose report asked, and its words (scrubbed, capped).
    pub worker: String,
    pub need: String,
    pub why: String,
    pub question: String,
    pub raised_at_ms: i64,
}

/// The user's yes to a run's plan (decision 0094's plan review).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanApproval {
    pub at_ms: i64,
    /// `user`, or `--approve-plan` for a run approved on the command line.
    pub by: String,
    /// How many plans were shown before this one was approved.
    pub revision: u32,
}

/// The longest [`OpenContract::last_refusal`] kept, in characters, from the
/// end (a check's output says what failed last).
pub const MAX_REFUSAL_CHARS: usize = 4_000;

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn one() -> u32 {
    1
}

impl OpenContract {
    /// A planned contract with one attempt and itself as its lineage.
    ///
    /// **`instruction` and `acceptance` must already be through
    /// `kgraf_secrets::intake`** (decision 0071), or be kGraf's own words: they
    /// are stored as given. Taking them in here instead would take in twice
    /// text a caller already took in, which drops its model copy. Debug builds
    /// assert that both scrub to themselves, which only taken-in text does.
    pub fn planned(
        contract: impl Into<String>,
        task: TaskId,
        instruction: impl Into<String>,
        acceptance: impl Into<String>,
    ) -> Self {
        let contract = contract.into();
        // Already the stored copy: every caller passes
        // text taken in through `kgraf_secrets::intake` (`sanitize_plan`, a
        // handoff item, a gate remediation) or kGraf's own words. Not taken in
        // again here: a second intake would drop the text's model copy,
        // which `brief_text` reads.
        let instruction = instruction.into();
        let acceptance = acceptance.into();
        debug_assert!(
            kgraf_secrets::scrub_persist(&instruction) == instruction
                && kgraf_secrets::scrub_persist(&acceptance) == acceptance,
            "OpenContract::planned was given text that did not go through intake"
        );
        Self {
            lineage: vec![contract.clone()],
            lineage_tasks: vec![instruction.clone()],
            contract,
            task,
            workers: Vec::new(),
            worker_routes: Vec::new(),
            handshake_route: None,
            acceptance,
            instruction,
            writes: Vec::new(),
            depends_on: Vec::new(),
            slices: Vec::new(),
            slice_names: Vec::new(),
            slice_from: None,
            slices_from_writes: false,
            tools: Vec::new(),
            route: None,
            route_pinned: false,
            attempts: 1,
            workspace: None,
            status: ContractStatus::Planned,
            failures: 0,
            base: None,
            worktrees: BTreeMap::new(),
            verifications: 0,
            environment_failures: 0,
            verifier_failures: 0,
            renewals: 0,
            retry_at_ms: None,
            redirects: Vec::new(),
            stopped_by_user: Vec::new(),
            resolver_for: None,
            resolver_source: None,
            task_kind: None,
            last_refusal: None,
            gate_commands: Vec::new(),
            granted_programs: Vec::new(),
            grants: Vec::new(),
            shortfall_notes: Vec::new(),
            shortfall_redispatches: 0,
            shortfall_workers: Vec::new(),
            shortfall_answers: Vec::new(),
            shortfall_stuck: Vec::new(),
            shortfall_hold: None,
            pending_permission: None,
        }
    }

    /// The id leases are held under: the lineage root, so a re-split remainder
    /// keeps the parent's leases in the parent's worktree.
    pub fn lease_holder(&self) -> &str {
        self.lineage.first().unwrap_or(&self.contract)
    }
}

/// A write lease the registry admitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmittedLease {
    /// The leased path, relative to the repository root.
    pub path: String,
    /// The node, for a sub-file lease keyed on a hierarchical node id (0050).
    pub node: Option<NodeId>,
    pub agent: AgentId,
    /// The lease generation, so a stale lease is recognizable on resume.
    pub generation: i64,
}

/// A commit the verifier accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedCommit {
    /// Full hex object id: 40 characters (SHA-1) or 64 (SHA-256).
    pub sha: String,
    /// What verified it, such as the test command or verifier id.
    pub verified_by: String,
    /// Milliseconds since the epoch.
    pub verified_ms: i64,
}

/// The dispatch the orchestrator plans next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDispatch {
    pub task: TaskId,
    /// The brief it will send, in summary.
    pub brief: String,
    /// Tasks that must finish first.
    pub depends_on: Vec<TaskId>,
}

/// A note or steer the user sent, persisted so a replay sees it in order. The
/// text stays untrusted: it is data for the next orchestrator call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedNote {
    pub id: String,
    pub channel: String,
    pub received_ms: i64,
    pub kind: InboundKind,
    pub text: UntrustedText,
    /// Whether an orchestrator call has been given it.
    #[serde(default)]
    pub delivered: bool,
}

/// Why a re-grounding ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundingTrigger {
    Resume,
    Cadence,
    /// A steer or note was waiting for the next orchestrator call.
    Steer,
}

/// One re-grounding verdict (0057): whether the next dispatch still serves the
/// verbatim original instruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundingRecord {
    pub at_ms: i64,
    pub trigger: GroundingTrigger,
    /// The contract the verdict is about.
    pub contract: String,
    pub serves: bool,
    pub reason: String,
    /// The ledger call that produced it, when a model was asked.
    pub call: Option<String>,
}

/// Counts the acceptance report shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunCounters {
    /// Contracts dispatched (one per dispatch, however many attempts).
    pub dispatches: u64,
    /// Contracts that waited at least once on a lease another contract held:
    /// each is a serialization (0015, 0048).
    pub serialized: u64,
    /// Refusals by the work-item cap or the live-agent ceiling.
    pub capacity_refusals: u64,
    /// Deferrals by the per-cluster in-flight cap (decision 0082).
    pub cluster_cap_refusals: u64,
    /// Readiness failures.
    pub readiness_failures: u64,
    /// Workers that reached the stop point and handed back or compacted (0057
    /// and 0095 sizing misses), or ran past their round budget on measured
    /// progress (0096), one per worker.
    pub sizing_misses: u64,
    /// Workers a round checkpoint let run past their round budget on measured
    /// progress (decision 0096).
    pub round_extensions: u64,
    /// Workers a round checkpoint stopped: no progress at two checkpoints in a
    /// row after a productive one, or the checkpoint ceiling (decision 0096).
    pub checkpoint_stops: u64,
    /// Reservations made (worker attempts and orchestrator calls); accounting
    /// only, decision 0080 removed every spend cap, so none is ever refused.
    pub budget_admitted: u64,
    /// Leases lost to expiry or takeover; each cancels the workers holding it.
    pub leases_lost: u64,
    /// Settlements above their hold: the estimate undercounted the call, an
    /// accounting fact with no effect on dispatch (decision 0080).
    pub overruns: u64,
    /// Attempts that never ran for want of infrastructure: a worker
    /// environment that could not be built (an MCP server down) or a worktree
    /// that could not be provided (a refused reopen, an unreadable tree, git
    /// failing). Not model attempts: never counted toward `max_retries`.
    pub environment_failures: u64,
    /// Dispatches whose workers were given at least one graph slice.
    pub sliced_dispatches: u64,
    /// Of those, dispatches given the declared files' symbols because no
    /// named slice resolved or none was named.
    pub slices_from_writes: u64,
    /// Slice names the plan gave that found nothing in the graph, each traced
    /// as an `empty_graph_lookup`.
    pub empty_slice_lookups: u64,
}

/// How the run stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Running,
    /// Every contract verified.
    Completed,
    /// Nothing can progress without the user.
    Blocked { reason: String },
    /// The user stopped it; resumable.
    Stopped,
}

/// One run's resumable state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStateRecord {
    pub schema_version: u32,
    pub run: RunId,
    /// The user's instruction, verbatim (0057).
    pub original_instruction: String,
    pub open_contracts: Vec<OpenContract>,
    pub admitted_leases: Vec<AdmittedLease>,
    pub last_verified_commit: Option<VerifiedCommit>,
    pub next_dispatch: Option<PlannedDispatch>,
    /// Milliseconds since the epoch.
    pub updated_ms: i64,
    /// Contracts the verifier accepted, in order.
    #[serde(default)]
    pub verified_contracts: Vec<String>,
    /// Contracts whose handoff was re-split into children.
    #[serde(default)]
    pub split_contracts: Vec<String>,
    #[serde(default)]
    pub notes: Vec<RecordedNote>,
    #[serde(default)]
    pub groundings: Vec<GroundingRecord>,
    #[serde(default)]
    pub counters: RunCounters,
    #[serde(default)]
    pub status: RunStatus,
    /// Whether decomposition finished; a record without it re-plans on resume.
    #[serde(default)]
    pub planned: bool,
    /// Decomposition in progress, saved after every model call, so a kill
    /// mid-decomposition does not repeat a call that already answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_progress: Option<PlanProgress>,
    /// Gates, worker-to-worker messages, subscriptions and wait states.
    #[serde(default)]
    pub coordination: crate::coordination::Coordination,
    /// Whether this run checks interface contracts, and the bounds of one
    /// check run. Written when the run starts; a resume with another setting is
    /// refused, naming both: a run half checked is neither. A
    /// record from before the field reads as off.
    #[serde(default)]
    pub contract_checks: ContractCheckSettings,
    /// Worker agent id -> challenges it sent through `challenge_contract` that
    /// the orchestrator took (counted when taken, whatever the outcome), for
    /// the per-worker budget (`DEFAULT_MAX_CHALLENGES_PER_WORKER`).
    #[serde(default)]
    pub worker_challenges: BTreeMap<String, u32>,
    /// The approved plan this run was started from, when it was: its
    /// contracts are that plan's items, `<plan>/item/<contract>` in the graph, so
    /// the galaxy can draw a contract on the presumed node its item adopted
    /// (decision 0064). A record from before the field reads as none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_plan: Option<String>,
    /// The user's yes to this run's plan, when plan review was on (decision
    /// 0094). With plan review on and this absent, no worker starts; a
    /// resumed run shows the plan again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_approved: Option<PlanApproval>,
    /// The run that started this one (decision 0067), reserved for runs
    /// started by other runs: `None` for a top-level run and for every record
    /// from before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run: Option<RunId>,
    /// Consumer-driven contract test attempts (0076): each consumer's own
    /// test of what it expects from a producer, and its outcome. Persisted
    /// directly here (not only in the Desk's in-memory copy) so it survives
    /// resume; a record from before this field existed reads as empty.
    #[serde(default)]
    pub consumer_tests: crate::consumer_tests::ConsumerTestBook,
    /// Verified contracts whose acceptance says nothing about the work
    /// (decision 0093): the check always passes, or the accepted worker
    /// finished after a policy refused a call it never ran. Contract id to
    /// reason. The run reports them as unverified, never as plainly done.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unverified: BTreeMap<String, String>,
    /// Times the whole plan was sent back to the planner because the
    /// pre-dispatch checks of its interfaces held it before any work started
    /// (at most once).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replans: u32,
    /// The check failures a re-plan sends back to the planner, kept until the
    /// re-plan is taken so a resume in between still sends them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replan_note: Option<String>,
    /// Workers the runtime compacted (decision 0095), in order: each is a
    /// contract that was larger than planned when the trigger was the stop
    /// point. The notice names the worker's transcript, which holds the full
    /// pre-compaction history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compactions: Vec<CompactionRecord>,
    /// Holds placed by pre-dispatch interface checks, in order, each with
    /// the route and model of every party (decision 0078). A record from
    /// before the field reads as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_holds: Vec<CheckHold>,
}

/// A hold a pre-dispatch interface check placed, with the route and model of
/// every party of the interface (decision 0078), so a hold is attributable
/// to a model pair from the run record alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckHold {
    pub interface: String,
    pub version: u32,
    /// The failed check's stage (`artifact`, `stub_compile`, `vacuity`,
    /// `handshake`, `stub_land`), or `not_run` when the checks could not run.
    pub stage: String,
    /// The contracts the check held.
    pub held: Vec<String>,
    /// Every party of the interface, held or not.
    pub parties: Vec<crate::checks::PartyModel>,
}

/// One worker compaction, as the run record keeps it (decision 0095).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub contract: String,
    pub worker: String,
    /// Whether it counts as a sizing miss: the stop point triggered it, not a
    /// user's request.
    pub sizing_miss: bool,
    /// The runtime's notice, as the worker emitted it.
    pub notice: kgraf_runtime::CompactionNotice,
}

/// A run's contract-check setting, as its record keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractCheckSettings {
    pub enabled: bool,
    /// Bound of one test run, in seconds; 0 when off.
    #[serde(default)]
    pub test_timeout_s: u64,
    /// Bound of one compile, in seconds; 0 when off.
    #[serde(default)]
    pub compile_timeout_s: u64,
}

impl std::fmt::Display for ContractCheckSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.enabled {
            write!(
                f,
                "contract checks on (test bound {}s, compile bound {}s)",
                self.test_timeout_s, self.compile_timeout_s
            )
        } else {
            write!(f, "contract checks off")
        }
    }
}

/// One spec waiting to be sized or split.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub spec: ContractSpec,
    /// The final (sanitized, prefixed) contract id.
    pub id: String,
    /// How many splits deep it is.
    pub depth: u32,
}

/// Decomposition state between model calls.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgress {
    /// Specs still to size; the last one is next.
    pub stack: Vec<PlanItem>,
    /// Specs sized and accepted as contracts, in order.
    pub accepted: Vec<(ContractSpec, OpenContract)>,
    /// A split contract's id to the ids that replaced it.
    pub leaves: std::collections::BTreeMap<String, Vec<String>>,
    /// Every id taken so far, so a child id cannot collide with another.
    pub ids: BTreeSet<String>,
    /// Splits made so far, against the per-plan cap.
    pub splits: u32,
    /// Interface contracts the decomposition wrote so far, party ids as the
    /// plan names them; registered with the contract book when planning ends.
    /// Kept here so a plan resumed between model calls keeps them.
    #[serde(default)]
    pub interfaces: Vec<crate::checks::InterfaceContract>,
}

impl PlanProgress {
    /// The state before any spec is sized: every spec on the stack (the first
    /// one on top) at depth 0, and its id taken.
    pub fn seeded(specs: Vec<ContractSpec>) -> Self {
        Self {
            ids: specs.iter().map(|s| s.id.clone()).collect(),
            stack: specs
                .into_iter()
                .rev()
                .map(|spec| PlanItem {
                    id: spec.id.clone(),
                    spec,
                    depth: 0,
                })
                .collect(),
            ..Self::default()
        }
    }
}

/// A record that cannot be resumed from.
#[derive(Debug, thiserror::Error)]
pub enum RunStateError {
    #[error("cannot parse run-state record")]
    Parse(#[from] serde_json::Error),
    #[error("run-state schema version {found} is not {expected}")]
    SchemaVersion { found: u32, expected: u32 },
    #[error("the original instruction is empty")]
    EmptyInstruction,
    #[error("contract `{contract}` appears twice")]
    DuplicateContract { contract: String },
    #[error("lease on `{path}` appears twice")]
    DuplicateLease { path: String },
    #[error("`{sha}` is not a full hex commit id")]
    BadCommit { sha: String },
    #[error("the next dispatch of task `{task}` depends on itself")]
    SelfDependency { task: String },
    #[error("contract `{contract}` depends on `{missing}`, which is neither open nor verified")]
    UnknownDependency { contract: String, missing: String },
    #[error("contract `{contract}` has {attempts} attempts; at least one is needed")]
    NoAttempts { contract: String, attempts: u32 },
}

impl RunStateRecord {
    /// Drop the in-memory model copy of every stored text in this record
    /// (decision 0071), for a run that has ended for good.
    pub fn forget_model_copies(&self) {
        let forget = kgraf_secrets::forget_model_copy;
        forget(&self.original_instruction);
        for c in &self.open_contracts {
            forget(&c.instruction);
            forget(&c.acceptance);
            c.lineage_tasks.iter().for_each(|t| forget(t));
        }
        for c in &self.coordination.closed {
            forget(&c.instruction);
            forget(&c.acceptance);
        }
        for n in &self.notes {
            n.text.forget_model_copy();
        }
        for m in &self.coordination.mailbox.messages {
            m.body.forget_model_copy();
        }
        if let Some(p) = &self.plan_progress {
            for item in &p.stack {
                forget(&item.spec.instruction);
                forget(&item.spec.acceptance);
            }
            for (spec, c) in &p.accepted {
                forget(&spec.instruction);
                forget(&spec.acceptance);
                forget(&c.instruction);
            }
        }
    }

    /// A fresh record for a run.
    pub fn new(run: RunId, original_instruction: impl Into<String>, now_ms: i64) -> Self {
        Self {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            run,
            // Scrubbed at intake; a model reads
            // `kgraf_secrets::model_copy` of it.
            original_instruction: kgraf_secrets::intake(&original_instruction.into()),
            open_contracts: Vec::new(),
            admitted_leases: Vec::new(),
            last_verified_commit: None,
            next_dispatch: None,
            updated_ms: now_ms,
            verified_contracts: Vec::new(),
            split_contracts: Vec::new(),
            notes: Vec::new(),
            groundings: Vec::new(),
            counters: RunCounters::default(),
            status: RunStatus::Running,
            planned: false,
            plan_progress: None,
            coordination: crate::coordination::Coordination::default(),
            contract_checks: ContractCheckSettings::default(),
            worker_challenges: BTreeMap::new(),
            source_plan: None,
            plan_approved: None,
            parent_run: None,
            consumer_tests: crate::consumer_tests::ConsumerTestBook::default(),
            unverified: BTreeMap::new(),
            replans: 0,
            replan_note: None,
            compactions: Vec::new(),
            check_holds: Vec::new(),
        }
    }

    /// Check the record before resuming from it.
    pub fn validate(&self) -> Result<(), RunStateError> {
        if self.schema_version != RUN_STATE_SCHEMA_VERSION {
            return Err(RunStateError::SchemaVersion {
                found: self.schema_version,
                expected: RUN_STATE_SCHEMA_VERSION,
            });
        }
        if self.original_instruction.trim().is_empty() {
            return Err(RunStateError::EmptyInstruction);
        }
        let mut contracts = BTreeSet::new();
        for c in &self.open_contracts {
            if !contracts.insert(c.contract.as_str()) {
                return Err(RunStateError::DuplicateContract {
                    contract: c.contract.clone(),
                });
            }
            if c.attempts == 0 {
                return Err(RunStateError::NoAttempts {
                    contract: c.contract.clone(),
                    attempts: 0,
                });
            }
        }
        for v in &self.verified_contracts {
            if !contracts.insert(v.as_str()) {
                return Err(RunStateError::DuplicateContract {
                    contract: v.clone(),
                });
            }
        }
        for c in &self.open_contracts {
            for d in &c.depends_on {
                if !contracts.contains(d.as_str()) {
                    return Err(RunStateError::UnknownDependency {
                        contract: c.contract.clone(),
                        missing: d.clone(),
                    });
                }
            }
        }
        let mut leases = BTreeSet::new();
        for l in &self.admitted_leases {
            if !leases.insert((l.path.as_str(), l.node.as_ref().map(NodeId::as_str))) {
                return Err(RunStateError::DuplicateLease {
                    path: l.path.clone(),
                });
            }
        }
        if let Some(commit) = &self.last_verified_commit {
            let hex = commit.sha.chars().all(|c| c.is_ascii_hexdigit());
            if !hex || !(commit.sha.len() == 40 || commit.sha.len() == 64) {
                return Err(RunStateError::BadCommit {
                    sha: commit.sha.clone(),
                });
            }
        }
        if let Some(next) = &self.next_dispatch
            && next.depends_on.contains(&next.task)
        {
            return Err(RunStateError::SelfDependency {
                task: next.task.to_string(),
            });
        }
        Ok(())
    }

    /// Parse and validate a stored record.
    pub fn from_json(text: &str) -> Result<Self, RunStateError> {
        let record: Self = serde_json::from_str(text)?;
        record.validate()?;
        Ok(record)
    }

    /// Validate and serialize for storage. An invalid record is not written.
    pub fn to_json(&self) -> Result<String, RunStateError> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }

    /// An open contract by id.
    pub fn contract(&self, id: &str) -> Option<&OpenContract> {
        self.open_contracts.iter().find(|c| c.contract == id)
    }

    /// An open contract by id, mutably.
    pub fn contract_mut(&mut self, id: &str) -> Option<&mut OpenContract> {
        self.open_contracts.iter_mut().find(|c| c.contract == id)
    }

    /// Whether a contract has been verified.
    pub fn is_verified(&self, id: &str) -> bool {
        self.verified_contracts.iter().any(|v| v == id)
    }

    /// Where the run stands, from the stored record alone: for a reader that is
    /// not the run's owner. Like the live status reply it carries states and
    /// categories only, never verifier output or model text; a blocked run's
    /// reason is left out for that reason.
    pub fn status_line(&self) -> String {
        let state = match &self.status {
            RunStatus::Running => "running",
            RunStatus::Completed => "completed",
            RunStatus::Blocked { .. } => "blocked",
            RunStatus::Stopped => "stopped",
        };
        format!(
            "run {} ({state}): {} verified, {} open; {}",
            self.run,
            self.verified_contracts.len(),
            self.open_contracts.len(),
            crate::waits::one_line(&self.coordination.waits)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RunStateRecord {
        let mut r = RunStateRecord::new(RunId::new("run-1"), "port the parser to Rust", 1);
        let mut c = OpenContract::planned("c1", "t1".into(), "port it", "cargo test passes");
        c.workers.push("w1".into());
        r.open_contracts.push(c);
        r.admitted_leases.push(AdmittedLease {
            path: "src/parser.rs".into(),
            node: Some(NodeId::new("n-parse")),
            agent: "w1".into(),
            generation: 3,
        });
        r.last_verified_commit = Some(VerifiedCommit {
            sha: "a".repeat(40),
            verified_by: "cargo test --workspace".into(),
            verified_ms: 2,
        });
        r.next_dispatch = Some(PlannedDispatch {
            task: "t2".into(),
            brief: "wire the parser into the CLI".into(),
            depends_on: vec!["t1".into()],
        });
        r.notes.push(RecordedNote {
            id: "n1".into(),
            channel: "box".into(),
            received_ms: 3,
            kind: InboundKind::Note,
            text: UntrustedText::new("keep the public API"),
            delivered: false,
        });
        r
    }

    #[test]
    fn a_record_survives_storage_unchanged() {
        let r = record();
        let back = RunStateRecord::from_json(&r.to_json().expect("ser")).expect("de");
        assert_eq!(back, r);
    }

    #[test]
    fn the_instruction_is_kept_verbatim() {
        let text = "  Port the parser.\n\nKeep the public API.  ";
        let r = RunStateRecord::new(RunId::new("r"), text, 0);
        let back = RunStateRecord::from_json(&r.to_json().expect("ser")).expect("de");
        assert_eq!(back.original_instruction, text);
    }

    #[test]
    fn invalid_records_are_refused_on_read_and_on_write() {
        let mut r = record();
        r.schema_version = 1;
        assert!(matches!(
            r.to_json(),
            Err(RunStateError::SchemaVersion { .. })
        ));

        let mut r = record();
        r.original_instruction = " ".into();
        assert!(matches!(r.validate(), Err(RunStateError::EmptyInstruction)));

        let mut r = record();
        r.open_contracts.push(r.open_contracts[0].clone());
        assert!(matches!(
            r.validate(),
            Err(RunStateError::DuplicateContract { .. })
        ));

        let mut r = record();
        r.verified_contracts.push("c1".into());
        assert!(matches!(
            r.validate(),
            Err(RunStateError::DuplicateContract { .. })
        ));

        let mut r = record();
        r.open_contracts[0].depends_on.push("ghost".into());
        assert!(matches!(
            r.validate(),
            Err(RunStateError::UnknownDependency { .. })
        ));

        let mut r = record();
        r.open_contracts[0].attempts = 0;
        assert!(matches!(
            r.validate(),
            Err(RunStateError::NoAttempts { .. })
        ));

        let mut r = record();
        r.admitted_leases.push(r.admitted_leases[0].clone());
        assert!(matches!(
            r.validate(),
            Err(RunStateError::DuplicateLease { .. })
        ));

        for bad in ["abc", &"g".repeat(40), &"a".repeat(41)] {
            let mut r = record();
            if let Some(c) = r.last_verified_commit.as_mut() {
                c.sha = bad.to_owned();
            }
            assert!(
                matches!(r.validate(), Err(RunStateError::BadCommit { .. })),
                "{bad}"
            );
        }
        let mut r = record();
        if let Some(c) = r.last_verified_commit.as_mut() {
            c.sha = "b".repeat(64);
        }
        assert!(r.validate().is_ok(), "SHA-256 object ids are accepted");

        let mut r = record();
        if let Some(n) = r.next_dispatch.as_mut() {
            n.depends_on.push("t2".into());
        }
        assert!(matches!(
            r.validate(),
            Err(RunStateError::SelfDependency { .. })
        ));
    }

    /// Consumer test outcomes persist in the run record
    /// itself, not only in the Desk's in-memory copy, and round-trip through
    /// storage with every outcome kind.
    #[test]
    fn consumer_test_outcomes_round_trip_through_the_persisted_record() {
        use crate::consumer_tests::{ConsumerTestKey, ConsumerTestOutcome};

        let mut r = record();
        r.consumer_tests.record(
            ConsumerTestKey::new("config", 1, "cons", "prod"),
            ConsumerTestOutcome::Accepted {
                test: crate::checks::FileOverlay::new("consumer_test.py", "content"),
                calls: vec![],
            },
        );
        r.consumer_tests.record(
            ConsumerTestKey::new("config", 1, "cons2", "prod"),
            ConsumerTestOutcome::NotRun {
                reason: "the reservation was refused".into(),
            },
        );
        let back = RunStateRecord::from_json(&r.to_json().expect("ser")).expect("de");
        assert_eq!(back, r);
        assert_eq!(back.consumer_tests, r.consumer_tests);
    }

    /// A record saved before this field existed has no `consumer_tests` key
    /// at all; it must still load, with an empty book (`#[serde(default)]`).
    #[test]
    fn a_record_from_before_consumer_tests_existed_still_loads() {
        let r = record();
        let json = r.to_json().expect("ser");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let mut object = value.as_object().expect("object").clone();
        object.remove("consumer_tests");
        let without_field = serde_json::to_string(&object).expect("re-serialize");
        let back = RunStateRecord::from_json(&without_field).expect("de");
        assert_eq!(
            back.consumer_tests,
            crate::consumer_tests::ConsumerTestBook::default()
        );
    }

    #[test]
    fn two_sub_file_leases_on_one_path_are_distinct() {
        let mut r = record();
        r.admitted_leases.push(AdmittedLease {
            node: Some(NodeId::new("n-other")),
            ..r.admitted_leases[0].clone()
        });
        assert!(r.validate().is_ok());
    }

    #[test]
    fn an_unknown_key_in_a_stored_record_is_refused() {
        let mut value = serde_json::to_value(record()).expect("value");
        value["progress_percent"] = serde_json::json!(90);
        assert!(matches!(
            RunStateRecord::from_json(&value.to_string()),
            Err(RunStateError::Parse(_))
        ));
    }

    /// Decision 0067 adds `OpenContract.redirects` and
    /// `RunStateRecord.parent_run`. A record written before either existed
    /// carries neither key; it must still load, with both reading as empty.
    #[test]
    fn an_old_record_without_redirects_or_parent_run_deserializes() {
        let mut value = serde_json::to_value(record()).expect("value");
        assert!(
            value.get("parent_run").is_none(),
            "an absent parent_run is already omitted, not written as null"
        );
        for c in value["open_contracts"]
            .as_array_mut()
            .expect("open_contracts")
        {
            c.as_object_mut()
                .expect("contract object")
                .remove("redirects");
        }
        let back =
            RunStateRecord::from_json(&value.to_string()).expect("an old record must still load");
        assert!(back.open_contracts[0].redirects.is_empty());
        assert_eq!(back.parent_run, None);
    }

    #[test]
    fn a_re_split_child_holds_leases_under_its_lineage_root() {
        let mut child = OpenContract::planned("c1.1", "t".into(), "rest", "tests pass");
        child.lineage = vec!["c1".into(), "c1.1".into()];
        assert_eq!(child.lease_holder(), "c1");
    }

    /// A new record and a planned contract store the
    /// persist-scrubbed copy, and the model copy maps back from it.
    #[test]
    fn intake_scrubs_the_instruction_and_a_brief_and_keeps_the_model_copy() {
        let token = format!("ghp_{}", "Yk2Lm8Nq4Rs6Tv0Wx3Za5Bc7De9Fg1Hj2Kp4");
        let record = RunStateRecord::new(
            RunId::new("r1"),
            format!("run-state instruction with {token}"),
            1,
        );
        assert!(!record.original_instruction.contains(&token));
        assert!(kgraf_secrets::model_copy(&record.original_instruction).contains(&token));
        // Callers pass text already taken in; `planned` keeps it as is, so
        // its model copy survives (a second intake would drop it).
        let c = OpenContract::planned(
            "c1",
            "t1".into(),
            kgraf_secrets::intake(&format!("run-state brief uses {token}")),
            kgraf_secrets::intake(&format!("run-state acceptance: {token} rotated")),
        );
        for stored in [&c.instruction, &c.acceptance, &c.lineage_tasks[0]] {
            assert!(!stored.contains(&token), "{stored}");
            assert!(kgraf_secrets::model_copy(stored).contains(&token));
        }
    }

    /// `planned` refuses raw text in a debug build.
    #[test]
    #[should_panic(expected = "did not go through intake")]
    #[cfg(debug_assertions)]
    fn planned_asserts_its_text_went_through_intake() {
        let token = format!("ghp_{}", "Pl1An2Ne3Dr4Aw5Ab6Cd7Ef8Gh9Ij0Kl1Mn2");
        let _ = OpenContract::planned("c1", "t1".into(), format!("use {token}"), "ok");
    }
}
