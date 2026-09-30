//! Subagents (decision 0067, point 5): a worker's `spawn_subagent` call, the
//! child's life on the loop, and its report back to the parent.
//!
//! **States of one child** (the loop owns them; nothing here is shared):
//!
//! ```text
//!   spawn call --refused--> tool result (no child exists)
//!       | admitted, fenced, worktree, record saved, row written
//!       v
//!   Running --Done, parent cancelled (stop)--> Resumable (row open, record open)
//!       |                                          | parent dispatched again
//!       |                                          v
//!       |                                      Adopted --slot--> Running
//!       | Done
//!       v
//!   (children still live? park the Done until they settle)
//!       v
//!   Merge (writer, finished) -> lift fence -> report row -> record settled
//!       -> session ended -> report injected into the parent (once)
//! ```
//!
//! **Disjoint writes.** A writer child's paths are fenced on its parent's
//! `ScopeFence` before the checkpoint is taken and lifted only after the
//! child's work is merged, so the parent writes a path only while no child
//! holds it; the child writes only its own worktree, and only its paths
//! (its brief's write scope). The runtime refuses a fenced write both when a
//! round is approved and when each call starts (`kgraf-runtime`).
//! A read-only child shares the parent's worktree under
//! `ScopeFence::read_only()`, with the parent's tool list unchanged, so both
//! send one prefix (0043).
//!
//! **The wakeup is registered before the child is detached.** The spawn is
//! saved in the parent contract's `worktrees` map (a [`SpawnRecord`]) before
//! the child's task starts. That record names the parent, the task and the
//! paths, so a killed run re-adopts the child when its parent is dispatched
//! again, fences the parent before the parent's worker exists, and continues
//! the child from its transcript: no completed round is paid again. The
//! report goes into the parent's inbox before the record is marked settled,
//! and its id is `subagent-report:<child>`, which the runtime refuses to
//! inject twice into one transcript: it reaches the parent once.
//!
//! **A parent that finishes first** is held (its `Finished` parked) until
//! its children settle, so a writer child's work is merged before the
//! parent's attempt is verified. A parent that fails cancels its children;
//! a parent that is stopped cancels them resumably. A run stop does not
//! cancel the children of a parent that already finished: they run to their
//! end, because cancelling them would let the parent's attempt be verified
//! without their work; the stop waits for them.
//!
//! Admission: a writer child takes one work item and one agent slot through
//! `AdmissionGate::try_admit(1)`, the gate every dispatch uses. A read-only
//! child takes only an agent slot (`AdmissionGate::try_admit_agents(1)`): it
//! writes nothing and shares its parent's worktree, so it is part of its
//! parent's work item, and a busy run does not refuse it. The live-agent
//! ceilings bind both. The depth limit comes from
//! [`ParallelismLimits::derive`] of the gate's live-agent ceiling (the
//! slider), and so does the soft notice on children per parent.

use super::*;
use crate::subagent_tool::{
    ChildMerge, MAX_REPORT_ANSWER_CHARS, SUBAGENT_REPORT_PREFIX, SpawnRefusal, SpawnRequest,
    Spawned, SubagentReport,
};
use kgraf_core::TaskId;
use kgraf_core::parallel::ParallelismLimits;
use kgraf_core::session::{
    ConnectionBasis, InboxItem, InboxSource, ReasoningVisibility, SessionFacts, SessionId,
    SessionKind, SessionOutcome, SessionRecords,
};
use kgraf_graph::SharedSessions;
use kgraf_runtime::{FenceGuard, InjectError, Injection};
use serde::{Deserialize, Serialize};

/// What reaches the loop about subagents.
pub(super) enum SubagentMsg {
    Spawn {
        parent: AgentId,
        request: SpawnRequest,
        reply: oneshot::Sender<Result<Spawned, SpawnRefusal>>,
    },
    Done(Box<ChildDone>),
}

/// A child's worker ended.
pub(super) struct ChildDone {
    child: AgentId,
    result: WorkerResult,
    /// The child never ran: its environment or worktree was not provided.
    env_failure: Option<String>,
    /// Its merge, already known without running it: a result recorded
    /// before the process stopped and merged again (`recover_child_merge`).
    recovered: Option<ChildMerge>,
}

/// The durable spawn, kept in the parent contract's `worktrees` map under
/// the child's id: opaque to the workspace provider (it is asked for a
/// child's worktree only through `prepare_child`), and it keeps the child's
/// worktree from startup cleanup, which spares every name that map holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SpawnRecord {
    subagent: SpawnFacts,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SpawnFacts {
    parent: String,
    task: String,
    route: String,
    writes: Vec<String>,
    read_only: bool,
    /// Delegation hops below the worker: 1 for a worker's child.
    depth: u32,
    /// `(session, task)` from the worker down to this child, inclusive: the
    /// trace the cycle and depth guards read.
    lineage: Vec<(String, String)>,
    /// The parent's round that spawned it: the child's first round names it
    /// as its `parent_call`.
    parent_call: Option<String>,
    /// Settled: merged, reported and ended. Kept so its worktree (a
    /// conflict's, say) stays protected until the contract closes.
    #[serde(default)]
    settled: bool,
    /// Written before the child's worktree exists and cleared once its rows
    /// are written: a record still `preparing` after a kill is a spawn that
    /// never started, and whatever it left is discarded.
    #[serde(default)]
    preparing: bool,
}

/// One live child (running, or adopted and waiting for a slot).
struct LiveChild {
    facts: SpawnFacts,
    contract: String,
    task: TaskId,
    /// Where the child works: its own worktree, or its parent's (read-only).
    workspace: PathBuf,
    parent_workspace: PathBuf,
    cancel: Arc<Notify>,
    /// The child's own fence: read-only for a read-only child; for a writer,
    /// the one its own writer children fence paths on.
    fence: ScopeFence,
    injector: Arc<OnceLock<Injector>>,
    /// The fence on the parent over this child's paths (writer only).
    parent_guard: Option<FenceGuard>,
    admitted: Option<Admitted>,
    reservation: Option<Reservation>,
    running: bool,
    /// `Some(resumable)` once its parent ended; `true` when the parent was
    /// stopped (and so is continued later), `false` when it failed.
    ended_parent: Option<bool>,
    /// Reopen attempts that found its worktree busy, and when to try next.
    busy_tries: u32,
    retry_at_ms: i64,
    /// The over-limit notice was sent for this adoption. Every notice has its
    /// own id, so nothing else stops a busy retry from sending it again.
    over_limit_notified: bool,
    /// The user stopped this child directly (`stop_session`): its `Done`
    /// pauses it rather than settling it, and `resume_session` continues it.
    user_stopped: bool,
    /// A `resume_session` arrived while the stop it is resuming from was
    /// still in flight (`running` was still `true`: the cancel was sent but
    /// its `Done` had not reached `on_child_done` yet). Clearing
    /// `user_stopped` right away would desync it from that pending `Done`:
    /// `on_child_done`'s pause check reads `user_stopped` to tell a requested
    /// pause from a genuine end, so a resume that raced ahead of the
    /// confirmation would make a merely-*requested* cancel look like an
    /// unrequested one, retiring the child for good instead of pausing and
    /// restarting it (its queued inbox rows lost with it). Held here and applied once `pause_child` actually runs.
    resume_after_stop: bool,
    /// Resolves when the child's detached task ends (its sender drops with
    /// the task). Set when the child starts.
    ended: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// A finish held until the finisher's children settle.
enum Parked {
    Attempt(Box<Finished>),
    Child(Box<ChildDone>),
}

/// A report on its way to the parent's transcript.
struct Wake {
    parent: AgentId,
    injection: Injection,
}

/// The loop's subagent state.
#[derive(Default)]
pub(super) struct Subagents {
    live: BTreeMap<AgentId, LiveChild>,
    parked: BTreeMap<AgentId, Parked>,
    /// Paths merged into a parent that already finished: added to its
    /// result's `changed_paths` when its finish is released.
    merged_paths: BTreeMap<AgentId, Vec<PathBuf>>,
    wakes: BTreeMap<String, Wake>,
    /// Adopted children waiting for a slot.
    restart: BTreeSet<AgentId>,
    /// Parents already given the soft-children notice.
    noticed: BTreeSet<AgentId>,
    /// Session-store failures seen off an async path, traced on the next tick
    /// (never dropped).
    errors: Vec<String>,
    /// Settled children whose spend could not be read from the ledger and who had
    /// no hold to stand in for it. Retried at every tick until the read works;
    /// until then their spend is missing from the budget, and that was said.
    uncharged: Vec<Uncharged>,
    orchestrator_row: bool,
    /// Spawns a killed process left `preparing`: `(contract, child)`.
    never_started: BTreeSet<(String, AgentId)>,
    /// Stop requests seen, the first one's time, and whether the children of
    /// finished parents were already stopped.
    stops: u32,
    stop_at_ms: Option<i64>,
    stopped_held: bool,
}

/// A settled child's calls whose spend has not been charged yet.
struct Uncharged {
    child: AgentId,
    task: TaskId,
    calls: Vec<CallId>,
}

/// The inbox id prefix of a notice about a child to its parent.
pub(crate) const SUBAGENT_NOTICE_PREFIX: &str = "subagent-notice:";

/// Reopen attempts on a busy worktree before the child is settled as failed.
const MAX_BUSY_TRIES: u32 = 8;

/// The longest wait between two reopen attempts.
const MAX_BUSY_BACKOFF_MS: i64 = 30_000;

/// How long a run stop lets the children of already-finished parents run
/// before it stops them (resumably). A second stop stops them at once.
pub(crate) const SUBAGENT_STOP_GRACE_MS: i64 = 30_000;

/// A running agent that may spawn: a contract's attempt or a live child.
struct ParentView {
    contract: String,
    task: TaskId,
    route: String,
    workspace: PathBuf,
    writes: Vec<String>,
    fence: ScopeFence,
    lineage: Vec<(String, String)>,
    /// The parent's session depth: 1 for a worker.
    session_depth: u32,
    reservation: Money,
}

enum Slot {
    Ready(Injector),
    NotYet,
    Gone,
}

impl OrchestratorHandle {
    /// A worker's `spawn_subagent` call, answered by the loop.
    pub(crate) async fn spawn_subagent(
        &self,
        parent: AgentId,
        request: SpawnRequest,
    ) -> Result<Spawned, SpawnRefusal> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(LoopMsg::Subagent(Box::new(SubagentMsg::Spawn {
                parent,
                request,
                reply,
            })))
            .await
            .map_err(|_| SpawnRefusal::Stopped)?;
        rx.await.map_err(|_| SpawnRefusal::Stopped)?
    }
}

fn is_finished_work(outcome: &WorkerOutcome) -> bool {
    matches!(
        outcome,
        WorkerOutcome::Completed | WorkerOutcome::HandedBack { .. }
    )
}

fn session_outcome(outcome: &WorkerOutcome) -> SessionOutcome {
    match outcome {
        WorkerOutcome::Completed => SessionOutcome::Completed,
        WorkerOutcome::HandedBack { .. } => SessionOutcome::HandedBack,
        WorkerOutcome::Cancelled => SessionOutcome::Cancelled,
        WorkerOutcome::Stalled { .. } => SessionOutcome::Stalled,
        WorkerOutcome::BudgetExhausted => SessionOutcome::BudgetExhausted,
        WorkerOutcome::Failed { .. } => SessionOutcome::Failed,
    }
}

fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!(
            "{}... (cut; the whole answer is in its transcript)",
            &text[..at]
        ),
        None => text.to_owned(),
    }
}

/// The child's brief. The task is the parent's words, verbatim; the lines
/// around it say what the child may do.
fn child_instruction(child: &AgentId, facts: &SpawnFacts) -> String {
    let mode = if facts.read_only {
        "You are read-only: you share your parent's worktree for reading, and every write you \
         attempt is refused."
            .to_owned()
    } else {
        format!(
            "You may write only these files: {}. You work in your own worktree, branched from \
             your parent's current work; your changes are merged into your parent's worktree \
             when you finish.",
            facts.writes.join(", ")
        )
    };
    format!(
        "You are subagent `{child}`, spawned by `{}`. {mode}\n\nYour task:\n{}\n\nWhen you are \
         done, answer with a short report of what you did and found: your parent receives it.",
        facts.parent, facts.task
    )
}

/// The one place a session row is built, so a field added to
/// `SessionFacts` is added once.
#[allow(clippy::too_many_arguments)]
fn session_facts(
    id: &AgentId,
    run: &RunId,
    kind: SessionKind,
    parent: Option<SessionId>,
    contract: Option<String>,
    route: Option<String>,
    model: Option<String>,
    depth: u32,
    transcript: Option<PathBuf>,
    worktree: Option<PathBuf>,
    write_scope: Vec<String>,
    started_ms: i64,
) -> SessionFacts {
    SessionFacts {
        id: id.clone(),
        run: run.clone(),
        kind,
        parent,
        contract,
        route,
        model,
        depth,
        transcript,
        worktree,
        write_scope,
        reasoning: ReasoningVisibility::Unknown,
        started_ms,
        // A subagent, a worker and the orchestrator are addressable; only a
        // merge-conflict resolver is not (decision 0073).
        addressable: true,
    }
}

impl RunLoop {
    // ------------------------------------------------------------- limits

    fn sessions(&self) -> SharedSessions {
        SharedSessions(self.s.graph.clone())
    }

    /// The slider's derived limits: `N` is the gate's live-agent ceiling.
    fn limits(&self) -> ParallelismLimits {
        let l = self.s.admission.gate().limits();
        let n = u32::try_from(l.max_live_agents()).unwrap_or(u32::MAX);
        let w = u32::try_from(l.max_work_items()).unwrap_or(u32::MAX);
        ParallelismLimits::derive(n, w, 1)
    }

    /// The delegation policy with its depth from the slider (decision 0067,
    /// point 8): used for subagents and for the handoff re-split alike.
    pub(super) fn delegation_policy(&self) -> DelegationPolicy {
        self.config.delegation.with_limits(&self.limits())
    }

    // ------------------------------------------------------------- views

    fn parent_view(&self, id: &AgentId) -> Option<ParentView> {
        if let Some(c) = self.subagents.live.get(id) {
            if !c.running {
                return None;
            }
            return Some(ParentView {
                contract: c.contract.clone(),
                task: c.task.clone(),
                route: c.facts.route.clone(),
                workspace: c.workspace.clone(),
                writes: if c.facts.read_only {
                    Vec::new()
                } else {
                    c.facts.writes.clone()
                },
                fence: c.fence.clone(),
                lineage: c.facts.lineage.clone(),
                session_depth: 1 + c.facts.depth,
                reservation: c
                    .reservation
                    .as_ref()
                    .map(Reservation::amount)
                    .unwrap_or(Money::ZERO),
            });
        }
        if self.subagents.parked.contains_key(id) {
            return None;
        }
        for (cid, f) in &self.in_flight {
            // `cancels` holds an attempt from dispatch until its finish.
            if !f.cancels.contains_key(id) {
                continue;
            }
            let fence = f.fences.get(id)?;
            let attempt = f.attempts.iter().find(|a| &a.worker == id)?;
            let contract = self.record.contract(cid)?;
            return Some(ParentView {
                contract: cid.clone(),
                task: contract.task.clone(),
                route: attempt.route.clone(),
                workspace: attempt.workspace.clone(),
                writes: contract.writes.iter().map(|w| w.path.clone()).collect(),
                fence: fence.clone(),
                lineage: vec![(id.to_string(), contract.instruction.clone())],
                session_depth: 1,
                reservation: f
                    .reservations
                    .get(id)
                    .map(Reservation::amount)
                    .unwrap_or(f.estimate),
            });
        }
        None
    }

    fn children_of(&self, parent: &AgentId) -> Vec<AgentId> {
        self.subagents
            .live
            .iter()
            .filter(|(_, c)| c.facts.parent == parent.as_str())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Every spawn record of the contract (settled ones included).
    fn spawn_records(&self, contract: &str) -> Vec<(AgentId, SpawnFacts)> {
        self.record
            .contract(contract)
            .map(|c| {
                c.worktrees
                    .iter()
                    .filter_map(|(k, v)| {
                        serde_json::from_str::<SpawnRecord>(v)
                            .ok()
                            .map(|r| (AgentId::new(k.clone()), r.subagent))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `<parent>.s<n>`, never reusing an `n` the record or the store has.
    fn next_child_id(&self, parent: &AgentId, contract: &str) -> Result<AgentId, String> {
        let prefix = format!("{parent}.s");
        let n_of = |id: &str| {
            id.strip_prefix(&prefix)
                .and_then(|rest| rest.parse::<u32>().ok())
        };
        let store = self.sessions();
        // A failed read must refuse, not guess: an id reused while an
        // earlier child's refs remain would apply that child's work.
        let stored: Vec<String> = blocking(|| store.children(parent))
            .map(|v| v.into_iter().map(|f| f.id.to_string()).collect())
            .map_err(|e| format!("the children of `{parent}` could not be read: {e}"))?;
        let most = self
            .spawn_records(contract)
            .iter()
            .map(|(id, _)| id.to_string())
            .chain(self.subagents.live.keys().map(ToString::to_string))
            .chain(stored)
            .filter_map(|id| n_of(&id))
            .max()
            .unwrap_or(0);
        Ok(AgentId::new(format!("{prefix}{}", most + 1)))
    }

    /// The parent's last model round: the one whose tool call is spawning.
    fn last_call_of(&self, agent: &AgentId, task: &TaskId) -> Option<CallId> {
        let ledger = self.s.ledger.clone();
        blocking(|| ledger.calls_under_task(task.as_str(), Some(agent.as_str())))
            .ok()?
            .into_iter()
            .filter(|c| c.agent_id == agent.as_str())
            .map(|c| c.id)
            .next_back()
    }

    fn transcript_rel(&self, id: &AgentId) -> PathBuf {
        let path = self.files.worker_transcript(id.as_str());
        match self.s.runs_dir.parent() {
            Some(state) => path
                .strip_prefix(state)
                .map(Path::to_path_buf)
                .unwrap_or(path),
            None => path,
        }
    }

    // ------------------------------------------------------------- rows

    pub(super) fn orchestrator_session(&self) -> AgentId {
        AgentId::new(format!("orchestrator:{}", self.record.run))
    }

    pub(super) fn ensure_orch_session_row(&mut self) {
        if self.subagents.orchestrator_row {
            return;
        }
        let id = self.orchestrator_session();
        let facts = session_facts(
            &id,
            &self.record.run,
            SessionKind::Orchestrator,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            Vec::new(),
            self.now(),
        );
        match self.record_session(facts) {
            Ok(()) => self.subagents.orchestrator_row = true,
            Err(e) => self.subagents.errors.push(e.to_string()),
        }
    }

    /// The handed-back session a handoff child continues (decision 0067:
    /// "handoff children take the handed-back session as parent"): the
    /// superseded contract `x` of child contract `x.h<n>`, its last worker.
    fn handoff_parent(&self, contract: &str) -> Option<AgentId> {
        let (base, n) = contract.rsplit_once(".h")?;
        n.parse::<u32>().ok()?;
        self.record
            .coordination
            .closed
            .iter()
            .rev()
            .find(|c| c.contract == base)
            .and_then(|c| c.workers.last())
            .map(|w| AgentId::new(w.clone()))
    }

    /// A worker's session row at dispatch: its parent is the orchestrator, or
    /// for a handoff child the session that handed back. A resolver contract
    /// (decision 0073) gets `SessionKind::Resolver` and
    /// `addressable: false` instead: the one exception to "every worker is
    /// addressable".
    fn record_worker_row(
        &mut self,
        contract: &OpenContract,
        worker: &AgentId,
        route: &str,
        workspace: &Path,
    ) {
        self.ensure_orch_session_row();
        let parent = self
            .handoff_parent(&contract.contract)
            .unwrap_or_else(|| self.orchestrator_session());
        let model = self.route(route).map(|r| r.model().to_owned());
        let is_resolver = contract.resolver_for.is_some();
        let kind = if is_resolver {
            SessionKind::Resolver
        } else {
            SessionKind::Worker
        };
        let mut facts = session_facts(
            worker,
            &self.record.run,
            kind,
            Some(parent),
            Some(contract.contract.clone()),
            Some(route.to_owned()),
            model,
            1,
            Some(self.transcript_rel(worker)),
            Some(workspace.to_path_buf()),
            contract.writes.iter().map(|w| w.path.clone()).collect(),
            self.now(),
        );
        if is_resolver {
            facts.addressable = false;
        }
        if let Err(e) = self.record_session(facts) {
            self.subagents.errors.push(e.to_string());
        }
    }

    // ------------------------------------------------------------- hooks

    /// Hook in `start_attempt`, before the worker's task exists: record the
    /// worker's session row, and re-adopt children a killed or stopped
    /// process left, fencing their paths on this attempt's fence before the
    /// worker can write.
    pub(super) fn adopt_subagents(
        &mut self,
        contract: &OpenContract,
        worker: &AgentId,
        route: &str,
        workspace: &Path,
        fence: &ScopeFence,
    ) {
        self.record_worker_row(contract, worker, route, workspace);
        self.adopt_children_of(&contract.contract, worker, workspace, fence);
    }

    fn adopt_children_of(
        &mut self,
        contract: &str,
        parent: &AgentId,
        parent_workspace: &Path,
        parent_fence: &ScopeFence,
    ) {
        let task = match self.record.contract(contract) {
            Some(c) => c.task.clone(),
            None => return,
        };
        for (child, facts) in self.spawn_records(contract) {
            if facts.parent != parent.as_str()
                || facts.settled
                || self.subagents.live.contains_key(&child)
            {
                continue;
            }
            if facts.preparing {
                // Killed between its record and its rows: it never ran.
                self.subagents
                    .never_started
                    .insert((contract.to_owned(), child));
                continue;
            }
            let parent_guard = (!facts.read_only).then(|| parent_fence.fence(&facts.writes));
            let fence = if facts.read_only {
                ScopeFence::read_only()
            } else {
                ScopeFence::default()
            };
            self.subagents.live.insert(
                child.clone(),
                LiveChild {
                    contract: contract.to_owned(),
                    task: task.clone(),
                    workspace: parent_workspace.to_path_buf(),
                    parent_workspace: parent_workspace.to_path_buf(),
                    cancel: Arc::new(Notify::new()),
                    fence,
                    injector: Arc::new(OnceLock::new()),
                    parent_guard,
                    admitted: None,
                    reservation: None,
                    running: false,
                    ended_parent: None,
                    busy_tries: 0,
                    retry_at_ms: 0,
                    over_limit_notified: false,
                    user_stopped: false,
                    resume_after_stop: false,
                    ended: None,
                    facts,
                },
            );
            self.subagents.restart.insert(child);
        }
        // Reports that reached the inbox and maybe not the transcript: offered
        // again; the runtime refuses an id the transcript already holds.
        let store = self.sessions();
        match blocking(|| store.undelivered(parent)) {
            Ok(items) => {
                for item in items {
                    if item.id.starts_with(SUBAGENT_REPORT_PREFIX)
                        || item.id.starts_with(SUBAGENT_NOTICE_PREFIX)
                    {
                        let injection = Injection {
                            id: item.id.clone(),
                            source: item.source,
                            kind: item.kind,
                            mode: item.mode,
                            // The model's copy (decision 0071).
                            text: kgraf_secrets::model_copy(&item.text).into_owned(),
                        };
                        self.subagents.wakes.insert(
                            item.id,
                            Wake {
                                parent: parent.clone(),
                                injection,
                            },
                        );
                    }
                }
            }
            Err(e) => self
                .subagents
                .errors
                .push(format!("the inbox of `{parent}` could not be read: {e}")),
        }
    }

    /// Hook at the top of `on_finished`: a worker with live children is held
    /// until they settle (`None`); otherwise its finish goes on (`Some`).
    pub(super) async fn hold_for_subagents(
        &mut self,
        f: Finished,
    ) -> Result<Option<Finished>, OrchestratorError> {
        let parent = f.worker.clone();
        if self.children_of(&parent).is_empty() {
            return Ok(Some(self.with_merged_paths(f)));
        }
        if !is_finished_work(&f.result.outcome) {
            let resumable = matches!(f.result.outcome, WorkerOutcome::Cancelled);
            self.end_children(&parent, resumable).await?;
        }
        if self.children_of(&parent).is_empty() {
            return Ok(Some(self.with_merged_paths(f)));
        }
        self.subagents
            .parked
            .insert(parent, Parked::Attempt(Box::new(f)));
        Ok(None)
    }

    fn with_merged_paths(&mut self, mut f: Finished) -> Finished {
        if let Some(paths) = self.subagents.merged_paths.remove(&f.worker) {
            for p in paths {
                if !f.result.changed_paths.contains(&p) {
                    f.result.changed_paths.push(p);
                }
            }
        }
        f
    }

    /// The parent ended without finishing its work: cancel every descendant.
    /// `resumable`: the parent was stopped and will be continued, so the
    /// children's sessions stay open to be adopted again.
    async fn end_children(
        &mut self,
        parent: &AgentId,
        resumable: bool,
    ) -> Result<(), OrchestratorError> {
        for child in self.children_of(parent) {
            let running = match self.subagents.live.get_mut(&child) {
                Some(c) => {
                    c.ended_parent = Some(resumable);
                    if c.running {
                        c.cancel.notify_one();
                    }
                    c.running
                }
                None => continue,
            };
            // Box: the recursion is async.
            Box::pin(self.end_children(&child, resumable)).await?;
            if !running {
                // Adopted and never restarted: nothing runs to report back.
                self.subagents.restart.remove(&child);
                let done = ChildDone {
                    child: child.clone(),
                    result: self.unstarted_result(&child),
                    env_failure: Some("its parent ended before it ran again".into()),
                    recovered: None,
                };
                self.settle_child(done).await?;
            }
        }
        Ok(())
    }

    fn unstarted_result(&self, child: &AgentId) -> WorkerResult {
        let task = self
            .subagents
            .live
            .get(child)
            .map(|c| c.task.clone())
            .unwrap_or_else(|| TaskId::new(""));
        WorkerResult::bare(task, child.clone(), WorkerOutcome::Cancelled)
    }

    /// Hook after every loop message: deliver reports, restart adopted
    /// children that now have a slot, and trace store failures.
    pub(super) async fn subagent_tick(&mut self) -> Result<(), OrchestratorError> {
        // Every drained detail is traced before a store failure is returned:
        // the list is already taken, so an early return would drop the rest.
        let mut failed = None;
        for detail in std::mem::take(&mut self.subagents.errors) {
            let at = self.now();
            let traced = self
                .trace(TraceEvent::Notice {
                    at_ms: at,
                    contract: String::new(),
                    worker: String::new(),
                    detail: format!("subagent sessions: {detail}"),
                })
                .await;
            if let Err(e) = traced {
                failed.get_or_insert(e);
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
        self.charge_unread_children().await;
        if !self.subagents.never_started.is_empty() {
            self.discard_never_started().await?;
        }
        if self.stopping && !self.subagents.stopped_held {
            let now = self.now();
            let first = *self.subagents.stop_at_ms.get_or_insert(now);
            if self.subagents.stops >= 2 || now - first >= SUBAGENT_STOP_GRACE_MS {
                self.stop_held_children().await?;
            }
        }
        if !self.subagents.restart.is_empty() {
            self.restart_children().await?;
        }
        if !self.subagents.wakes.is_empty() {
            self.deliver_wakes().await?;
        }
        Ok(())
    }

    /// Hook in `Control::Stop`. A stop cancels running workers; the children
    /// of a parent that already finished keep running, so the parent is not
    /// verified without their work, for up to [`SUBAGENT_STOP_GRACE_MS`], or
    /// until a second stop.
    pub(super) fn on_stop_subagents(&mut self) {
        self.subagents.stops += 1;
        let now = self.now();
        self.subagents.stop_at_ms.get_or_insert(now);
        let held: Vec<AgentId> = self.subagents.parked.keys().cloned().collect();
        let waiting: Vec<AgentId> = held.iter().flat_map(|p| self.children_of(p)).collect();
        if self.subagents.stops == 1 && !waiting.is_empty() {
            self.subagents.errors.push(format!(
                "stop: subagents {waiting:?} of agents that already finished keep running for up \
                 to {}s so their work is merged; stop again to stop them now (resumable)",
                SUBAGENT_STOP_GRACE_MS / 1000
            ));
        }
    }

    /// Stop the children of every held (finished) parent, resumably: each
    /// held finish becomes a stop, so its attempt is kept for the resume
    /// rather than verified without its children's work, and each child's
    /// session is marked paused, the state the session view shows.
    async fn stop_held_children(&mut self) -> Result<(), OrchestratorError> {
        self.subagents.stopped_held = true;
        let now = self.now();
        for parked in self.subagents.parked.values_mut() {
            match parked {
                Parked::Attempt(f) => f.result.outcome = WorkerOutcome::Cancelled,
                Parked::Child(d) => d.result.outcome = WorkerOutcome::Cancelled,
            }
        }
        let held: Vec<AgentId> = self.subagents.parked.keys().cloned().collect();
        let mut stopped = Vec::new();
        for parent in held {
            for child in self.children_of(&parent) {
                stopped.push(child);
            }
            self.end_children(&parent, true).await?;
        }
        let store = self.sessions();
        for child in &stopped {
            if let Err(e) = blocking(|| store.set_paused(child, Some(now))) {
                self.subagents.errors.push(format!(
                    "`{child}` stopped; its paused mark could not be written: {e}"
                ));
            }
        }
        if !stopped.is_empty() {
            self.subagents.errors.push(format!(
                "stop: subagents {stopped:?} were stopped with their finished parents' attempts \
                 kept; a resume continues them"
            ));
        }
        Ok(())
    }

    /// Spawns a killed process left `preparing`: their worktree and refs are
    /// removed, the record dropped, and the parent told (never silent).
    async fn discard_never_started(&mut self) -> Result<(), OrchestratorError> {
        for (contract, child) in std::mem::take(&mut self.subagents.never_started) {
            let parent = self
                .spawn_records(&contract)
                .into_iter()
                .find(|(id, _)| *id == child)
                .map(|(_, f)| AgentId::new(f.parent));
            let discarded = self.s.workspaces.discard_child(&child).await;
            self.drop_spawn_record(&contract, &child);
            let text = match discarded {
                Ok(()) => format!(
                    "subagent `{child}` was never started: the process stopped while it was \
                     being prepared. Spawn it again if you still need it."
                ),
                Err(e) => format!(
                    "subagent `{child}` was never started, and what it left could not be \
                     removed: {e}"
                ),
            };
            if let Some(parent) = parent {
                self.notify_parent(&parent, &child, "never-started", &text);
            } else {
                self.subagents.errors.push(text);
            }
        }
        Ok(())
    }

    pub(super) async fn on_subagent(&mut self, m: SubagentMsg) -> Result<(), OrchestratorError> {
        match m {
            SubagentMsg::Spawn {
                parent,
                request,
                reply,
            } => {
                let spawned = self.spawn_child(parent.clone(), request).await;
                let at = self.now();
                let (action, to, detail) = match &spawned {
                    Ok(s) => (
                        "subagent_spawned",
                        vec![s.child.to_string()],
                        if s.read_only {
                            "read-only".to_owned()
                        } else {
                            format!("writer on {:?}", s.writes)
                        },
                    ),
                    Err(r) => ("subagent_refused", Vec::new(), r.to_string()),
                };
                self.trace(TraceEvent::Message {
                    at_ms: at,
                    id: to.first().cloned().unwrap_or_default(),
                    action: action.into(),
                    from: parent.to_string(),
                    to,
                    detail,
                })
                .await?;
                let _ = reply.send(spawned);
            }
            SubagentMsg::Done(d) => self.on_child_done(*d).await?,
        }
        Ok(())
    }

    // ------------------------------------------------------------- spawn

    async fn spawn_child(
        &mut self,
        parent: AgentId,
        request: SpawnRequest,
    ) -> Result<Spawned, SpawnRefusal> {
        if self.stopping {
            return Err(SpawnRefusal::Stopped);
        }
        let Some(p) = self.parent_view(&parent) else {
            return Err(SpawnRefusal::UnknownParent(parent.to_string()));
        };
        let mut writes: Vec<String> = Vec::new();
        for w in request.writes.iter().map(|w| w.trim()) {
            if !w.is_empty() && !writes.iter().any(|x| x == w) {
                writes.push(w.to_owned());
            }
        }
        if request.read_only && !writes.is_empty() {
            return Err(SpawnRefusal::ReadOnlyWrites);
        }
        if !request.read_only && writes.is_empty() {
            return Err(SpawnRefusal::NoWrites);
        }
        let outside: Vec<String> = writes
            .iter()
            .filter(|w| {
                !p.writes
                    .iter()
                    .any(|s| kgraf_runtime::same_path(Path::new(s), Path::new(w)))
            })
            .cloned()
            .collect();
        if !outside.is_empty() {
            return Err(SpawnRefusal::NotSubset {
                paths: outside,
                scope: p.writes.clone(),
            });
        }
        let held: Vec<String> = writes
            .iter()
            .filter(|w| p.fence.is_fenced(w))
            .cloned()
            .collect();
        if !held.is_empty() {
            return Err(SpawnRefusal::Fenced { paths: held });
        }
        let roster = self.roster();
        let target = request.route.clone().unwrap_or_else(|| p.route.clone());
        let trace: Vec<String> = p.lineage.iter().map(|(id, _)| id.clone()).collect();
        let tasks: Vec<String> = p.lineage.iter().map(|(_, t)| t.clone()).collect();
        let policy = self.delegation_policy();
        let delegation = guards::check(
            &policy,
            &roster,
            &DelegationRequest {
                source_contract: parent.as_str(),
                source_route: &p.route,
                target_route: &target,
                task: &request.task,
                trace: &trace,
                trace_tasks: &tasks,
            },
        );
        let delegation = match delegation {
            Ok(d) => d,
            Err(refusal) => {
                if matches!(refusal, guards::GuardRefusal::Cycle { .. }) {
                    let at = self.now();
                    let _ = self
                        .trace(TraceEvent::LoopPattern {
                            at_ms: at,
                            contract: p.contract.clone(),
                            kind: LoopKind::Alternating,
                            detail: refusal.to_string(),
                        })
                        .await;
                }
                return Err(SpawnRefusal::Guard(refusal));
            }
        };
        // Pre-flight: the slot, then the money, before anything exists.
        let admitted = {
            let mut gate = self.s.admission.gate();
            if request.read_only {
                gate.try_admit_agents(1)
            } else {
                gate.try_admit(1)
            }
        }
        .map_err(SpawnRefusal::NoSlot)?;
        let now = self.now();
        let run = self.run_id();
        let reservation =
            self.s
                .admission
                .budget()
                .reserve(&run, p.task.as_str(), p.reservation, now, true);
        let release = |me: &mut Self, admitted: Admitted, reservation: Reservation| {
            me.s.admission.gate().release(admitted);
            me.s.admission.budget().release(reservation);
        };
        let child = match self.next_child_id(&parent, &p.contract) {
            Ok(c) => c,
            Err(e) => {
                release(self, admitted, reservation);
                return Err(SpawnRefusal::Store(e));
            }
        };
        let mut lineage = p.lineage.clone();
        lineage.push((child.to_string(), request.task.clone()));
        let mut facts = SpawnFacts {
            parent: parent.to_string(),
            task: request.task.clone(),
            route: delegation.target_route.clone(),
            writes: writes.clone(),
            read_only: request.read_only,
            depth: delegation.depth,
            lineage,
            parent_call: self.last_call_of(&parent, &p.task).map(|c| c.to_string()),
            settled: false,
            preparing: true,
        };
        // Durable before anything exists: a kill from here on leaves a
        // record that says what to clean up.
        if let Err(e) = self.put_spawn_record(&p.contract, &child, &facts) {
            release(self, admitted, reservation);
            return Err(SpawnRefusal::Store(e));
        }
        // The parent's writes to the child's paths stop before the checkpoint.
        let parent_guard = (!request.read_only).then(|| p.fence.fence(&writes));
        let workspace = if request.read_only {
            p.workspace.clone()
        } else {
            match self.s.workspaces.prepare_child(&p.workspace, &child).await {
                Ok(w) => w,
                Err(e) => {
                    drop(parent_guard);
                    self.discard_spawn(&p.contract, &child, false).await;
                    release(self, admitted, reservation);
                    return Err(SpawnRefusal::Workspace(e.to_string()));
                }
            }
        };
        let model = self.route(&facts.route).map(|r| r.model().to_owned());
        self.ensure_orch_session_row();
        let row = session_facts(
            &child,
            &self.record.run,
            SessionKind::Subagent,
            Some(parent.clone()),
            Some(p.contract.clone()),
            Some(facts.route.clone()),
            model,
            p.session_depth + 1,
            Some(self.transcript_rel(&child)),
            Some(workspace.clone()),
            writes.clone(),
            now,
        );
        let recorded = self
            .record_session(row)
            .and_then(|()| self.open_by_rule(&parent, &child, ConnectionBasis::ParentChild));
        facts.preparing = false;
        let recorded = recorded.and_then(|()| {
            self.put_spawn_record(&p.contract, &child, &facts)
                .map_err(|e| OrchestratorError::Store(StoreError::Decision(e)))
        });
        if let Err(e) = recorded {
            drop(parent_guard);
            self.discard_spawn(&p.contract, &child, !request.read_only)
                .await;
            release(self, admitted, reservation);
            return Err(SpawnRefusal::Store(e.to_string()));
        }
        let notice = self.children_notice(&parent, &p.contract);
        let fence = if request.read_only {
            ScopeFence::read_only()
        } else {
            ScopeFence::default()
        };
        self.subagents.live.insert(
            child.clone(),
            LiveChild {
                facts,
                contract: p.contract.clone(),
                task: p.task.clone(),
                workspace,
                parent_workspace: p.workspace.clone(),
                cancel: Arc::new(Notify::new()),
                fence,
                injector: Arc::new(OnceLock::new()),
                parent_guard,
                admitted: Some(admitted),
                reservation: Some(reservation),
                running: false,
                ended_parent: None,
                busy_tries: 0,
                retry_at_ms: 0,
                over_limit_notified: false,
                user_stopped: false,
                resume_after_stop: false,
                ended: None,
            },
        );
        if let Err(e) = self.start_child(&child) {
            // Recorded but not started: settled as failed, so the parent
            // still hears of it (never silent).
            let mut result = self.unstarted_result(&child);
            result.outcome = WorkerOutcome::Failed { error: e.clone() };
            let _ = self
                .settle_child(ChildDone {
                    child: child.clone(),
                    result,
                    env_failure: Some(e.clone()),
                    recovered: None,
                })
                .await;
            return Err(SpawnRefusal::Workspace(e));
        }
        Ok(Spawned {
            child,
            read_only: request.read_only,
            writes,
            notice,
        })
    }

    fn put_spawn_record(
        &mut self,
        contract: &str,
        child: &AgentId,
        facts: &SpawnFacts,
    ) -> Result<(), String> {
        let value = serde_json::to_string(&SpawnRecord {
            subagent: facts.clone(),
        })
        .map_err(|e| e.to_string())?;
        let Some(c) = self.record.contract_mut(contract) else {
            return Err(format!("contract `{contract}` is not open"));
        };
        c.worktrees.insert(child.to_string(), value);
        self.save().map_err(|e| e.to_string())
    }

    /// Undo a spawn that failed after its record was written: the worktree
    /// and refs (`prepared`: a writer's worktree exists), then the record.
    async fn discard_spawn(&mut self, contract: &str, child: &AgentId, prepared: bool) {
        if prepared && let Err(e) = self.s.workspaces.discard_child(child).await {
            self.subagents.errors.push(format!(
                "the worktree of the refused subagent `{child}` could not be removed: {e}"
            ));
        }
        self.drop_spawn_record(contract, child);
    }

    fn drop_spawn_record(&mut self, contract: &str, child: &AgentId) {
        if let Some(c) = self.record.contract_mut(contract) {
            c.worktrees.remove(child.as_str());
        }
        if let Err(e) = self.save() {
            self.subagents.errors.push(e.to_string());
        }
    }

    fn settle_record(&mut self, contract: &str, child: &AgentId) {
        let Some(c) = self.record.contract_mut(contract) else {
            return;
        };
        let Some(value) = c.worktrees.get_mut(child.as_str()) else {
            return;
        };
        if let Ok(mut r) = serde_json::from_str::<SpawnRecord>(value) {
            r.subagent.settled = true;
            if let Ok(v) = serde_json::to_string(&r) {
                *value = v;
            }
        }
        if let Err(e) = self.save() {
            self.subagents.errors.push(e.to_string());
        }
    }

    /// A soft notice, never a refusal, above `soft_children` per parent.
    fn children_notice(&mut self, parent: &AgentId, contract: &str) -> Option<String> {
        let soft = self.limits().soft_children as usize;
        let count = self
            .spawn_records(contract)
            .iter()
            .filter(|(_, f)| f.parent == parent.as_str())
            .count();
        if count <= soft || !self.subagents.noticed.insert(parent.clone()) {
            return None;
        }
        let text = format!(
            "`{parent}` has {count} subagents, above the soft line of {soft} for this machine's \
             parallelism; each one takes a slot other work could use"
        );
        self.subagents.errors.push(text.clone());
        Some(text)
    }

    /// Start (or continue) a child's worker on its own task.
    fn start_child(&mut self, id: &AgentId) -> Result<(), String> {
        let (facts, task, workspace, fence, injector, cancel, contract) = {
            let Some(c) = self.subagents.live.get(id) else {
                return Err(format!("subagent `{id}` is not live"));
            };
            (
                c.facts.clone(),
                c.task.clone(),
                c.workspace.clone(),
                c.fence.clone(),
                c.injector.clone(),
                c.cancel.clone(),
                c.contract.clone(),
            )
        };
        let Some(route) = self.route(&facts.route) else {
            return Err(format!("route `{}` is not configured", facts.route));
        };
        let slices = self
            .record
            .contract(&contract)
            .map(|c| c.slices.clone())
            .unwrap_or_default();
        let brief = WorkerBrief {
            task,
            worker: id.clone(),
            instruction: child_instruction(id, &facts),
            workspace_root: workspace,
            model: route.model().to_owned(),
            slices,
            skills: Vec::new(),
            step_budget: self.config.step_budget,
            context: route.context.clone(),
            // A read-only child has no scope: its fence refuses every write,
            // so the refusal a model sees is the fence's (decision 0067).
            write_scope: (!facts.read_only)
                .then(|| facts.writes.iter().map(PathBuf::from).collect()),
        };
        let route_env = route.env.clone();
        let attempt_env = self.attempt_envs.clone().map(|envs| (envs, route.clone()));
        let path = self.files.worker_transcript(id.as_str());
        let parent_call = facts.parent_call.map(CallId::new);
        let tx = self.tx.clone();
        let board = self.frugal.board.clone();
        let graph = self.s.graph.clone();
        let child = id.clone();
        let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
        if let Some(c) = self.subagents.live.get_mut(id) {
            c.running = true;
            c.ended = Some(ended_rx);
        }
        // Local-only diagnostics (decision 0069), after this function's one
        // early failure (the route lookup), as `start_attempt` records it.
        let (provider, model) = facts
            .route
            .split_once(':')
            .unwrap_or((facts.route.as_str(), ""));
        kgraf_diag::record(kgraf_diag::DiagEvent::worker_spawned(provider, model));
        let panic_brief = brief.clone();
        let panic_child = child.clone();
        // Inner task per the panic-supervisor pattern used at the attempt and
        // verifier spawns: a panic here must still send `SubagentMsg::Done`,
        // or the parent's flight (and the process-wide admission slot it
        // holds) never releases.
        let subagent_task = tokio::spawn(async move {
            let built = match attempt_env {
                None => Ok(AttemptEnv {
                    env: route_env,
                    closer: None,
                }),
                Some((envs, route)) => envs
                    .build(&route, &brief.workspace_root, &brief.worker, &brief.task)
                    .await
                    .map_err(|e| format!("the subagent's environment could not be built: {e}")),
            };
            let (result, env_failure) = match built {
                Ok(AttemptEnv { env, closer }) => {
                    // The one attempt runner: a child's inbox is injected at
                    // spawn and its deliveries are marked, as a worker's.
                    let r = run_attempt(
                        env,
                        brief.clone(),
                        path,
                        cancel,
                        board,
                        fence,
                        injector,
                        graph,
                        parent_call,
                    )
                    .await
                    .map(|(result, _peak)| result);
                    if let Some(closer) = closer {
                        closer.close().await;
                    }
                    match r {
                        Ok(r) => (r, None),
                        Err(e) => (failed(&brief, e.clone()), Some(e)),
                    }
                }
                Err(e) => (failed(&brief, e.clone()), Some(e)),
            };
            (result, env_failure)
        });
        tokio::spawn(async move {
            // Dropped when this task ends, by any path.
            let _ended = ended_tx;
            let (result, env_failure) = match subagent_task.await {
                Ok(v) => v,
                Err(e) => {
                    let msg = if e.is_panic() {
                        format!("the subagent panicked: {e}")
                    } else {
                        format!("the subagent's task ended abnormally: {e}")
                    };
                    (failed(&panic_brief, msg.clone()), Some(msg))
                }
            };
            let _ = tx
                .send(LoopMsg::Subagent(Box::new(SubagentMsg::Done(Box::new(
                    ChildDone {
                        child: panic_child,
                        result,
                        env_failure,
                        recovered: None,
                    },
                )))))
                .await;
        });
        Ok(())
    }

    /// Adopted children: take a slot and continue them.
    async fn restart_children(&mut self) -> Result<(), OrchestratorError> {
        let waiting: Vec<AgentId> = self.subagents.restart.iter().cloned().collect();
        for id in waiting {
            let Some((read_only, parent_ws, contract, parent)) =
                self.subagents.live.get(&id).map(|c| {
                    (
                        c.facts.read_only,
                        c.parent_workspace.clone(),
                        c.contract.clone(),
                        AgentId::new(c.facts.parent.clone()),
                    )
                })
            else {
                self.subagents.restart.remove(&id);
                continue;
            };
            // While the run stops, only a child whose parent already finished
            // is continued: that parent is held until the child settles, and
            // the run cannot end before it. A running parent's cancel ends
            // its waiting children resumably (`hold_for_subagents`).
            if self.stopping && !self.subagents.parked.contains_key(&parent) {
                continue;
            }
            let now = self.now();
            if self
                .subagents
                .live
                .get(&id)
                .is_some_and(|c| c.retry_at_ms > now)
            {
                continue;
            }
            // Admitted before the kill: pre-flight admission (0017) never
            // stops what already runs, and waiting here could wait on the
            // very slot its parked parent holds. Counted when a slot is free;
            // run uncounted, with a notice, when none is.
            let tried = {
                let mut gate = self.s.admission.gate();
                if read_only {
                    gate.try_admit_agents(1)
                } else {
                    gate.try_admit(1)
                }
            };
            let admitted = match tried {
                Ok(a) => Some(a),
                Err(refusal) => {
                    let text = format!(
                        "subagent `{id}` continues over the limit ({refusal}): it was admitted \
                         before the process stopped"
                    );
                    let first = self
                        .subagents
                        .live
                        .get_mut(&id)
                        .is_some_and(|c| !std::mem::replace(&mut c.over_limit_notified, true));
                    if first {
                        self.notify_parent(&parent, &id, "over-limit", &text);
                    }
                    None
                }
            };
            let release = |me: &mut Self, admitted: Option<Admitted>| {
                if let Some(a) = admitted {
                    me.s.admission.gate().release(a);
                }
            };
            let estimate = self
                .in_flight
                .get(&contract)
                .map(|f| f.estimate)
                .unwrap_or(Money::ZERO);
            let task = match self.record.contract(&contract) {
                Some(c) => c.task.clone(),
                None => {
                    release(self, admitted);
                    continue;
                }
            };
            let run = self.run_id();
            let reservation =
                self.s
                    .admission
                    .budget()
                    .reserve(&run, task.as_str(), estimate, now, true);
            let workspace = if read_only {
                Ok(parent_ws.clone())
            } else {
                self.s.workspaces.reopen_child(&parent_ws, &id).await
            };
            let workspace = match workspace {
                Ok(w) => w,
                Err(PrepareFailure::Busy(why)) => {
                    release(self, admitted);
                    self.s.admission.budget().release(reservation);
                    let tries = match self.subagents.live.get_mut(&id) {
                        Some(c) => {
                            c.busy_tries += 1;
                            let base = (self.config.tick.as_millis() as i64).max(1);
                            let shift = c.busy_tries.min(16);
                            c.retry_at_ms = now + (base << shift).min(MAX_BUSY_BACKOFF_MS);
                            c.busy_tries
                        }
                        None => continue,
                    };
                    if tries == 1 {
                        let text = format!(
                            "subagent `{id}` cannot continue yet: its worktree is in use \
                             ({why}); tried again with a growing wait, {MAX_BUSY_TRIES} times \
                             at most"
                        );
                        self.notify_parent(&parent, &id, "busy", &text);
                    }
                    if tries >= MAX_BUSY_TRIES {
                        self.subagents.restart.remove(&id);
                        let mut result = self.unstarted_result(&id);
                        let error =
                            format!("its worktree stayed in use through {tries} tries: {why}");
                        result.outcome = WorkerOutcome::Failed {
                            error: error.clone(),
                        };
                        self.settle_child(ChildDone {
                            child: id.clone(),
                            result,
                            env_failure: Some(error),
                            recovered: None,
                        })
                        .await?;
                    }
                    continue;
                }
                Err(e) => {
                    release(self, admitted);
                    self.s.admission.budget().release(reservation);
                    self.subagents.restart.remove(&id);
                    // A kill after the merge removed the child's worktree and
                    // before its record was settled: the files are in the
                    // parent's worktree, and the report must say so.
                    if !read_only {
                        let writes = self
                            .subagents
                            .live
                            .get(&id)
                            .map(|c| c.facts.writes.clone())
                            .unwrap_or_default();
                        match self
                            .s
                            .workspaces
                            .recover_child_merge(&parent_ws, &id, &writes)
                            .await
                        {
                            Ok(Some(merge)) => {
                                let mut result = self.unstarted_result(&id);
                                result.outcome = WorkerOutcome::Completed;
                                result.answer = "(merged before the process stopped; its \
                                                 answer is in its transcript)"
                                    .into();
                                self.settle_child(ChildDone {
                                    child: id.clone(),
                                    result,
                                    env_failure: None,
                                    recovered: Some(merge),
                                })
                                .await?;
                                continue;
                            }
                            Ok(None) => {}
                            Err(detail) => self.subagents.errors.push(format!(
                                "the recorded result of subagent `{id}` could not be merged \
                                 again: {detail}"
                            )),
                        }
                    }
                    let mut result = self.unstarted_result(&id);
                    let error = format!("its worktree could not be reopened: {e}");
                    result.outcome = WorkerOutcome::Failed {
                        error: error.clone(),
                    };
                    self.settle_child(ChildDone {
                        child: id.clone(),
                        result,
                        env_failure: Some(error),
                        recovered: None,
                    })
                    .await?;
                    continue;
                }
            };
            self.subagents.restart.remove(&id);
            let own_fence = match self.subagents.live.get_mut(&id) {
                Some(c) => {
                    c.admitted = admitted;
                    c.reservation = Some(reservation);
                    c.workspace = workspace.clone();
                    c.fence.clone()
                }
                None => continue,
            };
            // Its own children fence it before it can write.
            self.adopt_children_of(&contract, &id, &workspace, &own_fence);
            if let Err(e) = self.start_child(&id) {
                let mut result = self.unstarted_result(&id);
                result.outcome = WorkerOutcome::Failed { error: e.clone() };
                self.settle_child(ChildDone {
                    child: id.clone(),
                    result,
                    env_failure: Some(e),
                    recovered: None,
                })
                .await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- finish

    async fn on_child_done(&mut self, d: ChildDone) -> Result<(), OrchestratorError> {
        let id = d.child.clone();
        if matches!(d.result.outcome, WorkerOutcome::Cancelled)
            && self
                .subagents
                .live
                .get(&id)
                .is_some_and(|c| c.user_stopped && c.ended_parent.is_none())
        {
            return self.pause_child(d).await;
        }
        let ended_parent = match self.subagents.live.get_mut(&id) {
            Some(c) => {
                c.running = false;
                c.ended_parent
            }
            None => return Ok(()),
        };
        if !self.children_of(&id).is_empty() {
            if !is_finished_work(&d.result.outcome) || d.env_failure.is_some() {
                let resumable = ended_parent == Some(true)
                    || matches!(d.result.outcome, WorkerOutcome::Cancelled)
                        && ended_parent.is_none();
                self.end_children(&id, resumable).await?;
            }
            if !self.children_of(&id).is_empty() {
                self.subagents.parked.insert(id, Parked::Child(Box::new(d)));
                return Ok(());
            }
        }
        self.settle_child(d).await
    }

    /// A child the user stopped directly (decision 0067, point 2): its spend
    /// is settled and its slot released, but it stays live and not running,
    /// with its record, row and fence kept, so its parent waits for it (the
    /// parent's finish is parked while a child is live) and `resume_session`
    /// continues it from its transcript. Its own children stop resumably.
    async fn pause_child(&mut self, d: ChildDone) -> Result<(), OrchestratorError> {
        let id = d.child.clone();
        let (admitted, reservation, task) = match self.subagents.live.get_mut(&id) {
            Some(c) => {
                c.running = false;
                (c.admitted.take(), c.reservation.take(), c.task.clone())
            }
            None => return Ok(()),
        };
        if let Some(a) = admitted {
            self.s.admission.gate().release(a);
        }
        self.charge_child(&id, &task, reservation, &d.result.calls)
            .await;
        Box::pin(self.end_children(&id, true)).await?;
        // A `resume_session` that arrived while this cancel was still in
        // flight (see `resume_child`) is applied now that the pause it was
        // waiting for is confirmed: restart at once instead of leaving the
        // child paused for a resume already asked for.
        if let Some(c) = self.subagents.live.get_mut(&id)
            && c.resume_after_stop
        {
            c.resume_after_stop = false;
            c.user_stopped = false;
            c.over_limit_notified = false;
            self.subagents.restart.insert(id);
        }
        Ok(())
    }

    /// The run loop failed: cancel every running child. A running child
    /// keeps its slot and hold until its task has ended, then returns the slot
    /// and settles the hold against its exact spend read from the ledger (its
    /// hold, an upper bound, when that read fails or a call's cost is unknown;
    /// a child with no hold still has its spend recorded), stamped with the
    /// time the child ended. Releasing at once would free the slot for a new
    /// worker while the child still writes, and would drop its spend from the
    /// hourly total. A child that ignores the cancel is waited for at most
    /// [`RELEASE_WAIT_BOUND`](super::RELEASE_WAIT_BOUND); then the slot and hold are released with a
    /// notice, since a slot held forever is capacity lost for the process. Its
    /// spawn record stays unsettled, so a resume adopts it again; its fence on
    /// the parent is lifted with the parent's run.
    pub(super) fn release_subagents_on_error(&mut self) {
        let handle = tokio::runtime::Handle::try_current().ok();
        let run = self.run_id();
        for (child, mut c) in std::mem::take(&mut self.subagents.live) {
            let waiting = if c.running {
                c.cancel.notify_one();
                c.ended.take().filter(|_| handle.is_some())
            } else {
                None
            };
            let admitted = c.admitted.take();
            let reservation = c.reservation.take();
            let admission = self.s.admission.clone();
            let task = c.task.as_str().to_owned();
            let run = run.clone();
            let (ledger, prices, clock) = (
                self.s.ledger.clone(),
                self.shared_prices.clone(),
                self.s.clock.clone(),
            );
            let charged = self.seeded.clone();
            let agent = child.clone();
            match (waiting, &handle) {
                (Some(rx), Some(handle)) => {
                    handle.spawn(async move {
                        wait_for_end(rx, &format!("subagent `{agent}`")).await;
                        if let Some(a) = admitted {
                            admission.gate().release(a);
                        }
                        let held = reservation.as_ref().map(Reservation::amount);
                        let spend = exact_agent_spend(
                            &ledger,
                            &prices,
                            &task,
                            &agent,
                            &charged,
                            held.unwrap_or(Money::ZERO),
                        );
                        let at = clock.now_ms();
                        let mut budget = admission.budget();
                        match reservation {
                            Some(r) => {
                                let _ = budget.settle(r, &run, &task, spend, at);
                            }
                            None => budget.record_spend(&run, &task, spend, at),
                        }
                    });
                }
                _ => {
                    if let Some(a) = admitted {
                        admission.gate().release(a);
                    }
                    if let Some(r) = reservation {
                        admission.budget().release(r);
                    }
                }
            }
            drop(c.parent_guard.take());
        }
        self.subagents.restart.clear();
        self.subagents.parked.clear();
    }

    // ------------------------------------------------------------- sessions

    /// `Some(running)` for a live child (running, or adopted, paused or
    /// waiting for a slot), `None` for any other id.
    pub(super) fn child_running(&self, id: &AgentId) -> Option<bool> {
        self.subagents.live.get(id).map(|c| c.running)
    }

    /// A live child's injector, once its worker exists.
    pub(super) fn child_injector(&self, id: &AgentId) -> Option<Injector> {
        self.subagents
            .live
            .get(id)
            .and_then(|c| c.injector.get().cloned())
    }

    /// The contract a live child works under (its root worker's).
    pub(super) fn child_contract(&self, id: &AgentId) -> Option<String> {
        self.subagents.live.get(id).map(|c| c.contract.clone())
    }

    /// Pairs of live children of one parent: siblings, connected by rule.
    pub(super) fn child_sibling_pairs(&self) -> Vec<(AgentId, AgentId)> {
        let ids: Vec<(&AgentId, &str)> = self
            .subagents
            .live
            .iter()
            .map(|(id, c)| (id, c.facts.parent.as_str()))
            .collect();
        let mut pairs = Vec::new();
        for (i, (a, pa)) in ids.iter().enumerate() {
            for (b, pb) in &ids[i + 1..] {
                if pa == pb {
                    pairs.push(((*a).clone(), (*b).clone()));
                }
            }
        }
        pairs
    }

    /// `stop_session` on a live child: cancel it so that its `Done` pauses
    /// it. `None` when `id` is not a live child.
    pub(super) fn stop_child(&mut self, id: &AgentId) -> Option<Result<(), SessionError>> {
        let c = self.subagents.live.get_mut(id)?;
        if !c.running {
            return Some(Err(SessionError::Refused(format!(
                "subagent `{id}` is not running"
            ))));
        }
        c.user_stopped = true;
        // A stop supersedes any resume requested while an earlier stop was
        // still in flight (`resume_after_stop`): the user's latest word is
        // stop, so the pending resume must not fire once this one confirms.
        c.resume_after_stop = false;
        c.cancel.notify_one();
        Some(Ok(()))
    }

    /// `resume_session` on a live child: a paused one takes a slot again and
    /// continues from its transcript. `None` when `id` is not a live child.
    ///
    /// `c.running` still `true` means the stop this resumes from was
    /// requested (`cancel.notify_one()`) but its `Done` has not reached
    /// `on_child_done` yet: the cancel is in flight, not yet confirmed. This
    /// resume is deferred (`resume_after_stop`) rather than clearing
    /// `user_stopped` now, because `on_child_done`'s own check
    /// (`c.user_stopped && ...`) is how it tells a requested pause from a
    /// genuine end; clearing it here first would make that soon-arriving
    /// `Done` look unrequested, retiring the child instead of pausing and
    /// restarting it. `pause_child` applies the deferred resume once it runs.
    pub(super) fn resume_child(&mut self, id: &AgentId) -> Option<()> {
        let c = self.subagents.live.get_mut(id)?;
        if c.user_stopped {
            if c.running {
                c.resume_after_stop = true;
            } else {
                c.user_stopped = false;
                c.over_limit_notified = false;
                self.subagents.restart.insert(id.clone());
            }
        }
        Some(())
    }

    async fn settle_child(&mut self, d: ChildDone) -> Result<(), OrchestratorError> {
        let id = d.child.clone();
        let Some(mut c) = self.subagents.live.remove(&id) else {
            return Ok(());
        };
        self.subagents.restart.remove(&id);
        let parent = AgentId::new(c.facts.parent.clone());
        let now = self.now();
        if let Some(a) = c.admitted.take() {
            self.s.admission.gate().release(a);
        }
        // From here to `release_parked` nothing returns early on an error:
        // the child is already out of `live`, so an early return would skip
        // its merge, its report and its settled mark, and leave its parent's
        // finish parked for good. A failure is kept in `errors` and traced on
        // the next tick instead.
        let spend = self
            .charge_child(&id, &c.task, c.reservation.take(), &d.result.calls)
            .await;
        let model = self
            .route(&c.facts.route)
            .map(|r| r.model().to_owned())
            .unwrap_or_default();
        let traced = self
            .trace(TraceEvent::WorkerFinished {
                at_ms: now,
                contract: c.contract.clone(),
                worker: id.to_string(),
                route: c.facts.route.clone(),
                model,
                outcome: outcome_name(&d.result.outcome).to_owned(),
                steps: d.result.steps,
                peak_context_tokens: 0,
                projected_tokens: 0,
                stop_tokens: 0,
                window_tokens: 0,
                sizing_miss: false,
                calls: d.result.calls.iter().map(|c| c.to_string()).collect(),
                spend_micros: spend.micros(),
            })
            .await;
        if let Err(e) = traced {
            self.subagents.errors.push(format!(
                "the end of subagent `{id}` could not be traced: {e}"
            ));
        }
        let outcome = d.result.outcome.clone();
        if c.ended_parent == Some(true) && matches!(outcome, WorkerOutcome::Cancelled) {
            // Stopped with its parent: the record and the row stay open.
            let old_guard = c.parent_guard.take();
            if let Some(p) = self.parent_view(&parent) {
                // The parent is running again already: a quick resume beat
                // this cancel, and its adoption skipped this child because
                // it was still live. Adopt it now, fenced on the parent's
                // current fence before the old guard lifts, so the parent
                // never writes its paths in between.
                c.parent_guard = (!c.facts.read_only).then(|| p.fence.fence(&c.facts.writes));
                drop(old_guard);
                c.parent_workspace = p.workspace.clone();
                c.workspace = p.workspace;
                c.cancel = Arc::new(Notify::new());
                c.injector = Arc::new(OnceLock::new());
                c.running = false;
                c.ended_parent = None;
                c.busy_tries = 0;
                c.retry_at_ms = 0;
                c.over_limit_notified = false;
                c.user_stopped = false;
                c.resume_after_stop = false;
                self.subagents.live.insert(id.clone(), c);
                self.subagents.restart.insert(id);
                return Ok(());
            }
            // Otherwise the parent's next start adopts it.
            drop(old_guard);
            self.release_parked(&parent);
            return Ok(());
        }
        let finished = is_finished_work(&outcome) && d.env_failure.is_none();
        let merge = if let Some(merge) = d.recovered.clone() {
            merge
        } else if c.facts.read_only {
            ChildMerge::ReadOnly
        } else if !finished {
            ChildMerge::NotMerged {
                reason: match (&d.env_failure, &outcome) {
                    (Some(e), _) => e.clone(),
                    (None, WorkerOutcome::Failed { error }) => format!("it failed: {error}"),
                    (None, o) => format!("it ended {}", outcome_name(o)),
                },
            }
        } else {
            match self
                .s
                .workspaces
                .merge_child(&c.parent_workspace, &c.workspace, &c.facts.writes)
                .await
            {
                Ok(merge) => merge,
                Err(detail) => ChildMerge::Failed {
                    detail,
                    applied: Vec::new(),
                },
            }
        };
        if let ChildMerge::Merged {
            warning: Some(w), ..
        }
        | ChildMerge::Empty {
            warning: Some(w), ..
        } = &merge
        {
            self.subagents.errors.push(w.clone());
        }
        // The fence lifts only now: the child's work is in the parent's
        // worktree (or reported as not), and the parent may write again.
        drop(c.parent_guard.take());
        let landed: Vec<PathBuf> = match &merge {
            ChildMerge::Merged { .. } => c.facts.writes.iter().map(PathBuf::from).collect(),
            ChildMerge::Failed { applied, .. } => applied.iter().map(PathBuf::from).collect(),
            _ => Vec::new(),
        };
        if !landed.is_empty() {
            self.subagents
                .merged_paths
                .entry(parent.clone())
                .or_default()
                .extend(landed);
        }
        let report = SubagentReport {
            subagent: id.to_string(),
            parent: parent.to_string(),
            outcome: outcome_name(&outcome).to_owned(),
            read_only: c.facts.read_only,
            merge: merge.clone(),
            answer: cut(&d.result.answer, MAX_REPORT_ANSWER_CHARS),
        };
        // The report first, then the settled mark, then the row's end: a
        // crash in between re-adopts the child, whose finished transcript
        // costs no round, and the report is not written twice.
        self.push_report(&parent, &id, &report);
        self.settle_record(&c.contract, &id);
        if let Err(e) = self.retire_session(&id, session_outcome(&outcome)) {
            self.subagents.errors.push(e.to_string());
        }
        let status = serde_json::to_value(&merge)
            .ok()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_owned))
            .unwrap_or_default();
        let traced = self
            .trace(TraceEvent::Integration {
                at_ms: now,
                contract: c.contract.clone(),
                outcome: format!("subagent_{status}"),
                detail: format!("{id} into {parent}"),
            })
            .await;
        if let Err(e) = traced {
            self.subagents.errors.push(format!(
                "the merge of subagent `{id}` could not be traced: {e}"
            ));
        }
        if let Err(e) = self.deliver_wakes().await {
            self.subagents.errors.push(format!(
                "the report of subagent `{id}` could not be delivered: {e}"
            ));
        }
        self.release_parked(&parent);
        Ok(())
    }

    /// Settle a child's hold against what its calls cost and record the
    /// spend. Never fails: a ledger read that fails charges the hold
    /// instead, and an overrun that cannot be traced is kept in `errors`.
    /// The calls are then marked charged (`seeded`), so a paused or
    /// re-adopted child, whose next result lists these calls again (a
    /// resumed worker carries its earlier calls), is not charged for them a
    /// second time.
    async fn charge_child(
        &mut self,
        id: &AgentId,
        task: &TaskId,
        reservation: Option<Reservation>,
        calls: &[CallId],
    ) -> Money {
        let held = reservation
            .as_ref()
            .map(Reservation::amount)
            .unwrap_or(Money::ZERO);
        let mut read_lost = false;
        let spend = match self.spend_of(calls, held) {
            Ok((s, _)) => s,
            Err(e) => {
                if held == Money::ZERO {
                    // With no hold to stand in for it, nothing can be charged
                    // now. The child is settled and will not report again, so
                    // its calls are kept and read again at every tick until the
                    // charge lands; the calls stay unmarked until then.
                    read_lost = true;
                    self.subagents.uncharged.push(Uncharged {
                        child: id.clone(),
                        task: task.clone(),
                        calls: calls.to_vec(),
                    });
                    self.subagents.errors.push(format!(
                        "the spend of subagent `{id}` could not be read and it had no hold, so \
                         none is charged yet; it is read again at every tick: {e}"
                    ));
                } else {
                    self.subagents.errors.push(format!(
                        "the spend of subagent `{id}` could not be read; its hold of {held} is \
                         charged instead: {e}"
                    ));
                }
                held
            }
        };
        let (now, run) = (self.now(), self.run_id());
        let overrun = {
            let mut budget = self.s.admission.budget();
            match reservation {
                Some(r) => budget.settle(r, &run, task.as_str(), spend, now),
                None => {
                    budget.record_spend(&run, task.as_str(), spend, now);
                    None
                }
            }
        };
        if !read_lost {
            Arc::make_mut(&mut self.seeded).extend(calls.iter().cloned());
        }
        if let Some(o) = overrun
            && let Err(e) = self.on_overrun(o).await
        {
            self.subagents.errors.push(format!(
                "the overrun of subagent `{id}` could not be recorded: {e}"
            ));
        }
        spend
    }

    /// Read again the spend of settled children whose first read failed, and
    /// charge what is found. A child stays listed until its read works.
    async fn charge_unread_children(&mut self) {
        if self.subagents.uncharged.is_empty() {
            return;
        }
        for pending in std::mem::take(&mut self.subagents.uncharged) {
            match self.spend_of(&pending.calls, Money::ZERO) {
                Ok((spend, _)) => {
                    let (now, run) = (self.now(), self.run_id());
                    self.s
                        .admission
                        .budget()
                        .record_spend(&run, pending.task.as_str(), spend, now);
                    Arc::make_mut(&mut self.seeded).extend(pending.calls.iter().cloned());
                    self.subagents.errors.push(format!(
                        "the spend of subagent `{}` was read at last and charged: {spend}",
                        pending.child
                    ));
                }
                Err(_) => self.subagents.uncharged.push(pending),
            }
        }
    }

    fn push_report(&mut self, parent: &AgentId, child: &AgentId, report: &SubagentReport) {
        let id = format!("{SUBAGENT_REPORT_PREFIX}{child}");
        self.push_to_parent(parent, child, id, InboxKind::Message, report.render());
    }

    /// A notice about `child` to its parent's inbox (and transcript): the
    /// place the parent and the session view read, unlike the trace. Each
    /// notice has its own id (the time and a counter): the same `what` can
    /// happen again (a child resumed over the limit twice), and a reused id
    /// would be refused by the inbox store and by the runtime as a duplicate,
    /// so the second notice would never arrive.
    fn notify_parent(&mut self, parent: &AgentId, child: &AgentId, what: &str, text: &str) {
        let (now, n) = (self.now(), self.sessions_rt.next_id());
        let id = format!("{SUBAGENT_NOTICE_PREFIX}{child}:{what}:{now}-{n}");
        self.push_to_parent(parent, child, id, InboxKind::Notice, text.to_owned());
        self.subagents.errors.push(text.to_owned());
    }

    fn push_to_parent(
        &mut self,
        parent: &AgentId,
        child: &AgentId,
        id: String,
        kind: InboxKind,
        text: String,
    ) {
        let now = self.now();
        let item = InboxItem {
            id: id.clone(),
            session: parent.clone(),
            source: InboxSource::Agent { id: child.clone() },
            kind,
            mode: InjectMode::AtBoundary,
            // Raw: the inbox store takes it in (decision 0071).
            text,
            received_ms: now,
            not_before_ms: now,
            delivered: None,
        };
        let store = self.sessions();
        let present = blocking(|| store.undelivered(parent))
            .map(|v| v.iter().any(|i| i.id == id))
            .unwrap_or(false);
        if !present && let Err(e) = blocking(|| store.push_inbox(&item)) {
            self.subagents.errors.push(format!(
                "the report of `{child}` could not be written to the inbox of `{parent}`: {e}"
            ));
        }
        let injection = Injection {
            id: item.id.clone(),
            source: item.source,
            kind: item.kind,
            mode: item.mode,
            // The model-facing copy of the raw report (decision 0071).
            text: kgraf_secrets::scrub_model_facing(&item.text).into_owned(),
        };
        self.subagents.wakes.insert(
            id,
            Wake {
                parent: parent.clone(),
                injection,
            },
        );
    }

    /// Release a parked finish once the finisher has no live children.
    fn release_parked(&mut self, parent: &AgentId) {
        if !self.children_of(parent).is_empty() {
            return;
        }
        match self.subagents.parked.remove(parent) {
            Some(Parked::Attempt(f)) => {
                let f = self.with_merged_paths(*f);
                self.backlog.push_back(LoopMsg::Finished(Box::new(f)));
            }
            Some(Parked::Child(d)) => {
                self.backlog
                    .push_back(LoopMsg::Subagent(Box::new(SubagentMsg::Done(d))));
            }
            None => {}
        }
    }

    fn injector_state(&self, parent: &AgentId) -> Slot {
        if let Some(c) = self.subagents.live.get(parent) {
            return match c.injector.get() {
                Some(i) => Slot::Ready(i.clone()),
                None if c.running => Slot::NotYet,
                None => Slot::Gone,
            };
        }
        for f in self.in_flight.values() {
            if let Some(slot) = f.injectors.get(parent) {
                return match slot.get() {
                    Some(i) => Slot::Ready(i.clone()),
                    None if f.cancels.contains_key(parent) => Slot::NotYet,
                    None => Slot::Gone,
                };
            }
        }
        Slot::Gone
    }

    /// Hand each pending report to its parent's worker. A parent not running
    /// in this process keeps the report in its inbox; adoption offers it
    /// again when the parent is dispatched.
    async fn deliver_wakes(&mut self) -> Result<(), OrchestratorError> {
        let ids: Vec<String> = self.subagents.wakes.keys().cloned().collect();
        for id in ids {
            let Some(parent) = self.subagents.wakes.get(&id).map(|w| w.parent.clone()) else {
                continue;
            };
            let action = match self.injector_state(&parent) {
                Slot::NotYet => continue,
                Slot::Gone => {
                    self.subagents.wakes.remove(&id);
                    "report_held"
                }
                Slot::Ready(injector) => {
                    let Some(w) = self.subagents.wakes.remove(&id) else {
                        continue;
                    };
                    match injector.inject(w.injection) {
                        Ok(()) => "report_delivered",
                        // Already in the parent's transcript: delivered once.
                        Err(InjectError::Duplicate { .. }) => continue,
                        Err(InjectError::Finished { .. }) => "report_after_parent_finished",
                        Err(InjectError::NotBuilt) => "report_not_injectable",
                    }
                }
            };
            let at = self.now();
            self.trace(TraceEvent::Message {
                at_ms: at,
                id: id.clone(),
                action: action.into(),
                from: id.trim_start_matches(SUBAGENT_REPORT_PREFIX).to_owned(),
                to: vec![parent.to_string()],
                detail: match action {
                    "report_held" => "its parent is not running here; the report stays in its inbox",
                    "report_after_parent_finished" => {
                        "its parent answered first; the report stays in its inbox and the run record"
                    }
                    _ => "",
                }
                .into(),
            })
            .await?;
        }
        Ok(())
    }
}

fn failed(brief: &WorkerBrief, error: String) -> WorkerResult {
    WorkerResult::bare(
        brief.task.clone(),
        brief.worker.clone(),
        WorkerOutcome::Failed { error },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spawn_record_is_told_apart_from_a_worktree_handle() {
        // The map also holds `{gitlink, base}` handles of ordinary workers.
        let handle = r#"{"gitlink":"gitdir: x","base":"abc"}"#;
        assert!(serde_json::from_str::<SpawnRecord>(handle).is_err());
        let facts = SpawnFacts {
            parent: "w".into(),
            task: "t".into(),
            route: "alpha".into(),
            writes: vec!["src/a.rs".into()],
            read_only: false,
            depth: 1,
            lineage: vec![("w".into(), "x".into())],
            parent_call: None,
            settled: false,
            preparing: false,
        };
        let text = serde_json::to_string(&SpawnRecord { subagent: facts }).expect("json");
        assert!(serde_json::from_str::<SpawnRecord>(&text).is_ok());
    }

    /// A failed run settles a running child against its exact ledger spend, not
    /// its hold: only the child's own calls, not those already charged, priced
    /// from the table; a hold stands in only for a cost that is unknown.
    #[test]
    fn a_released_childs_spend_is_read_from_the_ledger_not_taken_from_its_hold() {
        use kgraf_core::{Money, PriceTable, Protocol, TokenUsage};
        let mut prices = PriceTable::new();
        prices.set(
            "m",
            kgraf_core::ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        let ledger = kgraf_provider::Ledger::open_in_memory().expect("ledger");
        let mut ids = Vec::new();
        for (agent, model, input) in [
            ("c1", "m", 1_000u64),
            ("c1", "m", 2_000),
            ("other", "m", 4_000),
            ("c1", "unpriced", 8_000),
        ] {
            let token = ledger
                .begin("probe", Protocol::Additive, model, agent, "run/t")
                .expect("begin");
            ids.push(token.id().clone());
            token
                .settle(
                    &TokenUsage {
                        uncached_input: input,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
        }
        let child = AgentId::new("c1");
        let cost = |input: u64| Money::from_micros(input as i64);
        let mut charged = BTreeSet::new();
        charged.insert(ids[0].clone());
        // A call on an unpriced model is unknown: the larger hold stands in.
        let big_hold = Money::from_micros(1_000_000);
        assert_eq!(
            exact_agent_spend(&ledger, &prices, "run/t", &child, &charged, big_hold),
            big_hold
        );
        // With a hold below the known spend, the exact figure wins; the charged
        // call and the other agent's call are not counted.
        assert_eq!(
            exact_agent_spend(&ledger, &prices, "run/t", &child, &charged, Money::ZERO),
            cost(2_000)
        );
    }

    #[test]
    fn a_long_answer_is_cut_on_a_character_boundary() {
        let text = "é".repeat(10);
        let cut = cut(&text, 4);
        assert!(cut.starts_with("éééé..."), "{cut}");
        assert_eq!(super::cut("short", 10), "short");
    }
}
