//! Between-run edits on the run loop (decision 0100, sections 3, 5 and 7):
//! reopening a run with a change, copying a run's plan with an edited
//! instruction, and applying a change through one `replan_delta` call. The
//! rules are `crate::delta`'s; this file makes the call, saves each state,
//! and does what needs the outside world: a redirect into a resumable
//! worker's inbox, a stopped attempt's worktree and leases.
//!
//! The call runs on the loop in this build: a reopen applies its change
//! before the loop runs, and plan review waits for its answer anyway. Taking
//! planner calls off the loop is a later step of the same decision.

use super::*;
use crate::coordination::{ClosedContract, Closure};
use crate::delta::{
    self, AppliedOps, ChangeSource, ChangeState, DeltaAnswer, DeltaRules, NotApplied,
    RecordedChange,
};
use std::time::Instant;

/// When a user's request reached kGraf: the start of the B9 and B11
/// interactions (decision 0100, section 1).
#[derive(Clone, Copy, Debug)]
pub struct Received {
    pub instant: Instant,
    pub at_ms: i64,
}

impl Received {
    pub fn now() -> Self {
        Self {
            instant: Instant::now(),
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
        }
    }
}

/// A change as the user gave it.
#[derive(Clone, Debug)]
pub struct ChangeInput {
    pub source: ChangeSource,
    /// The change's text, or, for an edit or a copy, the edited instruction.
    pub text: String,
    /// For an edit: the instruction the edit was made from. `None`: the
    /// run's effective instruction as the record holds it.
    pub opened: Option<String>,
    pub received: Received,
}

impl ChangeInput {
    pub fn change(text: impl Into<String>, received: Received) -> Self {
        Self {
            source: ChangeSource::Change,
            text: text.into(),
            opened: None,
            received,
        }
    }

    pub fn edit(edited: impl Into<String>, opened: Option<String>, received: Received) -> Self {
        Self {
            source: ChangeSource::Edit,
            text: edited.into(),
            opened,
            received,
        }
    }

    /// The change log's text and the edited instruction: an edit's line
    /// diff, or the change itself. An edit that changes no line is refused,
    /// before anything is written or asked.
    fn resolve(&self, effective: &str) -> Result<(String, Option<String>), OrchestratorError> {
        match self.source {
            ChangeSource::Edit | ChangeSource::Copy => {
                let opened = self.opened.as_deref().unwrap_or(effective);
                match delta::line_diff(opened, &self.text) {
                    Some(diff) => Ok((diff, Some(self.text.clone()))),
                    None => Err(OrchestratorError::NothingChanged),
                }
            }
            ChangeSource::Change | ChangeSource::PlanReview => {
                if self.text.trim().is_empty() {
                    return Err(OrchestratorError::NothingChanged);
                }
                Ok((self.text.clone(), None))
            }
        }
    }
}

/// How a change card ended.
enum CardAnswer {
    /// Apply: the user said yes, or the mode stopped asking meanwhile.
    Yes,
    No,
    /// The run was stopped while the card waited.
    Stopped,
    /// The run's time limit ran out while the card waited.
    TimeUp,
}

/// Loop-only state of the changes this process took in.
#[derive(Default)]
pub(super) struct DeltaRuntime {
    /// When each change of this process was received, by number.
    received: BTreeMap<u32, Received>,
    /// Plan review: the change's answer was a full re-plan, which the user
    /// present at the card takes.
    pub(super) full_replan: bool,
    /// Stale re-checks still running: the change's number, the work it
    /// checks, and the task, aborted with a notice if the run ends first.
    pub(super) stale_tasks: Vec<(u32, Vec<String>, tokio::task::JoinHandle<()>)>,
}

impl Orchestrator {
    /// Reopen `run` with a change (decision 0100, section 5): a completed or
    /// stopped run takes one `replan_delta` call, no `decompose` and no
    /// re-grounding, and runs what the change added or changed on its
    /// branch. Refused while a land is pending or another process holds the
    /// run, with nothing written, so the change is not lost.
    pub async fn reopen(
        self,
        run: RunId,
        change: ChangeInput,
    ) -> Result<RunReport, OrchestratorError> {
        self.name_run(&run);
        let store = RunStore::new(self.services.graph.clone());
        let record = store
            .load(&run)?
            .ok_or_else(|| OrchestratorError::NoRecord(run.to_string()))?;
        if let Some(reason) = delta::reopen_refusal(&record) {
            return Err(OrchestratorError::ReopenRefused {
                run: run.to_string(),
                reason,
            });
        }
        let (text, edited) = change.resolve(&delta::effective_instruction(&record))?;
        // Plan review governs the first plan only (decision 0100, section 4):
        // a run with verified work has no first plan left to show.
        if record.plan_approved.is_none() && record.verified_contracts.is_empty() {
            self.check_plan_review()?;
        }
        let from = run_status_name(&record.status);
        // When the session before ended; a record from before kept no end,
        // so its last save stands in.
        let prev_session_end_ms = record.session_end_ms.unwrap_or(record.updated_ms);
        let mut state = on_heap(|| RunLoop::open(self, record, true)).await?;
        // A reopen that carries a change re-grounds nothing (section 5): the
        // change is applied by its own call, and nothing else changed.
        state.reground_on_resume = false;
        state.record.reopens += 1;
        let session = state.record.reopens;
        // Traced first, so every event of the new session follows it.
        let at_ms = state.now();
        state
            .trace(TraceEvent::Reopened {
                at_ms,
                session,
                from: from.to_owned(),
                change: u32::try_from(state.record.changes.len() + 1).unwrap_or(u32::MAX),
                prev_session_end_ms: Some(prev_session_end_ms),
            })
            .await?;
        on_heap(|| state.resume_triage()).await?;
        state.take_change(
            change.source,
            &text,
            edited.as_deref(),
            change.received,
            session,
        )?;
        let planned = if state.record.planned {
            on_heap(|| state.advance_changes()).await.map(|()| true)
        } else {
            match on_heap(|| state.plan()).await {
                Ok(true) => on_heap(|| state.advance_changes()).await.map(|()| true),
                other => other,
            }
        };
        if planned.is_ok() {
            state.stop_if_change_blocks().await?;
            state.emit_reopen_latency(change.received).await?;
        }
        on_heap(|| state.run_reviewed(planned)).await
    }

    /// Start `run` from a copy of `source`'s plan with an edited instruction
    /// (decision 0100, section 7): the source's contracts whose verified work
    /// is already in the new run's base are done, the rest remain, and one
    /// `replan_delta` call applies the edit. The source is read, never
    /// written.
    pub async fn start_from_run(
        self,
        run: RunId,
        source: RunId,
        instruction: String,
        received: Received,
    ) -> Result<RunReport, OrchestratorError> {
        self.name_run(&run);
        let store = RunStore::new(self.services.graph.clone());
        let old = store
            .load(&source)?
            .ok_or_else(|| OrchestratorError::NoRecord(source.to_string()))?;
        let input = ChangeInput {
            source: ChangeSource::Copy,
            text: instruction,
            opened: None,
            received,
        };
        let (text, edited) = input.resolve(&delta::effective_instruction(&old))?;
        self.check_plan_review()?;
        let now = self.services.clock.now_ms();
        let record = copied_record(&old, run, now);
        let mut state = on_heap(|| RunLoop::open(self, record, false)).await?;
        state.seed_copy(&old).await?;
        state.take_change(ChangeSource::Copy, &text, edited.as_deref(), received, 0)?;
        let planned = on_heap(|| state.advance_changes()).await.map(|()| true);
        if planned.is_ok() {
            state.stop_if_change_blocks().await?;
        }
        on_heap(|| state.run_reviewed(planned)).await
    }
}

/// The status as the `reopened` event names it.
fn run_status_name(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Completed => "completed",
        RunStatus::Blocked { .. } => "blocked",
        RunStatus::Stopped => "stopped",
    }
}

/// A new run's record copied from `old`: the source's effective instruction
/// as its original (the edit is its first change), its specs, and the
/// source named. Contracts are seeded once the run's branch exists
/// ([`RunLoop::seed_copy`]).
fn copied_record(old: &RunStateRecord, run: RunId, now: i64) -> RunStateRecord {
    let mut record = RunStateRecord::new(run, delta::effective_instruction(old), now);
    record.copied_from = Some(old.run.clone());
    record.plan_root = Some(old.plan_root.clone().unwrap_or_else(|| old.run.clone()));
    // The specs a copy reads; a check is carried only on the contracts that
    // remain, after `seed_copy` judges it.
    record.plan_specs = old
        .plan_specs
        .iter()
        .map(|(id, s)| {
            let mut s = s.clone();
            s.check = None;
            (id.clone(), s)
        })
        .collect();
    record
}

impl RunLoop {
    /// Take in one change: number it, save it as received.
    pub(super) fn take_change(
        &mut self,
        source: ChangeSource,
        text: &str,
        edited: Option<&str>,
        received: Received,
        session: u32,
    ) -> Result<u32, OrchestratorError> {
        let number = u32::try_from(self.record.changes.len() + 1).unwrap_or(u32::MAX);
        let change =
            RecordedChange::received(number, source, text, edited, received.at_ms, session);
        self.record.changes.push(change);
        // B4 and B11 time the loop's own work on the change, from here; B9
        // times the whole reopen from the request (`received`).
        self.delta_rt.received.insert(number, Received::now());
        self.queue(TraceEvent::Change {
            at_ms: self.now(),
            number,
            step: "received".into(),
            class: None,
            detail: format!("from {}", source.as_str()),
        });
        self.save()?;
        Ok(number)
    }

    /// Advance every change that still needs work, in order. A change left
    /// pending or refused does not stop the next.
    pub(super) async fn advance_changes(&mut self) -> Result<(), OrchestratorError> {
        let open: Vec<usize> = self
            .record
            .changes
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.state.is_open() || (c.state == ChangeState::Applied && !c.outside_done)
            })
            .map(|(i, _)| i)
            .collect();
        for i in open {
            if self.stopping {
                break;
            }
            self.advance_change(i).await?;
        }
        Ok(())
    }

    /// The newest session's change was not applied (pending or refused): the
    /// run stops before it dispatches anything, so the plan the change was
    /// meant to alter does not run as it was. Said as a notice; a plain
    /// resume runs the plan as it stands.
    pub(super) async fn stop_if_change_blocks(&mut self) -> Result<(), OrchestratorError> {
        let session = self.record.reopens;
        let Some((number, why)) = self.record.changes.iter().rev().find_map(|c| {
            (c.session == session)
                .then_some(())
                .and(match &c.state {
                    ChangeState::Pending { reason } => Some(format!("is pending: {reason}")),
                    ChangeState::Refused { reason } => Some(format!("was refused: {reason}")),
                    _ => None,
                })
                .map(|why| (c.number, why))
        }) else {
            return Ok(());
        };
        let detail = kgraf_secrets::scrub_persist_owned(format!(
            "change {number} {why}; the run stopped before dispatching anything, so the plan \
             it was meant to change does not run as it was. Send the change again, or resume \
             the run to run the plan as it stands"
        ));
        self.notices.insert(detail.clone());
        self.record.status = RunStatus::Stopped;
        self.stopping = true;
        self.save()?;
        let at_ms = self.now();
        self.trace(TraceEvent::Notice {
            at_ms,
            contract: String::new(),
            worker: String::new(),
            detail,
        })
        .await
    }

    fn change_state(&mut self, i: usize, state: ChangeState) {
        if let Some(c) = self.record.changes.get_mut(i) {
            c.state = state;
        }
    }

    async fn note_change(
        &mut self,
        i: usize,
        step: &str,
        detail: String,
    ) -> Result<(), OrchestratorError> {
        let Some(c) = self.record.changes.get(i) else {
            return Ok(());
        };
        let (number, class) = (c.number, c.class.map(|k| k.as_str().to_owned()));
        let at_ms = self.now();
        self.trace(TraceEvent::Change {
            at_ms,
            number,
            step: step.to_owned(),
            class,
            detail,
        })
        .await
    }

    /// One change through its states until it is applied, refused, pending,
    /// or stopped. Saves after every step.
    async fn advance_change(&mut self, i: usize) -> Result<(), OrchestratorError> {
        loop {
            let Some(change) = self.record.changes.get(i).cloned() else {
                return Ok(());
            };
            match change.state {
                ChangeState::Received | ChangeState::Planning => {
                    if !self.ask_delta(i).await? {
                        return Ok(());
                    }
                }
                ChangeState::Answered => {
                    self.apply_answer(i).await?;
                    if self.stopping
                        && self
                            .record
                            .changes
                            .get(i)
                            .is_some_and(|c| c.state == ChangeState::Answered)
                    {
                        // Stopped while its card waited: the answer stays
                        // saved and a resume asks again.
                        return Ok(());
                    }
                }
                ChangeState::Applied if !change.outside_done => {
                    return self.outside_work(i).await;
                }
                ChangeState::Applied
                | ChangeState::Refused { .. }
                | ChangeState::Pending { .. } => {
                    return Ok(());
                }
            }
        }
    }

    /// Make the delta call for change `i` and save its answer. `Ok(false)`:
    /// no answer to apply (a stop, a refusal, the time limit).
    async fn ask_delta(&mut self, i: usize) -> Result<bool, OrchestratorError> {
        if let Some(why) = self.planner_budget_gate("change").await? {
            let reason = format!("the run's planner time is spent ({why}); the change waits");
            self.change_state(
                i,
                ChangeState::Pending {
                    reason: reason.clone(),
                },
            );
            self.save()?;
            self.note_change(i, "pending", reason).await?;
            return Ok(false);
        }
        let snapshot = delta::snapshot(&self.record);
        if let Some(c) = self.record.changes.get_mut(i) {
            c.snapshot = snapshot;
            c.state = ChangeState::Planning;
        }
        self.save()?;
        let Some(change) = self.record.changes.get(i).cloned() else {
            return Ok(false);
        };
        let context = self.change_context(&change)?;
        let cache_layout = self.config.cache_layout;
        let request = crate::plan::DeltaRequest {
            prompt: delta::delta_prompt(&self.record, &change, &context, cache_layout),
            cap: delta::output_cap(&self.record, &change),
            own_checks: self.config.own_checks,
            cache_layout,
            mark_plan: cache_layout
                && delta::plan_mark_due(
                    self.s.decomposer.cache_ttl(),
                    delta::previous_delta_ms(&self.record, change.number, self.last_delta_ms),
                    self.now(),
                ),
        };
        let basis = CostBasis {
            model: self.s.decomposer.cost_basis().model,
            max_output_tokens: self
                .s
                .decomposer
                .output_limit(delta::DELTA_KIND, Some(request.cap)),
        };
        let reservation = self.reserve_call(&basis, estimate_tokens(&request.prompt.text()) + 600);
        let stop = self.stop_token.clone();
        let asked_at = self.now();
        let asked = std::time::Instant::now();
        self.publish_planning(Some("change"));
        let out =
            super::planner_tasks::cut_by_stop(&stop, self.s.decomposer.replan_delta(&request))
                .await;
        self.publish_planning(None);
        self.planner_budget
            .record(u64::try_from(self.now() - asked_at).unwrap_or(0));
        self.last_delta_ms = Some(self.now());
        let Some(out) = out else {
            self.trace_planning_cut("replan_delta", asked).await;
            // The answer never reached the record: a resume asks again. The
            // cut call is counted with this change all the same.
            self.stopping = true;
            self.settle_call(reservation, None)?;
            self.count_stray_calls(i, asked_at);
            self.save()?;
            self.notices.insert(
                "the run was stopped during a change's planner call; the call may have billed \
                 and is listed by `kgraf unreconciled`, and a resume asks again"
                    .to_owned(),
            );
            return Ok(false);
        };
        match out {
            Ok(out) => {
                self.settle_call(reservation, Some(&out.calls))?;
                let tokens = self.tokens_of(&out.calls);
                let kind = out.answer.kind();
                if let Some(c) = self.record.changes.get_mut(i) {
                    c.calls.extend(out.calls.iter().map(ToString::to_string));
                    c.tokens += tokens;
                    c.answer = Some(out.answer);
                    c.state = ChangeState::Answered;
                }
                // Saved before anything of it applies (as `plan_progress`).
                self.save()?;
                self.note_change(i, "answered", format!("{kind}, {tokens} tokens"))
                    .await?;
                Ok(true)
            }
            Err(e) => {
                self.settle_call(reservation, e.calls())?;
                let calls: Vec<CallId> = e.calls().unwrap_or_default().to_vec();
                let tokens = self.tokens_of(&calls);
                if let Some(c) = self.record.changes.get_mut(i) {
                    c.calls.extend(calls.iter().map(ToString::to_string));
                    c.tokens += tokens;
                }
                let reason = kgraf_secrets::scrub_persist_owned(format!(
                    "the change's planner call failed: {e}"
                ));
                self.refuse_change(i, reason).await?;
                Ok(false)
            }
        }
    }

    async fn refuse_change(&mut self, i: usize, reason: String) -> Result<(), OrchestratorError> {
        self.notices.insert(format!(
            "change {} was not applied: {reason}",
            self.record.changes.get(i).map_or(0, |c| c.number)
        ));
        self.change_state(
            i,
            ChangeState::Refused {
                reason: reason.clone(),
            },
        );
        self.save()?;
        self.note_change(i, "refused", reason).await
    }

    async fn pend_change(&mut self, i: usize, reason: String) -> Result<(), OrchestratorError> {
        self.notices.insert(format!(
            "change {} is pending: {reason}",
            self.record.changes.get(i).map_or(0, |c| c.number)
        ));
        self.change_state(
            i,
            ChangeState::Pending {
                reason: reason.clone(),
            },
        );
        self.save()?;
        self.note_change(i, "pending", reason).await
    }

    /// Re-ask change `i` once, quoting `why`; a second refusal refuses it.
    async fn reask_or_refuse(
        &mut self,
        i: usize,
        why: String,
        conflict: bool,
    ) -> Result<(), OrchestratorError> {
        let spent = self.record.changes.get(i).is_some_and(|c| {
            if conflict {
                c.conflict_reasks > 0
            } else {
                c.reasks > 0
            }
        });
        if spent {
            return self.refuse_change(i, why).await;
        }
        if let Some(c) = self.record.changes.get_mut(i) {
            if conflict {
                c.conflict_reasks += 1;
            } else {
                c.reasks += 1;
            }
            c.refusal = Some(kgraf_secrets::scrub_persist_owned(why.clone()));
            c.state = ChangeState::Received;
        }
        self.save()?;
        self.note_change(i, "reasked", why).await
    }

    /// Apply change `i`'s saved answer, whole or not at all.
    async fn apply_answer(&mut self, i: usize) -> Result<(), OrchestratorError> {
        let Some(change) = self.record.changes.get(i).cloned() else {
            return Ok(());
        };
        let answer = match change.answer.clone() {
            Some(a) => a,
            None => {
                // An answered state with no answer cannot be applied: asked
                // again (never applied in part).
                self.change_state(i, ChangeState::Received);
                return self.save();
            }
        };
        let (adds, changes, removes) = match answer {
            DeltaAnswer::NoChange { answer } => {
                let applied = AppliedOps {
                    no_change: Some(kgraf_secrets::scrub_persist_owned(answer.clone())),
                    ..AppliedOps::default()
                };
                self.notices
                    .insert(kgraf_secrets::scrub_persist_owned(format!(
                        "change {}: the planner found nothing in the plan to change: {answer}",
                        change.number
                    )));
                return self.finish_apply(i, None, applied).await;
            }
            DeltaAnswer::ReplanAll { why }
                if change.source == ChangeSource::PlanReview && self.perms.attended =>
            {
                // The user is at the plan review card: a full re-plan is
                // theirs to see before anything runs.
                self.delta_rt.full_replan = true;
                self.notices
                    .insert(kgraf_secrets::scrub_persist_owned(format!(
                        "change {}: the planner asked for a full re-plan ({why}); the plan is made \
                     again with your change and shown",
                        change.number
                    )));
                let applied = AppliedOps {
                    replanned_all: true,
                    ..AppliedOps::default()
                };
                return self.finish_apply(i, None, applied).await;
            }
            DeltaAnswer::ReplanAll { why } => {
                return self
                    .pend_change(
                        i,
                        kgraf_secrets::scrub_persist_owned(format!(
                            "the planner asks for a full re-plan ({why}); a full re-plan runs \
                             only on your yes, so the plan is unchanged"
                        )),
                    )
                    .await;
            }
            DeltaAnswer::Delta {
                add,
                change,
                remove,
            } => (add, change, remove),
        };
        // Section 3, rule 6: a straight chain among the added contracts goes
        // through chain collapse, as a plan's does. No pre-split call: an
        // oversized added contract is caught by the sizing-miss signals.
        // The adds may depend on the plan's open and verified contracts: the
        // collapse's own validation takes those as external.
        let known: BTreeSet<String> = self
            .record
            .open_contracts
            .iter()
            .map(|c| c.contract.clone())
            .chain(self.record.verified_contracts.iter().cloned())
            .collect();
        let adds = self
            .collapse_chains_with(adds, &BTreeSet::new(), &known)
            .await?;
        let roster = self.roster();
        let tools = self.tools();
        let settings = self.perms.control.settings();
        let mode = settings.mode;
        let own_checks = self.config.own_checks;
        let rules = DeltaRules {
            roster: &roster,
            tools: &tools,
            max_contracts: self.config.max_contracts_per_plan,
            bypass_git_files: self.bypass_git_files(),
            mode,
            own_checks: own_checks.then_some(&settings),
        };
        let prepared = match delta::prepare(&self.record, &change, adds, changes, removes, &rules) {
            Ok(p) => p,
            Err(NotApplied::Refused(why)) => return self.reask_or_refuse(i, why, false).await,
            Err(NotApplied::Conflict(list)) => {
                let why = format!(
                    "the plan moved while the change was planned: {}",
                    list.join("; ")
                );
                return self.reask_or_refuse(i, why, true).await;
            }
            Err(NotApplied::Final(why)) => return self.refuse_change(i, why).await,
        };
        // The repository checks of a plan (`plan_repo`), only while nothing
        // of the run has merged: the checkout holds no verified work of the
        // run, so a check against it would refuse a plan built on that work.
        if self.record.verified_contracts.is_empty()
            && let Some(root) = self.s.workspaces.repo_root()
        {
            let mut copy = self.record.clone();
            delta::apply(
                &mut copy,
                &prepared,
                &self.run_id(),
                own_checks,
                change.number,
            );
            let specs: Vec<ContractSpec> = copy
                .open_contracts
                .iter()
                .filter_map(|c| copy.plan_specs.get(&c.contract).cloned())
                .collect();
            if let Some(refusal) =
                crate::plan_repo::refusal(&specs, &[], &crate::plan_repo::DiskRepo(root))
            {
                return self.reask_or_refuse(i, refusal.to_string(), false).await;
            }
        }
        let users_yes = change.source.is_users_yes();
        if delta::needs_yes(mode, prepared.class, users_yes) {
            if let Some(c) = self.record.changes.get_mut(i) {
                c.class = Some(prepared.class);
            }
            if !self.change_asks_attended() {
                return self
                    .pend_change(
                        i,
                        format!(
                            "it is a {} change and your permission mode ({}) asks before one \
                             applies; nobody was there to answer",
                            prepared.class.as_str(),
                            mode.as_str()
                        ),
                    )
                    .await;
            }
            match self.ask_for_change(&change, &prepared, users_yes).await? {
                CardAnswer::Yes => {}
                CardAnswer::No => {
                    return self
                        .refuse_change(i, "you answered no to the change".to_owned())
                        .await;
                }
                // Stopped while the card waited: the answer stays saved.
                CardAnswer::Stopped => return Ok(()),
                CardAnswer::TimeUp => {
                    return self
                        .pend_change(
                            i,
                            "the run's time limit ran out before you answered the change card"
                                .to_owned(),
                        )
                        .await;
                }
            }
        }
        // Whole or nothing: applied to a copy, which replaces the record in
        // one save.
        let mut next = self.record.clone();
        let applied = delta::apply(
            &mut next,
            &prepared,
            &self.run_id(),
            own_checks,
            change.number,
        );
        self.record = next;
        let added = applied.added.clone();
        self.finish_apply(i, Some(prepared.class), applied).await?;
        self.ask_added_interfaces(change.number, &added).await
    }

    /// Whether someone can answer a change card now: the box attends this
    /// run, or a person at the terminal answers (`--interactive`).
    fn change_asks_attended(&self) -> bool {
        self.perms.attended || self.perms.desk.terminal_answers_changes()
    }

    /// Show the change card and wait for the answer. `Some(true)`: apply
    /// (the user said yes, or the mode stopped asking meanwhile);
    /// `Some(false)`: the user said no; `None`: the run was stopped while it
    /// waited.
    async fn ask_for_change(
        &mut self,
        change: &RecordedChange,
        prepared: &delta::Prepared,
        users_yes: bool,
    ) -> Result<CardAnswer, OrchestratorError> {
        let desk = self.perms.desk.clone();
        desk.show_change(crate::review_desk::ChangeView {
            run: self.run_id(),
            number: change.number,
            class: prepared.class.as_str().to_owned(),
            mode: self.perms.control.mode().as_str().to_owned(),
            change: change.text.clone(),
            ops: prepared.summary(),
            stops_run: change.source != ChangeSource::PlanReview,
        });
        let at_ms = self.now();
        self.trace(TraceEvent::Change {
            at_ms,
            number: change.number,
            step: "asked".into(),
            class: Some(prepared.class.as_str().to_owned()),
            detail: format!(
                "your permission mode ({}) asks before a {} change applies",
                self.perms.control.mode().as_str(),
                prepared.class.as_str()
            ),
        })
        .await?;
        let mut modes = self.perms.control.subscribe();
        let answer = loop {
            if let Some(a) = desk.take_change_answer() {
                break if a { CardAnswer::Yes } else { CardAnswer::No };
            }
            if !delta::needs_yes(self.perms.control.mode(), prepared.class, users_yes) {
                break CardAnswer::Yes;
            }
            if self.stop_requested() || self.stopping {
                break CardAnswer::Stopped;
            }
            let answered = desk.answered();
            let limit = self.limit_reached();
            let mut time_up = false;
            tokio::select! {
                () = answered => {}
                _ = modes.changed() => {}
                () = limit => time_up = true,
                m = self.rx.recv() => match m {
                    Some(LoopMsg::Control(Control::Stop)) => self.stopping = true,
                    Some(other) => self.backlog.push_back(other),
                    None => {}
                },
            }
            if time_up {
                break CardAnswer::TimeUp;
            }
        };
        desk.clear_change();
        Ok(answer)
    }

    /// Resolves when the run's time limit runs out; never, with no limit.
    pub(super) fn limit_reached(&self) -> impl std::future::Future<Output = ()> + use<> {
        let left = self
            .run_deadline
            .as_ref()
            .map(|d| std::time::Duration::from_millis(d.limit_left_ms(self.now())));
        async move {
            match left {
                Some(left) => tokio::time::sleep(left).await,
                None => std::future::pending::<()>().await,
            }
        }
    }

    /// Section 3, rule 6: with contract checks on, interfaces are asked only
    /// for the edges the added contracts make (their producers write code a
    /// consumer uses), off the plan's other contracts, which they may bind.
    /// A failed ask leaves those edges unchecked and the plan as it is.
    /// Interface ids take the change's `d<n>.` prefix, as a re-plan's take
    /// `re<n>.`: the book keeps every interface once registered.
    async fn ask_added_interfaces(
        &mut self,
        number: u32,
        added: &[String],
    ) -> Result<(), OrchestratorError> {
        if self.contracts.is_none() || added.is_empty() {
            return Ok(());
        }
        let specs: Vec<ContractSpec> = added
            .iter()
            .filter_map(|id| self.record.plan_specs.get(id).cloned())
            .collect();
        let outer: BTreeSet<String> = self
            .record
            .open_contracts
            .iter()
            .map(|c| c.contract.clone())
            .chain(self.record.verified_contracts.iter().cloned())
            .filter(|id| !added.contains(id))
            .collect();
        let context = PlanRequest {
            instruction: kgraf_secrets::model_copy(&delta::effective_instruction(&self.record))
                .into_owned(),
            split_reason: None,
            notes: Vec::new(),
            known_files: self.known_files()?,
            routes: self.roster(),
            model_routes: self.model_route_lines()?,
            model_calls: None,
            tools: self.tools(),
            templates: Vec::new(),
            interfaces: true,
            project_context: String::new(),
            own_checks: false,
        };
        let Some((interfaces, unchecked)) = self.interface_asks(context, &specs, &outer).await?
        else {
            return Ok(());
        };
        let prefix = format!("d{number}.");
        let interfaces: Vec<crate::checks::InterfaceContract> = interfaces
            .into_iter()
            .map(|mut ic| {
                if !ic.id.starts_with(&prefix) {
                    ic.id = format!("{prefix}{}", ic.id);
                }
                ic
            })
            .collect();
        let registered = interfaces.clone();
        self.register_interfaces(interfaces).await?;
        let contracts: Vec<OpenContract> = self
            .record
            .open_contracts
            .iter()
            .filter(|c| added.contains(&c.contract))
            .cloned()
            .collect();
        self.record_unchecked(&contracts, &registered, &unchecked)
            .await?;
        self.save()
    }

    /// Mark change `i` applied with what it did, save, then do the outside
    /// work: redirects into resumable workers' inboxes, stopped attempts'
    /// worktrees and leases.
    async fn finish_apply(
        &mut self,
        i: usize,
        class: Option<delta::DeltaClass>,
        applied: AppliedOps,
    ) -> Result<(), OrchestratorError> {
        let effective = delta::effective_instruction(&self.record);
        let base = self.record.replans;
        if let Some(c) = self.record.changes.get_mut(i) {
            c.state = ChangeState::Applied;
            c.class = class;
            c.refusal = None;
            c.effective_instruction = Some(delta::instruction_after(&effective, c));
            c.applied = Some(applied.clone());
        }
        // The held-plan re-plan bound counts per change.
        self.record.replan_base = base;
        if self.record.status != RunStatus::Running && !applied_is_empty(&applied) {
            self.record.status = RunStatus::Running;
        }
        self.save()?;
        self.note_change(i, "applied", applied_detail(&applied))
            .await?;
        if let Some(lost) = &applied.lost {
            let detail = format!(
                "change {}: it supersedes verified work on {lost} with nothing that writes \
                 those files; that verified code stays merged on the run's branch",
                self.record.changes.get(i).map_or(0, |c| c.number)
            );
            self.notices.insert(detail.clone());
            let at_ms = self.now();
            self.trace(TraceEvent::Notice {
                at_ms,
                contract: String::new(),
                worker: String::new(),
                detail,
            })
            .await?;
        }
        self.outside_work(i).await?;
        if !applied.stale.is_empty() {
            self.notices.insert(format!(
                "verified work that depends on superseded work may be stale: {}; it is checked \
                 again in the background once the work replacing it ({}) merges",
                applied.stale.join(", "),
                applied.stale_after.join(", ")
            ));
            self.start_due_stale_rechecks()?;
        }
        if !applied.unknown_staleness.is_empty() {
            self.notices.insert(format!(
                "whether this verified work is stale is unknown, because this run's record did \
                 not keep its dependencies: {}",
                applied.unknown_staleness.join(", ")
            ));
        }
        self.emit_change_latency(i).await
    }

    /// An amendment of a contract whose worker is resumable: a redirect in
    /// each worker's inbox, delivered at its next boundary when it resumes,
    /// and kept on the contract so a later attempt's brief carries it.
    async fn redirect_running(&mut self, i: usize, id: &str) -> Result<(), OrchestratorError> {
        let number = self.record.changes.get(i).map_or(0, |c| c.number);
        let Some(c) = self.record.contract(id).cloned() else {
            return Ok(());
        };
        // The redirect text was put on the contract by `delta::apply`, in
        // the change's own save; the inbox rows are written from it.
        let text = kgraf_secrets::model_copy(
            &c.redirects
                .last()
                .cloned()
                .unwrap_or_else(|| format!("The plan changed (change {number}).")),
        )
        .into_owned();
        let now = self.now();
        let store = sessions::sessions_of(&self.s);
        for worker in &c.workers {
            let row = delta::redirect_row_id(number, worker.as_str());
            // Written once: after a restart the row is already there.
            if store.inbox_item(&row).ok().flatten().is_some() {
                continue;
            }
            self.ensure_session_row(worker);
            let item = kgraf_core::session::InboxItem {
                id: row,
                session: worker.clone(),
                source: InboxSource::User,
                kind: InboxKind::Redirect,
                mode: InjectMode::AtBoundary,
                text: text.clone(),
                received_ms: now,
                not_before_ms: now,
                delivered: None,
            };
            if let Err(e) = store.push_inbox(&item) {
                self.notices.insert(format!(
                    "the change's redirect to {worker} could not be recorded, so its next \
                     round does not see it: {e}"
                ));
            }
        }
        self.note_change(
            i,
            "redirected",
            format!("{id}: a redirect waits in each resumable worker's inbox"),
        )
        .await
    }

    /// A superseded or removed contract's stopped attempt: its transcript
    /// stays, its worktree is released under the run's retention (0088),
    /// and its leases go.
    async fn cancel_stopped_attempt(
        &mut self,
        old: &OpenContract,
    ) -> Result<(), OrchestratorError> {
        let id = old.contract.clone();
        let workers: Vec<String> = old.workers.iter().map(ToString::to_string).collect();
        for w in &workers {
            let worker = AgentId::new(w.clone());
            // Reopened from the recorded handle and released; never a fresh
            // worktree, and nothing when it is already gone.
            self.s.workspaces.discard_stopped(old, &worker).await;
            // A worker from a process that predates session rows gets one,
            // so its end is recorded.
            self.ensure_session_row(&worker);
            if let Err(e) = self.retire_session(&worker, SessionOutcome::Cancelled) {
                self.notices.insert(format!(
                    "the session of {worker} could not be marked ended: {e}"
                ));
            }
        }
        let holder = self.holder(old);
        self.release_leases_if_unused(&holder)?;
        self.save()?;
        let at_ms = self.now();
        self.trace(TraceEvent::Change {
            at_ms,
            number: 0,
            step: "cancelled".into(),
            class: None,
            detail: format!(
                "{id}: its attempt ({}) was stopped by the change; transcripts kept",
                workers.join(", ")
            ),
        })
        .await
    }

    /// The work outside the record an applied change needs: redirect inbox
    /// rows for amended running work, and the stopped attempts of replaced
    /// or removed work. Each step is idempotent, so a restart between them
    /// does each once; `outside_done` is saved when all are.
    async fn outside_work(&mut self, i: usize) -> Result<(), OrchestratorError> {
        let Some(applied) = self.record.changes.get(i).and_then(|c| c.applied.clone()) else {
            return Ok(());
        };
        for id in &applied.redirected {
            self.redirect_running(i, id).await?;
        }
        for old in &applied.stopped {
            self.cancel_stopped_attempt(old).await?;
        }
        if let Some(c) = self.record.changes.get_mut(i) {
            c.outside_done = true;
        }
        self.save()
    }

    /// Start each applied change's stale re-check whose replacing work has
    /// all merged (verified), on the tip that now holds it. Checked on every
    /// turn of the loop; a started check is saved, so a resume does not
    /// start it twice.
    pub(super) fn start_due_stale_rechecks(&mut self) -> Result<(), OrchestratorError> {
        let due: Vec<(usize, u32, Vec<String>)> = self
            .record
            .changes
            .iter()
            .enumerate()
            .filter(|(_, c)| c.state == ChangeState::Applied && !c.stale_started)
            .filter_map(|(i, c)| {
                let a = c.applied.as_ref()?;
                let merged = a
                    .stale_after
                    .iter()
                    .all(|id| self.record.verified_contracts.contains(id));
                (!a.stale.is_empty() && merged).then(|| (i, c.number, a.stale.clone()))
            })
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        // Saved as started before any starts: a restart never runs one twice.
        for (i, _, _) in &due {
            if let Some(c) = self.record.changes.get_mut(*i) {
                c.stale_started = true;
            }
        }
        self.save()?;
        for (_, number, stale) in due {
            self.recheck_stale(number, stale);
        }
        Ok(())
    }

    /// At the run's end: a stale re-check still running is stopped and said
    /// so, and one whose replacing work never merged is said not to have
    /// run. Neither is silent.
    pub(super) fn end_stale_rechecks(&mut self) {
        for (change, stale, task) in std::mem::take(&mut self.delta_rt.stale_tasks) {
            task.abort();
            self.notices.insert(format!(
                "change {change}: the re-check of verified work it made stale ({}) did not \
                 finish before the run ended; it was stopped and its result is unknown",
                stale.join(", ")
            ));
        }
        for c in &self.record.changes {
            let Some(a) = c.applied.as_ref() else {
                continue;
            };
            if c.state == ChangeState::Applied && !c.stale_started && !a.stale.is_empty() {
                self.notices.insert(format!(
                    "change {}: verified work it made stale ({}) was not checked again: the \
                     work replacing it ({}) did not merge in this run",
                    c.number,
                    a.stale.join(", "),
                    a.stale_after.join(", ")
                ));
            }
        }
    }

    /// Re-run the checks of verified work a change made stale, in the
    /// background, on the run's current tip, which holds the work that
    /// replaced what it depends on (section 3, rule 6): the
    /// project's validation commands, as the root gate runs them. The result
    /// is information: a failure is shown, starts no remediation, and the
    /// root gate still judges the run. No gate runner: said, not run.
    fn recheck_stale(&mut self, change: u32, stale: Vec<String>) {
        let Some(runner) = self
            .coord
            .gate_runner
            .clone()
            .filter(|r| !r.commands().is_empty())
        else {
            self.notices.insert(format!(
                "change {change}: the stale verified work ({}) is not checked again: the run has \
                 no validation commands; the root gate judges it",
                stale.join(", ")
            ));
            return;
        };
        let pinned = self.record.coordination.integration_tip.clone().map(|sha| {
            crate::run_state::VerifiedCommit {
                sha,
                verified_by: format!("change {change}: stale re-check"),
                verified_ms: self.now(),
            }
        });
        let request = crate::gates::GateRequest {
            gate: format!("stale:change-{change}"),
            covers: stale.clone(),
            commands: runner.commands(),
            pinned,
        };
        let tx = self.tx.clone();
        let checked = stale.clone();
        let task = tokio::spawn(async move {
            let verdict = runner.run(request).await;
            let _ = tx
                .send(LoopMsg::Control(Control::StaleChecked {
                    change,
                    stale: checked,
                    verdict: Box::new(verdict),
                }))
                .await;
        });
        self.delta_rt.stale_tasks.push((change, stale, task));
    }

    /// The stale re-check ended: a pass or a failure, said as a notice and a
    /// `change` trace event (information only).
    pub(super) async fn on_stale_checked(
        &mut self,
        change: u32,
        stale: &[String],
        verdict: Result<crate::gates::GateVerdict, String>,
    ) -> Result<(), OrchestratorError> {
        self.delta_rt.stale_tasks.retain(|(n, _, _)| *n != change);
        let (step, detail) = match &verdict {
            Ok(v) if v.passed => (
                "stale_passed",
                format!(
                    "change {change}: verified work it made stale ({}) still passes `{}`",
                    stale.join(", "),
                    v.by
                ),
            ),
            Ok(v) => (
                "stale_failed",
                format!(
                    "change {change}: verified work it made stale ({}) fails `{}` on the new tip; \
                     information only: the root gate judges the run",
                    stale.join(", "),
                    v.by
                ),
            ),
            Err(e) => (
                "stale_failed",
                format!(
                    "change {change}: the re-check of verified work it made stale ({}) could not \
                     run: {e}",
                    stale.join(", ")
                ),
            ),
        };
        let detail = kgraf_secrets::scrub_persist_owned(detail);
        self.notices.insert(detail.clone());
        let at_ms = self.now();
        self.trace(TraceEvent::Change {
            at_ms,
            number: change,
            step: step.into(),
            class: None,
            detail,
        })
        .await
    }

    /// Ledger rows of `replan_delta` calls of this run started at or after
    /// `since_ms` that no change counts yet: a call cut by a stop. Counted
    /// with change `i`, at the spend the ledger settled.
    fn count_stray_calls(&mut self, i: usize, since_ms: i64) {
        let run = self.run_id();
        let agent = format!("orchestrator:{run}");
        let counted: BTreeSet<String> = self
            .record
            .changes
            .iter()
            .flat_map(|c| c.calls.iter().cloned())
            .collect();
        let rows: Vec<kgraf_provider::CallRecord> = self
            .s
            .ledger
            .calls_under_task(&run, Some(&agent))
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                r.trace.kind.as_deref() == Some(delta::DELTA_KIND)
                    && r.started_ms >= since_ms
                    && !counted.contains(&r.id.to_string())
            })
            .collect();
        let tokens: u64 = rows
            .iter()
            .map(|r| r.usage.total_input() + r.usage.total_output())
            .sum();
        if let Some(c) = self.record.changes.get_mut(i) {
            c.calls.extend(rows.iter().map(|r| r.id.to_string()));
            c.tokens += tokens;
        }
    }

    /// The project context a delta sends: what `decompose` sends (package
    /// names and the signatures the change names).
    fn change_context(&self, change: &RecordedChange) -> Result<String, OrchestratorError> {
        let files = self.known_files()?;
        let graph = self.s.graph.clone();
        let text = kgraf_secrets::model_copy(change.edited.as_deref().unwrap_or(&change.text))
            .into_owned();
        blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            Ok::<_, OrchestratorError>(
                crate::planner_context::project_context(&g, &text, &files).text,
            )
        })
    }

    /// Input and output tokens of `calls`, from the ledger.
    fn tokens_of(&self, calls: &[CallId]) -> u64 {
        calls
            .iter()
            .filter_map(|c| self.s.ledger.call(c).ok().flatten())
            .map(|r| r.usage.total_input() + r.usage.total_output())
            .sum()
    }

    /// B4 (`delta_applied`), or B11 (`edit_applied`) for an edit or a copy:
    /// from the loop taking the change in to its application. Model time is
    /// the change's own calls. A change taken in by an earlier process is not
    /// measured (its clock is gone).
    async fn emit_change_latency(&mut self, i: usize) -> Result<(), OrchestratorError> {
        let Some(change) = self.record.changes.get(i).cloned() else {
            return Ok(());
        };
        let Some(received) = self.delta_rt.received.get(&change.number).copied() else {
            return Ok(());
        };
        let kind = match change.source {
            ChangeSource::Edit | ChangeSource::Copy => crate::latency::LatencyKind::EditApplied,
            ChangeSource::Change | ChangeSource::PlanReview => {
                crate::latency::LatencyKind::DeltaApplied
            }
        };
        let split = self.change_split(&change, received);
        self.emit_latency(kind, split, None).await
    }

    /// B9 (`reopen`): from the request to the run running again with its
    /// change applied, host start and the loop's opening included.
    async fn emit_reopen_latency(&mut self, received: Received) -> Result<(), OrchestratorError> {
        let Some(change) = self.record.changes.last().cloned() else {
            return Ok(());
        };
        let split = self.change_split(&change, received);
        self.emit_latency(crate::latency::LatencyKind::Reopen, split, None)
            .await
    }

    fn change_split(&self, change: &RecordedChange, received: Received) -> crate::latency::Split {
        let wall_us = i64::try_from(received.instant.elapsed().as_micros()).unwrap_or(i64::MAX);
        let rows: Vec<kgraf_provider::CallRecord> = change
            .calls
            .iter()
            .filter_map(|c| self.s.ledger.call(&CallId::new(c.clone())).ok().flatten())
            .collect();
        let refs: Vec<&kgraf_provider::CallRecord> = rows.iter().collect();
        let model = crate::latency::call_intervals(&refs, received.at_ms, self.now());
        crate::latency::attribute(wall_us, &model, &[])
    }

    /// The new run's contracts from the source's: done when the verified
    /// work is already in this run's base (its commit is an ancestor, or its
    /// merge was empty), remaining otherwise. Unversioned work counts as
    /// remaining: nothing proves it is in the base.
    async fn seed_copy(&mut self, old: &RunStateRecord) -> Result<(), OrchestratorError> {
        let run = self.run_id();
        let mut done = Vec::new();
        let mut remaining: Vec<ContractSpec> = Vec::new();
        for id in &old.verified_contracts {
            let in_base = match old.coordination.integration.get(id) {
                Some(Integration::Empty) => true,
                Some(Integration::Merged { commit }) => self
                    .s
                    .workspaces
                    .commit_landed(commit)
                    .await
                    .unwrap_or(false),
                _ => false,
            };
            let spec = old.plan_specs.get(id).cloned().or_else(|| {
                old.coordination
                    .closed
                    .iter()
                    .find(|c| c.contract == *id)
                    .map(closed_spec)
            });
            match (in_base, spec) {
                (true, _) => done.push(id.clone()),
                (false, Some(s)) => remaining.push(s),
                (false, None) => {}
            }
        }
        for c in &old.open_contracts {
            let mut s = old.plan_specs.get(&c.contract).cloned().unwrap_or_else(|| {
                let mut s = closed_spec(&ClosedContract::of(c, Closure::Verified));
                s.depends_on.clone_from(&c.depends_on);
                s.tools.clone_from(&c.tools);
                s.task_kind = c.task_kind;
                s.route = c.route.clone().filter(|_| c.route_pinned);
                s
            });
            s.instruction.clone_from(&c.instruction);
            s.acceptance.clone_from(&c.acceptance);
            remaining.push(s);
        }
        let done_set: BTreeSet<&String> = done.iter().collect();
        let remaining_ids: BTreeSet<String> = remaining.iter().map(|s| s.id.clone()).collect();
        for id in &done {
            if let Some(closed) = old.coordination.closed.iter().find(|c| c.contract == *id) {
                self.record.coordination.closed.push(closed.clone());
            }
            self.record
                .coordination
                .integration
                .insert(id.clone(), Integration::Empty);
        }
        self.record.verified_contracts = done.clone();
        // The commits that verified the work carried over keep their record.
        for id in &done {
            if let Some(commit) = old.verified_commits.get(id) {
                self.record
                    .verified_commits
                    .insert(id.clone(), commit.clone());
            }
        }
        for mut s in remaining {
            // A dependency on work done in the base is met; one on a
            // contract that is neither remaining nor done (a split's parent)
            // is dropped, with the plan's own checks catching the rest.
            s.depends_on
                .retain(|d| done_set.contains(d) || remaining_ids.contains(d));
            let mut c = crate::plan::contract_from_spec(
                &s,
                s.id.clone(),
                &run,
                Vec::new(),
                Vec::new(),
                s.depends_on.clone(),
            );
            c.slice_names = s.slices.clone();
            c.slice_from = s.writes.first().map(|w| w.path.clone());
            // A copied check is judged as intake judges one (decision 0100):
            // the source's setting and permissions are not this run's.
            if let Some(check) = s.check.take() {
                let writes: Vec<String> = s.writes.iter().map(|w| w.path.clone()).collect();
                let settings = self.perms.control.settings();
                let refusal = if self.config.own_checks {
                    crate::shortfall::check_refusal(&check, &writes, &settings)
                } else {
                    Some("contract checks of their own are off for this run".to_owned())
                };
                match refusal {
                    None => {
                        c.contract_checks = vec![check.clone()];
                        s.check = Some(check);
                    }
                    Some(why) => {
                        let detail = kgraf_secrets::scrub_persist_owned(format!(
                            "contract {}: the check copied from run {} is dropped: {why}",
                            s.id, old.run
                        ));
                        self.notices.insert(detail.clone());
                        let at_ms = self.now();
                        self.trace(TraceEvent::Notice {
                            at_ms,
                            contract: s.id.clone(),
                            worker: String::new(),
                            detail,
                        })
                        .await?;
                    }
                }
            }
            self.record.plan_specs.insert(s.id.clone(), s);
            self.record.open_contracts.push(c);
        }
        self.record.planned = true;
        self.plan_gates_hook(&BTreeMap::new());
        if let Some(g) = self
            .record
            .coordination
            .gates
            .iter_mut()
            .find(|g| g.id == crate::gates::ROOT_GATE)
        {
            for id in &done {
                if !g.covers.contains(id) {
                    g.covers.push(id.clone());
                }
            }
        }
        if !self.record.coordination.gates.is_empty() {
            self.record.coordination.base_commit =
                self.s.workspaces.integration_tip().await.ok().flatten();
        }
        self.save()?;
        let at_ms = self.now();
        self.trace(TraceEvent::Notice {
            at_ms,
            contract: String::new(),
            worker: String::new(),
            detail: format!(
                "copied the plan of run {}: {} contract(s) done in this run's base ({}), {} \
                 remaining",
                old.run,
                done.len(),
                if done.is_empty() {
                    "none".to_owned()
                } else {
                    done.join(", ")
                },
                self.record.open_contracts.len()
            ),
        })
        .await
    }
}

/// The spec a closed contract keeps when its plan-time spec was not
/// recorded: its id, text and writes.
fn closed_spec(c: &ClosedContract) -> ContractSpec {
    ContractSpec {
        id: c.contract.clone(),
        instruction: c.instruction.clone(),
        acceptance: c.acceptance.clone(),
        writes: c
            .writes
            .iter()
            .map(|p| WriteTarget {
                path: p.clone(),
                node: None,
            })
            .collect(),
        depends_on: Vec::new(),
        slices: Vec::new(),
        tools: Vec::new(),
        route: None,
        attempts: 1,
        task_kind: None,
        check: None,
    }
}

fn applied_is_empty(a: &AppliedOps) -> bool {
    a.added.is_empty()
        && a.amended.is_empty()
        && a.superseded.is_empty()
        && a.removed.is_empty()
        && a.followups.is_empty()
}

/// One line of what a change did.
fn applied_detail(a: &AppliedOps) -> String {
    let mut parts = Vec::new();
    let list = |v: &[String]| v.join(", ");
    if !a.added.is_empty() {
        parts.push(format!("added {}", list(&a.added)));
    }
    if !a.amended.is_empty() {
        parts.push(format!("amended {}", list(&a.amended)));
    }
    if !a.superseded.is_empty() {
        parts.push(format!(
            "superseded {}",
            a.superseded
                .iter()
                .map(|(o, n)| format!("{o} by {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !a.removed.is_empty() {
        parts.push(format!("removed {}", list(&a.removed)));
    }
    if !a.followups.is_empty() {
        parts.push(format!(
            "follow-ups {}",
            a.followups
                .iter()
                .map(|(o, n)| format!("{n} of {o}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(n) = &a.no_change {
        parts.push(format!("no change: {n}"));
    }
    if parts.is_empty() {
        "nothing to change".to_owned()
    } else {
        parts.join("; ")
    }
}
