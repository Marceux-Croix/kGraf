//! Scheduled runs start here: the orchestrator's implementation of
//! `kgraf_schedule::RunStarter` (decision 0052: "an orchestrator run started by
//! the clock").
//!
//! Readiness runs first and a readiness failure returns its typed class, so the
//! claim table disables a schedule that cannot succeed as configured and counts a
//! transient failure toward its threshold. Concurrent scheduled runs
//! are capped; a start over the cap is a transient failure, not a queued surprise.
//! A run that passes starts in the background with a run id derived from the
//! schedule and the due time, so a re-delivered occurrence (claiming is
//! at-least-once) names the same run and the run lease refuses the second start.
//! Every run's outcome, a failure included, is kept until
//! [`ScheduledRuns::take_finished`] collects it: none is dropped with its task.
//!
//! The factory must build every orchestrator on the process's one
//! [`crate::Admission`], so scheduled runs and interactive runs share one gate
//! and one budget.

use crate::orchestrator::{Orchestrator, OrchestratorError, RunReport};
use crate::readiness::ReadinessFailure;
use chrono::{DateTime, Utc};
use kgraf_core::RunId;
use kgraf_schedule::{DisableReason, FailureClass, RunStartOutcome, RunStarter, ScheduleError};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Builds an orchestrator per run (a run consumes its orchestrator) and checks
/// readiness before one is built.
///
/// This splits what used to be one `ready()` check into two,
/// because they answer different questions and a schedule store must treat their
/// failures differently:
/// - [`configured`](Self::configured) is whole-process: can this build run *any*
///   scheduled work at all (are routes even set up)? A caller (`SchedulerLoop`)
///   checks this once per tick, before claiming anything, so a build with nothing
///   configured never claims an occurrence just to disable it for a fact true of the
///   whole process and not of that schedule.
/// - [`ready`](Self::ready) is per-schedule: given this schedule's id, is *its*
///   credential present, is *its* MCP server reachable? `start_run` checks this
///   after a claim already succeeded, and its failure is what disables *that*
///   schedule with a stated reason (acceptance: "a schedule whose credential is
///   removed disables itself"). The default implementation reports no additional
///   per-schedule check, so an existing factory that only ever had one global
///   readiness fact does not need to add anything to keep building.
#[async_trait::async_trait]
pub trait OrchestratorFactory: Send + Sync {
    /// Whole-process readiness: routes configured, credential plumbing present
    /// at all. Never attributed to one schedule.
    async fn configured(&self) -> Result<(), ReadinessFailure>;
    /// This schedule's own readiness: its credential, its MCP servers. Checked
    /// inside `start_run`, after a claim already succeeded.
    async fn ready(&self, schedule_id: &str) -> Result<(), ReadinessFailure> {
        let _ = schedule_id;
        Ok(())
    }
    /// An orchestrator for `run`; the run id is given so its own model calls are
    /// attributed to that run in the ledger.
    fn build(&self, run: &RunId) -> Result<Orchestrator, OrchestratorError>;
}

/// A finished scheduled run: its report, or why it failed.
pub type Finished = (RunId, Result<RunReport, String>);

/// The `RunStarter` for scheduled runs.
pub struct ScheduledRuns {
    factory: Arc<dyn OrchestratorFactory>,
    /// Schedule id to the instruction it runs, verbatim. A `Mutex`, not a plain
    /// field: [`set_instructions`](Self::set_instructions) lets a caller whose
    /// schedule definitions can change over time replace this after
    /// construction, instead of the map being frozen at whatever it held the moment
    /// this `ScheduledRuns` was built.
    instructions: Mutex<BTreeMap<String, String>>,
    max_concurrent: usize,
    active: Arc<AtomicUsize>,
    finished: Arc<Mutex<Vec<Finished>>>,
}

impl ScheduledRuns {
    pub fn new(
        factory: Arc<dyn OrchestratorFactory>,
        instructions: BTreeMap<String, String>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            factory,
            instructions: Mutex::new(instructions),
            max_concurrent: max_concurrent.max(1),
            active: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The run id for an occurrence.
    pub fn run_id(schedule_id: &str, due_at: DateTime<Utc>) -> RunId {
        RunId::new(crate::plan::sanitize_id(&format!(
            "sched-{schedule_id}-{}",
            due_at.timestamp()
        )))
    }

    /// Scheduled runs still running.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Runs that have ended since the last call, failures included.
    pub fn take_finished(&self) -> Vec<Finished> {
        match self.finished.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(p) => std::mem::take(&mut *p.into_inner()),
        }
    }

    /// Whether this process can run anything at all right now, without claiming or
    /// attributing the answer to any particular schedule. Exposed so a caller (`SchedulerLoop`) can
    /// decide *not to claim in the first place* when the whole build cannot run work - a desktop
    /// shell with no route configuration yet, say - rather than claiming every due occurrence and
    /// then disabling each schedule one at a time for a fact that has nothing to do with any one of
    /// them. See [`OrchestratorFactory::configured`] and [`OrchestratorFactory::ready`] for the
    /// split this and `start_run`'s own check now respect.
    pub async fn configured(&self) -> Result<(), ReadinessFailure> {
        self.factory.configured().await
    }

    /// Replaces the instructions every schedule id runs: the previous shell wiring built one
    /// `ScheduledRuns` with an empty instruction map at the moment a schedule first appeared and
    /// never updated it, so a schedule added, renamed, or edited afterward ran with no instruction
    /// at all (`start_run`'s own "no instruction" `ConfigurationMissing` path). A caller whose
    /// schedule definitions can change over time - the background loop rereading them every tick -
    /// calls this each time they do.
    pub fn set_instructions(&self, instructions: BTreeMap<String, String>) {
        *self.instructions.lock().unwrap_or_else(|p| p.into_inner()) = instructions;
    }
}

#[async_trait::async_trait]
impl RunStarter for ScheduledRuns {
    async fn start_run(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
    ) -> Result<RunStartOutcome, ScheduleError> {
        let Some(instruction) = self
            .instructions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(schedule_id)
            .cloned()
        else {
            return Ok(RunStartOutcome::Failed(FailureClass::Disabling(
                DisableReason::ConfigurationMissing {
                    detail: format!("schedule `{schedule_id}` has no instruction"),
                },
            )));
        };
        // Per-schedule readiness (this schedule's own credential), not the
        // whole-process `configured()` check `SchedulerLoop` already ran before ever
        // claiming this occurrence. A failure here is what disables *this* schedule.
        if let Err(failure) = self.factory.ready(schedule_id).await {
            return Ok(RunStartOutcome::Failed(failure.to_failure_class()));
        }
        // Take a run slot, or refuse: the cap is checked and taken in one step.
        let taken = self
            .active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.max_concurrent).then_some(n + 1)
            });
        if taken.is_err() {
            // A capacity refusal is `Deferred`, not
            // `Transient` - the scheduler being full right now says nothing about
            // this schedule's own configuration, so it must not count toward the
            // consecutive-failure pause threshold the way a real run failure does.
            // `SchedulerLoop::record_deferral` retries it, bounded, on a later tick.
            return Ok(RunStartOutcome::Failed(FailureClass::Deferred {
                detail: format!(
                    "{} scheduled runs are running; the cap is {}",
                    self.active(),
                    self.max_concurrent
                ),
            }));
        }
        let run = Self::run_id(schedule_id, due_at);
        let orchestrator = match self.factory.build(&run) {
            Ok(o) => o,
            Err(e) => {
                self.active.fetch_sub(1, Ordering::SeqCst);
                // A build that can succeed later with the configuration unchanged
                // (an MCP server not answering, a git lock, a full disk) counts
                // toward the pause threshold; it must not disable the schedule for
                // good (failure.rs: a transient failure never disables by itself).
                let class = match e {
                    OrchestratorError::Build {
                        detail,
                        transient: true,
                    } => FailureClass::Transient {
                        detail: format!("the run could not be built: {detail}"),
                    },
                    OrchestratorError::MissingCredential(detail) => {
                        FailureClass::Disabling(DisableReason::MissingCredential { detail })
                    }
                    other => FailureClass::Disabling(DisableReason::ConfigurationMissing {
                        detail: other.to_string(),
                    }),
                };
                return Ok(RunStartOutcome::Failed(class));
            }
        };
        let active = self.active.clone();
        let finished = self.finished.clone();
        // The run in its own task, watched by this one: a panic inside the run
        // still records an outcome and gives its slot back, so the claim is
        // settled and the cap does not shrink for the rest of the process.
        let task = {
            let run = run.clone();
            tokio::spawn(async move { orchestrator.start(run, instruction).await })
        };
        tokio::spawn(async move {
            let outcome = match task.await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(e) if e.is_panic() => Err(format!("the run panicked: {e}")),
                Err(e) => Err(format!("the run's task ended abnormally: {e}")),
            };
            match finished.lock() {
                Ok(mut g) => g.push((run, outcome)),
                Err(p) => p.into_inner().push((run, outcome)),
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(RunStartOutcome::Started)
    }
}
