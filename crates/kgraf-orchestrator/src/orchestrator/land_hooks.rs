//! The run loop's side of landing (decision 0100, section 5): what the loop
//! records for a later land (start branch, program snapshot, root-gate
//! passes, interface stub and producer commits), the land itself when the
//! loop is live (the loop is then the one writer of the record and of
//! `kgraf/<run>`), and the run lease a land takes when no loop is live.
//!
//! A live land is split so the loop never waits on the merge-result gate:
//! `Control::Land` prepares it (git reads and the unsigned trial merge); the
//! gate runs as a background task that posts `Control::LandGated` back; that
//! message completes it (target re-read, signed commit, journal, ref move,
//! fold). Notes, answers, worker results and status are taken meanwhile. One
//! land waits at a time; a second is refused.
//!
//! A child module of `orchestrator.rs`, like `coord.rs`, so the loop carries
//! one-line hooks.

use super::*;
use crate::coordination::Integration;
use crate::gates::{self, GateRequest, GateVerdict, ROOT_GATE};
use crate::land_flow::{
    Evidence, LandContext, LandError, LandGit, LandOutcome, LandRequest, PendingLand, Step,
};
use crate::land_record::{LandBook, ProgramSnapshot, StartBranch};
use crate::run_state::VerifiedCommit;

type LandReply = oneshot::Sender<Result<LandOutcome, LandError>>;

/// What a run is given for landing: the repository side, the branch the run
/// starts from and the program snapshot taken then; and the land waiting
/// for its gate.
#[derive(Default)]
pub(crate) struct LandParts {
    pub(crate) lander: Option<Arc<dyn LandGit>>,
    pub(crate) start_branch: Option<StartBranch>,
    pub(crate) programs: Option<ProgramSnapshot>,
    pending: Option<LoopLandWait>,
}

/// The one land a live loop has in progress, waiting for its gate or its
/// move.
enum LoopLandWait {
    Gating {
        pending: Box<PendingLand>,
        reply: LandReply,
        task: tokio::task::JoinHandle<()>,
    },
    Moving {
        entry: Box<crate::land_record::LandingEntry>,
        notices: Vec<String>,
        reply: LandReply,
    },
}

impl LoopLandWait {
    fn target(&self) -> &str {
        match self {
            LoopLandWait::Gating { pending, .. } => &pending.target.name,
            LoopLandWait::Moving { entry, .. } => &entry.target,
        }
    }
}

impl LandParts {
    /// The loop is ending: stop a land's gate and answer its caller. A move
    /// already started finishes on its own; the journal keeps the land.
    pub(super) fn stop(&mut self) {
        match self.pending.take() {
            Some(LoopLandWait::Gating { reply, task, .. }) => {
                task.abort();
                let _ = reply.send(Err(LandError::Run(
                    "the run stopped before the land finished; nothing moved".into(),
                )));
            }
            Some(LoopLandWait::Moving { reply, .. }) => {
                let _ = reply.send(Err(LandError::Run(
                    "the run stopped while git moved the target; the land's journal keeps it \
                     open, so land again to finish it"
                        .into(),
                )));
            }
            None => {}
        }
    }

    /// Record the start branch and program snapshot in a fresh run's record.
    pub(super) fn seed(&self, record: &mut RunStateRecord) {
        if record.land.start_branch.is_none() {
            record.land.start_branch = self.start_branch.clone();
        }
        if record.land.programs.is_none() {
            record.land.programs = self.programs.clone();
        }
    }
}

impl Orchestrator {
    /// Land through this run's loop (`/land` while it runs): the repository
    /// side of a land, over the run's own branch.
    pub fn with_lander(mut self, lander: Arc<dyn LandGit>) -> Self {
        self.land.lander = Some(lander);
        self
    }

    /// The branch checked out when the run starts, recorded in a fresh run's
    /// record as its start branch (the default land target).
    pub fn with_start_branch(mut self, name: impl Into<String>, sha: impl Into<String>) -> Self {
        self.land.start_branch = Some(StartBranch {
            name: name.into(),
            sha: sha.into(),
        });
        self
    }

    /// The hooks and program settings when the run starts, recorded in a
    /// fresh run's record; a land compares against it.
    pub fn with_program_snapshot(mut self, snapshot: ProgramSnapshot) -> Self {
        self.land.programs = Some(snapshot);
        self
    }
}

impl OrchestratorHandle {
    /// Land the run through its live loop (the one land function,
    /// [`crate::land_flow::land`]). Only on the user's explicit yes.
    pub async fn land(&self, request: LandRequest) -> Result<LandOutcome, LandError> {
        let (reply, rx) = oneshot::channel();
        let gone = || LandError::Persist("the run's loop has ended".into());
        self.tx
            .send(LoopMsg::Control(Control::Land {
                request: Box::new(request),
                reply,
            }))
            .await
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    /// The user answered a land suggestion "Not now": recorded in the run
    /// record by the loop, so it survives a restart.
    pub async fn decline_land(&self, now_ms: i64) {
        let _ = self
            .tx
            .send(LoopMsg::Control(Control::LandDeclined { now_ms }))
            .await;
    }
}

/// The merge commit `contract`'s own verified work made on the run branch;
/// `None` when it made none (unversioned, empty, refused, or not recorded).
pub(super) fn merged_commit_of(
    integration: &BTreeMap<String, Integration>,
    contract: &str,
) -> Option<String> {
    match integration.get(contract) {
        Some(Integration::Merged { commit }) => Some(commit.clone()),
        _ => None,
    }
}

impl RunLoop {
    /// An interface's stub landed at `commit` (decision 0081).
    pub(super) fn note_stub_landed(
        &mut self,
        interface: &str,
        commit: &str,
        producer: Option<&str>,
    ) {
        self.record
            .land
            .note_stub_landed(interface, commit, producer);
    }

    /// `contract` verified: its own merge commit, when it made one, is the
    /// producer-verified commit of the interfaces it produces.
    pub(super) fn note_contract_verified(&mut self, contract: &str) {
        if let Some(commit) = merged_commit_of(&self.record.coordination.integration, contract) {
            self.record.land.note_contract_verified(contract, &commit);
        }
    }

    /// A gate's verdict: a root-gate pass on a pinned commit makes that
    /// commit landable, recorded even while covers are still open.
    pub(super) fn note_gate_verdict(
        &mut self,
        gate: &str,
        passed: bool,
        pinned: Option<&str>,
        at_ms: i64,
    ) {
        if passed
            && gate == ROOT_GATE
            && let Some(sha) = pinned
        {
            self.record.land.note_root_gate_pass(sha, at_ms);
        }
    }

    /// `Control::LandDeclined`: record the decline.
    pub(super) fn on_land_declined(&mut self, now_ms: i64) -> Result<(), OrchestratorError> {
        let verified = self.record.verified_contracts.len();
        let book = &mut self.record.land;
        book.land_declined_at = Some(now_ms);
        book.land_decline_count += 1;
        book.verified_at_decline = Some(verified);
        self.save()
    }

    /// `Control::Land`: prepare the land, then drive it: a gate or the
    /// target's move runs off the loop task and posts its result back.
    pub(super) async fn on_land(&mut self, request: LandRequest, reply: LandReply) {
        if let Some(waiting) = &self.land.pending {
            let target = waiting.target().to_owned();
            let _ = reply.send(Err(LandError::LandPending { target }));
            return;
        }
        let Some(lander) = self.land.lander.clone() else {
            let _ = reply.send(Err(LandError::Git(
                "this run has no repository to land from (its workspaces are not versioned)".into(),
            )));
            return;
        };
        let step = {
            let mut ctx = LoopLand { lp: self };
            crate::land_flow::prepare(lander.as_ref(), &mut ctx, &request)
        };
        self.drive_land(lander, step, reply);
    }

    /// Answer a finished step, or start its gate or its move in the
    /// background; the land stays pending (one at a time) until it posts back.
    fn drive_land(
        &mut self,
        lander: Arc<dyn LandGit>,
        step: Result<Step, LandError>,
        reply: LandReply,
    ) {
        match step {
            Err(e) => self.answer_land(reply, Err(e)),
            Ok(Step::Done(outcome)) => self.answer_land(reply, Ok(outcome)),
            Ok(Step::Gate(pending)) => {
                let runner = self
                    .coord
                    .gate_runner
                    .clone()
                    .filter(|r| !r.commands().is_empty());
                let Some(runner) = runner else {
                    let step = {
                        let mut ctx = LoopLand { lp: self };
                        crate::land_flow::complete(
                            lander.as_ref(),
                            &mut ctx,
                            pending,
                            Evidence::Verdict(None),
                        )
                    };
                    return self.drive_land(lander, step, reply);
                };
                let gate = GateRequest {
                    gate: ROOT_GATE.into(),
                    covers: Vec::new(),
                    commands: runner.commands(),
                    pinned: Some(VerifiedCommit {
                        sha: pending.trial_commit.clone(),
                        verified_by: "land: merge result".into(),
                        verified_ms: self.now(),
                    }),
                };
                let tx = self.tx.clone();
                let task = tokio::spawn(async move {
                    let verdict = runner.run(gate).await;
                    let _ = tx
                        .send(LoopMsg::Control(Control::LandGated {
                            verdict: Box::new(verdict),
                        }))
                        .await;
                });
                self.land.pending = Some(LoopLandWait::Gating {
                    pending: Box::new(pending),
                    reply,
                    task,
                });
            }
            Ok(Step::Move { entry, notices }) => {
                // The user's own git, with their hooks: never on the loop task.
                let tx = self.tx.clone();
                let moving = entry.clone();
                let git = lander.clone();
                let programs = self.record.land.programs.clone();
                tokio::task::spawn_blocking(move || {
                    let moved =
                        crate::land_flow::move_phase(git.as_ref(), &moving, programs.as_ref());
                    let _ = tx.blocking_send(LoopMsg::Control(Control::LandMoved {
                        moved: Box::new(moved),
                    }));
                });
                self.land.pending = Some(LoopLandWait::Moving {
                    entry: Box::new(entry),
                    notices,
                    reply,
                });
            }
        }
    }

    /// `Control::LandGated`: the merge-result gate ended; complete the land.
    pub(super) fn on_land_gated(&mut self, verdict: Result<GateVerdict, String>) {
        let Some(LoopLandWait::Gating { pending, reply, .. }) = self.land.pending.take() else {
            return;
        };
        let Some(lander) = self.land.lander.clone() else {
            let _ = reply.send(Err(LandError::Git("the run's repository is gone".into())));
            return;
        };
        let step = {
            let mut ctx = LoopLand { lp: self };
            crate::land_flow::complete(
                lander.as_ref(),
                &mut ctx,
                *pending,
                Evidence::Verdict(Some(verdict)),
            )
        };
        self.drive_land(lander, step, reply);
    }

    /// `Control::LandMoved`: the target's move ended; record it and fold.
    pub(super) fn on_land_moved(&mut self, moved: crate::land_flow::Moved) {
        let Some(LoopLandWait::Moving {
            entry,
            notices,
            reply,
        }) = self.land.pending.take()
        else {
            return;
        };
        let Some(lander) = self.land.lander.clone() else {
            let _ = reply.send(Err(LandError::Git("the run's repository is gone".into())));
            return;
        };
        let outcome = {
            let mut ctx = LoopLand { lp: self };
            crate::land_flow::finish_phase(lander.as_ref(), &mut ctx, *entry, notices, moved)
        };
        self.answer_land(reply, outcome);
    }

    fn answer_land(&mut self, reply: LandReply, outcome: Result<LandOutcome, LandError>) {
        let detail = match &outcome {
            Ok(LandOutcome::Landed { entry, .. }) => format!(
                "landed {} into {} as {}",
                entry.candidate, entry.target, entry.merge_commit
            ),
            Ok(other) => format!("land did not move anything: {other:?}"),
            Err(e) => format!("land refused: {e}"),
        };
        let at = self.now();
        self.queue(TraceEvent::Notice {
            at_ms: at,
            contract: String::new(),
            worker: String::new(),
            detail: kgraf_secrets::scrub_persist_owned(detail),
        });
        let _ = reply.send(outcome);
    }
}

/// The live loop as a land's context.
struct LoopLand<'a> {
    lp: &'a mut RunLoop,
}

#[async_trait::async_trait]
impl LandContext for LoopLand<'_> {
    fn book(&mut self) -> &mut LandBook {
        &mut self.lp.record.land
    }

    fn run_id(&self) -> String {
        self.lp.record.run.to_string()
    }

    fn contracts(&self) -> Vec<String> {
        self.lp.record.verified_contracts.clone()
    }

    fn known_stubs(&self) -> BTreeSet<String> {
        self.lp.landed_stub_ids()
    }

    fn now_ms(&self) -> i64 {
        self.lp.now()
    }

    fn persist(&mut self) -> Result<(), String> {
        self.lp.save().map_err(|e| e.to_string())
    }

    fn loop_live(&self) -> bool {
        true
    }

    /// Not used: the loop runs the gate in the background (`on_land`).
    async fn run_merge_gate(&mut self, _commit: &str) -> Option<Result<GateVerdict, String>> {
        None
    }

    fn open_remediation(&mut self, commit: &str, verdict: GateVerdict) -> Option<String> {
        let run = self.lp.run_id();
        let max = gates::DEFAULT_MAX_REMEDIATIONS;
        let at = self.lp.now();
        let verified: BTreeSet<String> =
            self.lp.record.verified_contracts.iter().cloned().collect();
        let mut fix = gates::record_verdict(
            &mut self.lp.record.coordination.gates,
            ROOT_GATE,
            Ok(verdict),
            Some(commit.to_owned()),
            at,
            max,
            &run,
            &|c| verified.contains(c),
        )?;
        fix.gate_commands = self
            .lp
            .coord
            .gate_runner
            .as_ref()
            .map(|r| r.commands())
            .unwrap_or_default();
        gates::open_remediation(
            &mut self.lp.record.coordination.gates,
            ROOT_GATE,
            &fix.contract,
        );
        let id = fix.contract.clone();
        self.lp.record.open_contracts.push(fix);
        Some(id)
    }
}

/// The run's lease, held by a land that runs with no live loop, so no loop
/// starts on the run meanwhile. Renewed while a gate runs; released when
/// dropped.
pub struct LandLease {
    graph: crate::store::SharedGraph,
    holder: String,
    leases: Vec<kgraf_graph::WriteLease>,
    ttl_ms: i64,
}

impl LandLease {
    /// Push the lease's expiry out. `Ok(false)`: it lapsed or another holder
    /// took it; the land must not write the record.
    pub fn renew(&self, now_ms: i64) -> Result<bool, OrchestratorError> {
        let g = self.graph.lock().map_err(|_| StoreError::Poisoned)?;
        let reg = LeaseRegistry::new(&g).with_ttl_ms(self.ttl_ms);
        let mut held = !self.leases.is_empty();
        for l in &self.leases {
            held &= reg.renew(l, now_ms)?;
        }
        Ok(held)
    }
}

impl Drop for LandLease {
    fn drop(&mut self) {
        if let Ok(g) = self.graph.lock() {
            let _ = release_holder(&LeaseRegistry::new(&g), &self.holder);
        }
    }
}

/// How long a land's run lease lasts unrenewed. The land renews it while a
/// gate runs and before each save. A lease a dead process left is taken
/// over at once (its holder names the process).
pub const LAND_LEASE_TTL_MS: i64 = 10 * 60 * 1000;

/// Take run `run`'s lease for a land with no live loop. Refused with
/// [`OrchestratorError::RunBusy`] while a loop (or another land) holds it.
pub fn take_run_lease_for_land(
    graph: &crate::store::SharedGraph,
    run: &RunId,
    now_ms: i64,
) -> Result<LandLease, OrchestratorError> {
    take_run_lease_for_land_with_ttl(graph, run, now_ms, LAND_LEASE_TTL_MS)
}

/// [`take_run_lease_for_land`] with another lifetime (tests).
pub fn take_run_lease_for_land_with_ttl(
    graph: &crate::store::SharedGraph,
    run: &RunId,
    now_ms: i64,
    ttl_ms: i64,
) -> Result<LandLease, OrchestratorError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let holder = run_lease_holder(run, ProcessIdentity::current(), nanos);
    let key = run_lease_key(run);
    let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
    match acquire_run_lease(&g, run, &holder, "land", &key, ttl_ms, now_ms)? {
        Acquisition::Granted(leases) => Ok(LandLease {
            graph: graph.clone(),
            holder,
            leases,
            ttl_ms,
        }),
        Acquisition::Blocked(conflicts) => {
            let c = conflicts.into_iter().next();
            Err(OrchestratorError::RunBusy {
                run: run.to_string(),
                holder: c.as_ref().map(|c| c.held_by.clone()).unwrap_or_default(),
                expires_ms: c.map(|c| c.expires_ms).unwrap_or(0),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The producer-verified commit is the contract's own merge, never the
    /// run's newest verified commit; nothing when it made none.
    #[test]
    fn the_producer_commit_is_the_contracts_own_merge() {
        let mut integration = BTreeMap::new();
        integration.insert(
            "p".to_owned(),
            Integration::Merged {
                commit: "own".into(),
            },
        );
        integration.insert(
            "q".to_owned(),
            Integration::Conflict {
                tip: "t".into(),
                paths: vec![],
                detail: String::new(),
                commit: None,
            },
        );
        integration.insert("e".to_owned(), Integration::Empty);
        assert_eq!(merged_commit_of(&integration, "p").as_deref(), Some("own"));
        assert_eq!(merged_commit_of(&integration, "q"), None, "commit: None");
        assert_eq!(merged_commit_of(&integration, "e"), None);
        assert_eq!(merged_commit_of(&integration, "absent"), None);
    }
}
