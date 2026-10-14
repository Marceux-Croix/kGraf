//! Planner calls a contract's dispatch waits on, run as tasks (decision 0100,
//! section 4): an interface's handshake, its stub land, each consumer's test,
//! and a re-grounding.
//!
//! **What stays on the loop.** The loop builds the request, makes the
//! reservation, and takes the answer in: it settles the reservation, and only
//! the loop writes the record and merges onto the run's branch. The task makes
//! the call and nothing else. (A stub land commits through the merge-back,
//! whose own lock serializes it with every merge of the process.)
//!
//! **The gate (`OrchestratorConfig::concurrent_planner`).** Off: the loop waits
//! for the task it spawned, as it waited for the call before, so nothing else
//! runs meanwhile and dispatch order is unchanged; a reader still
//! acknowledges, a status question and the question cards read the published
//! snapshot, and a stop cuts the call. On: the contract that needs the answer
//! waits and the loop goes on verifying, merging and dispatching the others;
//! the answer arrives as [`LoopMsg::Planner`].
//!
//! **Keys.** Each call is stamped `(kind, subject, generation)`: the subject is
//! `interface@version`, `interface@version/consumer` or the contract id, and the
//! generation counts the plan's replacements (re-plans, changes, reopens). At
//! most one call per `(kind, subject)` is in flight. An answer whose stamp is no
//! longer current is dropped at the fold, its spend still settled; a task whose
//! stamp goes stale while it runs is cut.
//!
//! **Cuts.** Every task runs under one cancel token, a child of the run's stop,
//! and inside a ledger stop scope, so a call cut by the stop or as stale
//! settles `Cancelled`, never `Unreconciled`.
//!
//! **Kills.** With the gate on, a task appends its answer to the intake table
//! (`planner_answer`) before it tells the loop. A process killed before the
//! loop took the answer in leaves the row: the resume charges its calls once
//! (the calls the ledger seeding already charged are skipped) and hands the
//! answer to the contract that needs it, so the paid call is not made again.
//!
//! ```text
//!   needed --spawn (reserve)--> in flight --answer--> taken in (settle) --> ready --> used by the gate
//!                                   |                        \--stale--> dropped (spend counted)
//!                                   \--stop or stale--> cut --> settled Cancelled
//! ```

use super::{LoopMsg, OrchestratorError, RunLoop};
use crate::admission::Reservation;
use crate::checks::HandshakeStatement;
use crate::consumer_tests::ConsumerTestOutcome;
use crate::run_intake::{IntakeKind, Row};
use crate::run_state::{ContractStatus, GroundingTrigger};
use crate::trace::TraceEvent;
use kgraf_core::CallId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::oneshot;

/// The kinds of planner call run as tasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TaskKind {
    Handshake,
    StubLand,
    ConsumerTest,
    Reground,
}

impl TaskKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::StubLand => "stub_land",
            Self::ConsumerTest => "consumer_test",
            Self::Reground => "reground",
        }
    }
}

/// A call's stamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TaskKey {
    pub kind: TaskKind,
    pub subject: String,
    pub generation: u64,
}

type Slot = (TaskKind, String);

impl TaskKey {
    fn slot(&self) -> Slot {
        (self.kind, self.subject.clone())
    }
}

/// One party's handshake statement, or why it could not be had, with the
/// calls it took (`None`: the failure named none, and the hold is charged
/// whole).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PartyStatement {
    pub party: String,
    pub route: String,
    pub statement: Result<HandshakeStatement, String>,
    pub calls: Option<Vec<CallId>>,
}

/// A re-grounding's verdict: whether the step serves, why, the answering
/// call and the calls re-asked for a malformed answer.
pub(super) type GroundVerdict = (bool, String, Option<CallId>, Vec<CallId>);

/// A re-grounding that could not answer: why, and the last call it made.
pub(super) type GroundFailure = (String, Option<CallId>);

/// What a task answered.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum Answer {
    /// One per open party, in party order.
    Handshake(Vec<PartyStatement>),
    /// The landed commit, or why the land failed.
    StubLand(Result<String, String>),
    ConsumerTest {
        outcome: ConsumerTestOutcome,
        /// The writer's calls (`None`: a failure that named none).
        calls: Option<Vec<CallId>>,
        /// Malformed answers asked again.
        retries: usize,
    },
    Reground {
        verdict: Result<GroundVerdict, GroundFailure>,
        calls: Option<Vec<CallId>>,
        /// The notes the call carried: delivered once it answered.
        note_ids: Vec<String>,
        trigger: GroundingTrigger,
    },
}

impl Answer {
    /// Every ledger call of the answer.
    fn calls(&self) -> Vec<CallId> {
        match self {
            Self::Handshake(parties) => parties
                .iter()
                .flat_map(|p| p.calls.clone().unwrap_or_default())
                .collect(),
            Self::StubLand(_) => Vec::new(),
            Self::ConsumerTest { calls, .. } | Self::Reground { calls, .. } => {
                calls.clone().unwrap_or_default()
            }
        }
    }

    fn failed(&self) -> bool {
        match self {
            Self::Handshake(parties) => parties.iter().any(|p| p.statement.is_err()),
            Self::StubLand(r) => r.is_err(),
            Self::ConsumerTest { outcome, .. } => {
                !matches!(outcome, ConsumerTestOutcome::Accepted { .. })
            }
            Self::Reground { verdict, .. } => verdict.is_err(),
        }
    }
}

/// The body of a `planner_answer` intake row.
#[derive(Serialize, Deserialize)]
struct AnswerRow {
    key: TaskKey,
    answer: Answer,
}

/// A task's end, taken in by the loop.
pub(super) struct TaskDone {
    key: TaskKey,
    /// `None`: the task was cut before it answered.
    answer: Option<Answer>,
    /// The `planner_answer` row the task appended (gate on only).
    row: Option<i64>,
    slot_wait_us: i64,
    call_us: i64,
    stop_us: Option<i64>,
}

struct InFlight {
    generation: u64,
    cancel: kgraf_tools::CancellationToken,
    /// When the loop cut it as stale, for the stop latency.
    cut_at: Arc<Mutex<Option<Instant>>>,
    reservations: Vec<Reservation>,
    task: tokio::task::JoinHandle<()>,
    /// Set by the task just before it reports: a task that ended without it
    /// died (a panic) and will never report.
    reported: Arc<std::sync::atomic::AtomicBool>,
}

/// The loop's planner tasks.
#[derive(Default)]
pub(super) struct PlannerTasks {
    in_flight: BTreeMap<Slot, InFlight>,
    /// Answers taken in and not yet used.
    ready: BTreeMap<Slot, Ready>,
    /// What each re-grounding in flight took over from the loop's cadence,
    /// by contract: given back when it ends with no answer.
    pub(super) reground_prior: BTreeMap<String, RegroundPrior>,
    /// Tasks that died without an answer, by key: the second death answers
    /// with a failure instead of asking again.
    deaths: BTreeMap<Slot, u32>,
    /// The notes each re-grounding in flight carries, by contract: not
    /// counted as waiting meanwhile, so they are not sent twice.
    pub(super) carried_notes: BTreeMap<String, Vec<String>>,
}

/// An answer taken in and not yet used: its stamp's generation, and the
/// intake row that keeps it across a kill (gate on).
struct Ready {
    generation: u64,
    answer: Answer,
    row: Option<i64>,
}

/// The re-grounding cadence a re-grounding in flight took over.
#[derive(Clone, Copy, Debug)]
pub(super) struct RegroundPrior {
    pub trigger: GroundingTrigger,
    pub on_resume: bool,
    pub since: u32,
}

/// What a planner step came to.
pub(super) enum Step {
    Done(Answer),
    /// The call is in flight (gate on), or was cut or answered stale: the
    /// contract waits.
    Waiting,
}

impl PlannerTasks {
    pub(super) fn busy(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Cut every task but a stub land (the run is stopping). Idempotent.
    pub(super) fn cut_all(&self) {
        for ((kind, _), f) in &self.in_flight {
            if *kind != TaskKind::StubLand {
                f.cancel.cancel();
            }
        }
    }

    /// Abort every task: the loop is gone and takes no answer in.
    pub(super) fn abort_all(&self) {
        for f in self.in_flight.values() {
            f.cancel.cancel();
            f.task.abort();
        }
    }
}

fn micros(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

impl RunLoop {
    /// The plan's generation: it moves when the plan is replaced (a re-plan, a
    /// change, a reopen), and an answer stamped with an older one is stale.
    pub(super) fn plan_generation(&self) -> u64 {
        u64::from(self.record.replans)
            + u64::from(self.record.reopens)
            + self.record.changes.len() as u64
    }

    pub(super) fn task_key(&self, kind: TaskKind, subject: String) -> TaskKey {
        TaskKey {
            kind,
            subject,
            generation: self.plan_generation(),
        }
    }

    /// Whether `key` still names work the run needs: its generation is the
    /// plan's, and its interface is at that version or its contract still
    /// waits to dispatch.
    fn key_current(&self, key: &TaskKey) -> bool {
        if key.generation != self.plan_generation() {
            return false;
        }
        match key.kind {
            TaskKind::Handshake | TaskKind::StubLand | TaskKind::ConsumerTest => {
                let label = key.subject.split('/').next().unwrap_or_default();
                let Some((id, version)) = label.rsplit_once('@') else {
                    return false;
                };
                self.contracts
                    .as_ref()
                    .and_then(|d| d.book_version(id))
                    .is_some_and(|v| format!("v{v}") == version)
            }
            TaskKind::Reground => self
                .record
                .contract(&key.subject)
                .is_some_and(|c| c.status == ContractStatus::Planned),
        }
    }

    /// The answer already taken in for `key` (`Done`), or `Waiting` while
    /// its call is in flight; `None`: the call is to be made
    /// ([`Self::planner_call`]). An answer of an older generation is dropped.
    pub(super) fn planner_lookup(&mut self, key: &TaskKey) -> Option<Step> {
        let slot = key.slot();
        if let Some(ready) = self.planner.ready.remove(&slot) {
            // Used or dropped either way: its row is resolved in the record,
            // saved with the state the answer changes.
            if let Some(row) = ready.row {
                self.record.intake_above.push(row);
            }
            if ready.generation == key.generation {
                return Some(Step::Done(ready.answer));
            }
        }
        self.planner
            .in_flight
            .contains_key(&slot)
            .then_some(Step::Waiting)
    }

    /// Make the call for `key` as a task: see the module docs.
    /// `reservations` are made by the caller on the loop, before this, and
    /// settled when the answer is taken in. Call only when
    /// [`Self::planner_lookup`] returned `None`.
    pub(super) async fn planner_call<F>(
        &mut self,
        key: TaskKey,
        reservations: Vec<Reservation>,
        call: F,
    ) -> Result<Step, OrchestratorError>
    where
        F: std::future::Future<Output = Answer> + Send + 'static,
    {
        let slot = key.slot();
        let concurrent = self.config.concurrent_planner;
        let (inline_tx, inline_rx) = oneshot::channel();
        let reply = if concurrent {
            Reply::Loop(self.tx.clone())
        } else {
            Reply::Inline(inline_tx)
        };
        let cancel = self.stop_token.child_token();
        let cut_at = Arc::new(Mutex::new(None));
        let reported = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn(run_task(TaskRun {
            reported: reported.clone(),
            key: key.clone(),
            run: self.record.run.to_string(),
            intake: self.intake.clone(),
            append: concurrent,
            cancel: cancel.clone(),
            cut_at: cut_at.clone(),
            spawned: Instant::now(),
            reply,
            call: Box::pin(call),
        }));
        let replaced = self.planner.in_flight.insert(
            slot.clone(),
            InFlight {
                generation: key.generation,
                cancel,
                cut_at,
                reservations,
                task,
                reported,
            },
        );
        // One call per key: `planner_lookup` is asked first, so nothing is
        // replaced; were it, its hold would be settled, never leaked.
        if let Some(old) = replaced {
            old.cancel.cancel();
            for r in old.reservations {
                self.settle_call(r, None)?;
            }
        }
        if concurrent {
            return Ok(Step::Waiting);
        }
        // Gate off: the loop waits for the call, as it did when it made it
        // itself. The task ends at a stop (its token is the stop's child).
        let Ok(done) = inline_rx.await else {
            // The task died before it reported (its sender is gone): it is
            // taken as ended now, not at a later step.
            self.reap_planner_slot(slot).await?;
            return Ok(self.planner_lookup(&key).unwrap_or(Step::Waiting));
        };
        Ok(match self.take_done(done).await? {
            Some((answer, _)) => Step::Done(answer),
            None => Step::Waiting,
        })
    }

    /// A task ended (gate on): its answer is taken in and kept for the
    /// contract that needs it.
    pub(super) async fn on_planner_done(
        &mut self,
        done: TaskDone,
    ) -> Result<(), OrchestratorError> {
        let slot = done.key.slot();
        if let Some((answer, row)) = self.take_done(done).await? {
            if slot.0 == TaskKind::StubLand {
                // A land commits whatever happens next: recorded now, so a stop
                // or a kill after it never loses a commit on the branch.
                if let Some(row) = row {
                    self.record.intake_above.push(row);
                }
                return self.take_stub_land(&slot.1, answer).await;
            }
            let generation = self.plan_generation();
            self.planner.ready.insert(
                slot,
                Ready {
                    generation,
                    answer,
                    row,
                },
            );
        }
        Ok(())
    }

    /// Settle a task's reservations at its calls' ledger cost, trace it, and
    /// return its answer when it is current.
    async fn take_done(
        &mut self,
        done: TaskDone,
    ) -> Result<Option<(Answer, Option<i64>)>, OrchestratorError> {
        let slot = done.key.slot();
        let Some(entry) = self.planner.in_flight.remove(&slot) else {
            return Ok(None);
        };
        let prior = if done.key.kind == TaskKind::Reground {
            // Delivered by this answer, or free for the next re-grounding.
            self.planner.carried_notes.remove(&done.key.subject);
            self.planner.reground_prior.remove(&done.key.subject)
        } else {
            None
        };
        let calls = done.answer.as_ref().map(Answer::calls).unwrap_or_default();
        match &done.answer {
            Some(Answer::Handshake(parties)) => {
                for (r, p) in entry.reservations.into_iter().zip(parties) {
                    self.settle_call(r, p.calls.as_deref())?;
                }
            }
            Some(a) => {
                let all = a.calls();
                let named = match a {
                    Answer::ConsumerTest { calls, .. } | Answer::Reground { calls, .. } => {
                        calls.is_some()
                    }
                    _ => true,
                };
                for r in entry.reservations {
                    self.settle_call(r, named.then_some(all.as_slice()))?;
                }
            }
            // Cut before it answered: its calls settle `Cancelled` (the stop
            // scope); what they used is not known, so the hold is charged
            // whole.
            None => {
                for r in entry.reservations {
                    self.settle_call(r, None)?;
                }
            }
        }
        let current = entry.generation == done.key.generation && self.key_current(&done.key);
        let outcome = match &done.answer {
            None => "cancelled",
            Some(_) if !current => "stale",
            Some(a) if a.failed() => "failed",
            Some(_) => "answered",
        };
        if self.config.concurrent_planner || done.answer.is_none() {
            let at_ms = self.now();
            self.write_latency_event(TraceEvent::PlannerTask {
                at_ms,
                kind: done.key.kind.as_str().to_owned(),
                subject: done.key.subject.clone(),
                generation: done.key.generation,
                outcome: outcome.to_owned(),
                calls: calls.len(),
                slot_wait_us: done.slot_wait_us,
                call_us: done.call_us,
                stop_us: done.stop_us,
            })
            .await;
        }
        let cut_stale = entry.cut_at.lock().is_ok_and(|at| at.is_some());
        if outcome == "cancelled" && cut_stale {
            self.notices.insert(format!(
                "a {} call for `{}` was cut: its plan moved on while it ran",
                done.key.kind.as_str(),
                done.key.subject
            ));
        }
        if outcome == "stale" {
            self.notices.insert(format!(
                "a {} answer for `{}` came after its plan moved on; it was dropped and its \
                 spend is counted",
                done.key.kind.as_str(),
                done.key.subject
            ));
        }
        let answer = if current { done.answer } else { None };
        if answer.is_none() {
            // No answer to use: the row is resolved, and a re-grounding gives
            // back the cadence it took, so its contract is not dispatched
            // ungrounded.
            if let Some(row) = done.row {
                self.record.intake_above.push(row);
            }
            if let Some(p) = prior {
                self.restore_reground(p);
            }
        }
        Ok(answer.map(|a| (a, done.row)))
    }

    /// Keep `answer` for the contract that needs it.
    pub(super) fn planner_keep(
        &mut self,
        kind: TaskKind,
        subject: &str,
        generation: u64,
        answer: Answer,
    ) {
        self.planner.ready.insert(
            (kind, subject.to_owned()),
            Ready {
                generation,
                answer,
                row: None,
            },
        );
    }

    /// Give a re-grounding's cadence back after it ended with no answer.
    fn restore_reground(&mut self, p: RegroundPrior) {
        self.reground_on_resume |= p.on_resume;
        self.since_reground = self.since_reground.max(p.since);
    }

    /// Take in the tasks that ended without reporting (a panic in the call).
    pub(super) async fn reap_dead_planner_tasks(&mut self) -> Result<(), OrchestratorError> {
        let dead: Vec<Slot> = self
            .planner
            .in_flight
            .iter()
            .filter(|(_, f)| {
                f.task.is_finished() && !f.reported.load(std::sync::atomic::Ordering::SeqCst)
            })
            .map(|(slot, _)| slot.clone())
            .collect();
        for slot in dead {
            self.reap_planner_slot(slot).await?;
        }
        Ok(())
    }

    /// One task that ended without an answer: removed so the run does not
    /// wait on it, its hold charged whole (the call may have billed; `kgraf
    /// unreconciled` lists it), and said. The first death lets its contract
    /// ask again (a re-grounding gives back the cadence it took, so the
    /// contract is not dispatched ungrounded); a second gives a failed answer,
    /// so the contract takes its kind's failure path (a hold, a failed land, a
    /// test not run) rather than asking without end.
    pub(super) async fn reap_planner_slot(&mut self, slot: Slot) -> Result<(), OrchestratorError> {
        let Some(entry) = self.planner.in_flight.remove(&slot) else {
            return Ok(());
        };
        let (kind, subject) = slot.clone();
        let (note_ids, prior) = if kind == TaskKind::Reground {
            (
                self.planner
                    .carried_notes
                    .remove(&subject)
                    .unwrap_or_default(),
                self.planner.reground_prior.remove(&subject),
            )
        } else {
            (Vec::new(), None)
        };
        for r in entry.reservations {
            self.settle_call(r, None)?;
        }
        let deaths = {
            let d = self.planner.deaths.entry(slot.clone()).or_insert(0);
            *d += 1;
            *d
        };
        let detail = format!(
            "the {} call for `{subject}` ended without an answer{}; the call may have billed \
             (`kgraf unreconciled` lists it)",
            kind.as_str(),
            if deaths > 1 {
                ", again, and its contract takes it as failed"
            } else {
                "; it is asked again when needed"
            }
        );
        if deaths > 1 {
            let why = format!("the {} call ended without an answer", kind.as_str());
            let failed = match kind {
                TaskKind::Handshake => Answer::Handshake(vec![PartyStatement {
                    party: subject.clone(),
                    route: String::new(),
                    statement: Err(why),
                    calls: None,
                }]),
                TaskKind::StubLand => Answer::StubLand(Err(why)),
                TaskKind::ConsumerTest => Answer::ConsumerTest {
                    outcome: ConsumerTestOutcome::NotRun { reason: why },
                    calls: None,
                    retries: 0,
                },
                TaskKind::Reground => Answer::Reground {
                    verdict: Err((why, None)),
                    calls: None,
                    note_ids,
                    trigger: prior.map_or(GroundingTrigger::Cadence, |p| p.trigger),
                },
            };
            self.planner.ready.insert(
                slot,
                Ready {
                    generation: entry.generation,
                    answer: failed,
                    row: None,
                },
            );
        } else if let Some(p) = prior {
            self.restore_reground(p);
        }
        self.notices.insert(detail.clone());
        let at_ms = self.now();
        self.trace(TraceEvent::Notice {
            at_ms,
            contract: String::new(),
            worker: String::new(),
            detail,
        })
        .await
    }

    /// Cut the tasks whose stamp went stale while they ran (a revision, a
    /// re-plan, a contract no longer waiting), and every task when the run
    /// stops.
    pub(super) fn cut_stale_planner_tasks(&mut self) {
        if self.stopping {
            self.planner.cut_all();
            return;
        }
        let stale: Vec<Slot> = self
            .planner
            .in_flight
            .iter()
            .filter(|((kind, subject), f)| {
                !self.key_current(&TaskKey {
                    kind: *kind,
                    subject: subject.clone(),
                    generation: f.generation,
                })
            })
            .map(|(slot, _)| slot.clone())
            .collect();
        for slot in stale {
            // A stub land is never cut: its merge runs on a blocking thread
            // that commits whatever happens to the task, so the loop lets it
            // finish and takes its answer in.
            if slot.0 == TaskKind::StubLand {
                continue;
            }
            if let Some(f) = self.planner.in_flight.get(&slot) {
                if let Ok(mut at) = f.cut_at.lock() {
                    at.get_or_insert_with(Instant::now);
                }
                f.cancel.cancel();
            }
        }
    }

    /// A `planner_answer` row from the intake table; `true` when it is
    /// resolved (the fold may pass it). An answer this process holds (in
    /// flight, or taken in and not yet used) is unresolved. One a killed
    /// process left is charged once (calls the resume's ledger seeding already
    /// charged are skipped) and handed to the contract that needs it; it stays
    /// unresolved until used, and its use is saved with the state it changes
    /// (`RunStateRecord::intake_above`).
    pub(super) fn fold_planner_answer(
        &mut self,
        row: &kgraf_graph::IntakeRow,
    ) -> Result<bool, OrchestratorError> {
        let Ok(parsed) = serde_json::from_str::<AnswerRow>(&row.body) else {
            self.notices.insert(format!(
                "intake row {} holds a planner answer this build cannot read; the call is made \
                 again when it is needed",
                row.id
            ));
            return Ok(true);
        };
        let slot = parsed.key.slot();
        if self.planner.in_flight.contains_key(&slot) || self.planner.ready.contains_key(&slot) {
            return Ok(false);
        }
        self.charge_unseeded(&parsed.answer.calls())?;
        self.planner.ready.insert(
            slot,
            Ready {
                generation: parsed.key.generation,
                answer: parsed.answer,
                row: Some(row.id),
            },
        );
        Ok(false)
    }

    /// Charge `calls` the run's spend does not hold yet, by the rule a resume
    /// seeds by, and mark them charged.
    fn charge_unseeded(&mut self, calls: &[CallId]) -> Result<(), OrchestratorError> {
        let fresh: Vec<CallId> = calls
            .iter()
            .filter(|c| !self.seeded.contains(*c))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return Ok(());
        }
        let ledger = self.s.ledger.clone();
        let records = super::blocking(|| {
            fresh
                .iter()
                .map(|id| ledger.call(id))
                .collect::<Result<Vec<_>, _>>()
        })?;
        let run = self.run_id();
        for record in records.into_iter().flatten() {
            if !Arc::make_mut(&mut self.seeded).insert(record.id.clone()) {
                continue;
            }
            match self.ledger_cost(&record) {
                Some(cost) => {
                    let at = record.finished_ms.unwrap_or(record.started_ms);
                    self.s.admission.budget().record_call_spend(
                        record.id.as_str(),
                        &run,
                        &run,
                        cost.to_money(),
                        at,
                    );
                }
                None => self.note_unpriced(&record.model),
            }
        }
        Ok(())
    }
}

/// A call on the planning path (`decompose`, `interfaces`, a change's
/// delta), which stays on the loop: made inside a ledger stop scope and cut by
/// the run's stop, so a stop leaves its calls `Cancelled` rather than
/// `Unreconciled`. `None`: the stop cut it.
pub(super) async fn cut_by_stop<F: std::future::Future>(
    stop: &kgraf_tools::CancellationToken,
    fut: F,
) -> Option<F::Output> {
    let probe = stop.clone();
    tokio::select! {
        out = kgraf_provider::within_stop_scope(move || probe.is_cancelled(), fut) => Some(out),
        () = stop.cancelled() => None,
    }
}

impl RunLoop {
    /// Trace a planning-path call the stop cut, with the stop latency: from
    /// the stop asked for to the call let go.
    pub(super) async fn trace_planning_cut(&mut self, kind: &str, asked: Instant) {
        let ended = Instant::now();
        let stop_us = self
            .intake
            .stopped_at()
            .map(|at| micros(ended.saturating_duration_since(at)));
        let at_ms = self.now();
        let generation = self.plan_generation();
        self.write_latency_event(TraceEvent::PlannerTask {
            at_ms,
            kind: kind.to_owned(),
            subject: String::new(),
            generation,
            outcome: "cancelled".to_owned(),
            calls: 0,
            slot_wait_us: 0,
            call_us: micros(ended.saturating_duration_since(asked)),
            stop_us,
        })
        .await;
    }
}

/// Where a task's end goes.
enum Reply {
    Loop(tokio::sync::mpsc::Sender<LoopMsg>),
    Inline(oneshot::Sender<TaskDone>),
}

struct TaskRun {
    reported: Arc<std::sync::atomic::AtomicBool>,
    key: TaskKey,
    run: String,
    intake: Arc<crate::run_intake::Intake>,
    /// Set with the gate on: the answer is appended before the loop hears.
    append: bool,
    cancel: kgraf_tools::CancellationToken,
    cut_at: Arc<Mutex<Option<Instant>>>,
    spawned: Instant,
    reply: Reply,
    call: std::pin::Pin<Box<dyn std::future::Future<Output = Answer> + Send>>,
}

async fn run_task(t: TaskRun) {
    let started = Instant::now();
    let probe = t.cancel.clone();
    let cancel = t.cancel.clone();
    let scoped = kgraf_provider::within_stop_scope(move || probe.is_cancelled(), t.call);
    let answer = if t.key.kind == TaskKind::StubLand {
        // Never cut: see `cut_stale_planner_tasks`.
        Some(scoped.await)
    } else {
        tokio::select! {
            // A call cut by the token settles `Cancelled` (the stop scope).
            a = scoped => Some(a),
            () = cancel.cancelled() => None,
        }
    };
    let ended = Instant::now();
    let stop_us = answer.is_none().then(|| {
        let cut = t
            .cut_at
            .lock()
            .ok()
            .and_then(|c| *c)
            .or_else(|| t.intake.stopped_at())
            .unwrap_or(ended);
        micros(ended.saturating_duration_since(cut))
    });
    let mut row = None;
    if let (true, Some(a)) = (t.append, &answer) {
        let intake = &t.intake;
        let body = serde_json::to_string(&AnswerRow {
            key: t.key.clone(),
            answer: a.clone(),
        })
        .unwrap_or_default();
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        row = intake
            .append(Row {
                run: t.run.clone(),
                kind: IntakeKind::PlannerAnswer,
                target: format!("{}:{}", t.key.kind.as_str(), t.key.subject),
                channel: "planner".to_owned(),
                message_id: String::new(),
                received_ms: at_ms,
                // Model answers already scrubbed where the loop stores them;
                // ids, verdicts and statements only.
                body: kgraf_secrets::scrub_persist_owned(body),
            })
            .await
            .ok();
    }
    let done = TaskDone {
        key: t.key,
        answer,
        row,
        slot_wait_us: micros(started.saturating_duration_since(t.spawned)),
        call_us: micros(ended.saturating_duration_since(started)),
        stop_us,
    };
    t.reported.store(true, std::sync::atomic::Ordering::SeqCst);
    match t.reply {
        Reply::Loop(tx) => {
            let _ = tx.send(LoopMsg::Planner(Box::new(done))).await;
        }
        Reply::Inline(tx) => {
            let _ = tx.send(done);
        }
    }
}
