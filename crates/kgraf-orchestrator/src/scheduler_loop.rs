//! Ties `kgraf_schedule`'s claim table and recurrence engine to
//! [`ScheduledRuns`] (decision 0052).
//!
//! This module is the scheduling *policy* loop: given a set of schedule definitions
//! and one claim table, each tick resolves what is due, claims it, starts it through
//! `ScheduledRuns`, and records what came back. It is not the run engine
//! (`orchestrator.rs`, untouched by this work) and not the claim table
//! (`kgraf_schedule::store`); it is the seam that calls both. Kept out of
//! `orchestrator.rs` so that file's own scope (one run's lifecycle) does not grow a
//! second, unrelated responsibility (many schedules' lifecycles); kept in this crate,
//! not `kgraf-schedule`, because it needs `ScheduledRuns::take_finished`, which
//! only exists here (see `starter.rs`'s own doc for why that crate does not depend on
//! this one).
//!
//! ## Steady state versus wake
//!
//! An earlier version of this module treated *every* tick as a "wake" evaluation:
//! `Schedule::resolve_wake`'s policy (`Skip` or `RunOnceOnWake`) applied uniformly,
//! which meant a `Skip`-policy schedule ticked continuously, right across its own due
//! time, never fired at all - `resolve_wake` under `Skip` only ever returns a
//! catch-up when something was *missed*, and a schedule ticked every few seconds is
//! never missing anything at the moment it becomes due.
//!
//! This version keeps `Schedule::latest_occurrence_since` (policy-neutral: "the
//! latest occurrence in this range, whatever the policy says to do about it") as the
//! single source of "what occurrence, if any, is the candidate", and decides
//! separately whether *this* tick is close enough to real time to count as the
//! schedule's normal, on-time firing:
//!
//! - **On time**: the occurrence's own due time is within
//!   [`SchedulerLoop::on_time_window`] of `now`. The gap is measured from the
//!   occurrence, not from the last tick: a heartbeat-based gap called an
//!   occurrence missed by hours "on time" as long as the loop itself had ticked
//!   recently. The candidate occurrence is searched for after `last_seen`, or after
//!   the schedule's creation time when nothing has been claimed yet. Both policies
//!   fire.
//! - **Not on time** (a real gap: the app was closed, a tick was skipped for far
//!   longer than the window): `RunOnceOnWake` still fires (its whole purpose);
//!   `Skip` drops it.
//!
//! ## Steady state is atomic with the claim
//!
//! `ScheduleStore::tick_schedule` reads `last_seen`, lets this module decide what (if
//! anything) is due, and - only if it decided to act - advances `last_seen` to
//! exactly that occurrence and claims it, all inside one transaction. The previous
//! design advanced `last_seen` to `now` unconditionally, before claiming, as a
//! separate statement: a crash in between stranded the occurrence (nobody had
//! claimed it, but `last_seen` already said it was accounted for) with no path back
//! to it, and left `claim_occurrence`'s own lease-expiry reclaim logic unreachable,
//! since nothing ever asked to claim that occurrence again. See `tick_schedule`'s own
//! doc for the full argument, including the boundary race this same transaction
//! closes.
//!
//! ## A schedule's first-ever evaluation is not "nothing missed"
//!
//! `Schedule::resolve_wake`'s `None` branch ("never evaluated, nothing missed") is
//! right for a schedule that has genuinely never had an occurrence. It is wrong for
//! one whose first occurrence already passed between creation and the first tick (a
//! one-shot ten seconds out, ticked every twenty; a daily schedule added while the
//! app was closed). `TrackedSchedule::created_at` is the fallback baseline for
//! exactly that case.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use kgraf_core::RunId;
use kgraf_schedule::{
    FailureClass, MissedRunPolicy, RunOutcome, RunStartOutcome, RunStarter, Schedule,
    ScheduleError, ScheduleStore, TickDecision,
};

use crate::run_state::RunStatus;
use crate::starter::ScheduledRuns;

/// A schedule as the loop sees it: its firing pattern, whether it is enabled, and
/// when it was created.
///
/// Persistence (decision 0052: "a schedule is a property of a work node, drawn on
/// the graph") is the caller's concern; this loop only ever reads the `Vec` handed to
/// [`SchedulerLoop::tick`], so a schedule disabled or edited in storage since the
/// previous tick is picked up immediately, with no cache to invalidate.
#[derive(Clone)]
pub struct TrackedSchedule {
    pub schedule: Schedule,
    pub enabled: bool,
    /// When this schedule was added. Used as the fallback baseline for
    /// `last_seen` on this schedule's first-ever evaluation; see the module doc.
    pub created_at: DateTime<Utc>,
}

/// One claimed-but-not-yet-settled occurrence, kept until
/// [`ScheduledRuns::take_finished`] reports it.
struct Pending {
    schedule_id: String,
    due_at: DateTime<Utc>,
    claimant: String,
    generation: i64,
}

/// What happened during one [`SchedulerLoop::tick`], for a caller to log or show
/// (decision 0052: schedule state and failures are visible, never silent, so a caller surfaces this
/// as configuration/status, not a second work-item list).
#[derive(Debug, Clone, PartialEq)]
pub enum TickEvent {
    /// An occurrence was claimed and handed to `ScheduledRuns`; it is running.
    Started {
        schedule_id: String,
        due_at: DateTime<Utc>,
    },
    /// An occurrence whose previous claimant's lease had expired (a crash or a hang)
    /// was reclaimed and is running again ("each tick reclaim claimed rows whose
    /// lease expired, emitting a visible abandoned event" - this is that event,
    /// distinct from `Started` so a repeated reclaim of the same occurrence is
    /// noticeable, not indistinguishable from ordinary firings).
    Reclaimed {
        schedule_id: String,
        due_at: DateTime<Utc>,
    },
    /// A previously started occurrence finished successfully.
    Completed {
        schedule_id: String,
        due_at: DateTime<Utc>,
    },
    /// An occurrence did not succeed: a readiness failure, a run failure, or a
    /// capacity refusal. `disabled` is `true` exactly when this failure disabled the
    /// schedule outright (decision 0052's typed failures), as opposed to merely
    /// counting toward its consecutive-failure threshold.
    Failed {
        schedule_id: String,
        due_at: DateTime<Utc>,
        detail: String,
        disabled: bool,
    },
    /// A schedule was skipped this tick because it is already disabled; `reason` is
    /// the stored disable reason, shown as-is (never a generic message).
    Disabled { schedule_id: String, reason: String },
    /// `last_seen` was found ahead of `now` (a forward system-clock jump, later
    /// corrected, or a hand-edited claim table) and was clamped back to `now`.
    /// Visible rather than silently absorbed, because a clock anomaly is itself worth
    /// knowing about even though this loop recovers from it on its own.
    ClockAnomaly { schedule_id: String, detail: String },
    /// This process cannot run any scheduled work at all right now - a desktop build with no route
    /// configuration yet, say. Reported once per enabled schedule without ever attempting to claim
    /// its occurrence: the previous behavior claimed anyway and let the readiness failure disable
    /// each schedule one at a time, which meant an environment fact unrelated to any one schedule's
    /// own health permanently paused every schedule in the file, with no path back short of `kgraf
    /// schedule enable` for each of them once the environment fact stopped being true.
    NotRunnable { schedule_id: String, detail: String },
    /// A capacity refusal: the scheduler is over its
    /// own concurrency cap right now. Distinct from `Failed` because it is not one -
    /// nothing about this schedule is wrong, and it never counts toward the
    /// consecutive-failure pause threshold - the occurrence is deferred, bounded, to
    /// a later tick; `deferred_count` is how many times so far.
    Deferred {
        schedule_id: String,
        due_at: DateTime<Utc>,
        detail: String,
        deferred_count: u32,
    },
    /// A deferred occurrence that finally started. Distinct from
    /// `Deferred`, which means "claimed and not started": a caller recording run
    /// history must see this as a start, with the number of deferrals before it.
    /// Never reported as `Reclaimed`: the occurrence was capacity-limited, not
    /// abandoned.
    StartedAfterDeferral {
        schedule_id: String,
        due_at: DateTime<Utc>,
        deferred_count: u32,
    },
    /// This instance's own lease on a still-
    /// pending occurrence had already expired (typically a suspend longer than the
    /// lease) by the time this tick ran, and re-claiming it under the same claimant
    /// failed too - someone else has already reclaimed it. This occurrence's own
    /// eventual result, whenever it arrives, is no longer this instance's to record
    /// (its `record_outcome` call will be fenced off as stale, harmlessly). Distinct
    /// from `Failed`: nothing is known to be wrong with the run itself, only with
    /// this instance's claim on it.
    LeaseLost {
        schedule_id: String,
        due_at: DateTime<Utc>,
    },
    /// A capacity refusal, deferred past a
    /// reasonable bound (the on-time window under `MissedRunPolicy::Skip`, or a
    /// later occurrence of the same schedule already being due), was dropped rather
    /// than run late or counted as a failure - the same "missed means dropped, not
    /// late" rule `Skip` already applies to an occurrence that was never claimed at
    /// all.
    Dropped {
        schedule_id: String,
        due_at: DateTime<Utc>,
        detail: String,
    },
    /// The run id `start_run` actually
    /// started could not be recorded against its occurrence (a store error at
    /// exactly the wrong moment). The run itself is unaffected and keeps running;
    /// only this occurrence's spend attribution in `schedule list` is at risk of
    /// being reported as unattributed rather than under its own schedule - visible,
    /// rather than the previous version's silently ignored error.
    RunIdNotRecorded {
        schedule_id: String,
        due_at: DateTime<Utc>,
        detail: String,
    },
}

/// How long a claim is held before another instance may reclaim it as abandoned.
/// Generous relative to a worker round, so a live run's own slowness is never read as
/// a crash: decision 0052's fencing is what makes a too-generous lease here safe (see
/// `kgraf_schedule::store`'s module doc), not a lease cut close to the wire.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(15 * 60);

/// How close to real time a tick must be for its occurrence to count as "on time"
/// rather than "missed" - generous relative to any sane tick cadence (the
/// desktop shell ticks every 20s; a caller with a coarser cadence should call
/// [`SchedulerLoop::with_on_time_window`] with its own interval plus grace).
pub const DEFAULT_ON_TIME_WINDOW: TimeDelta = TimeDelta::minutes(2);

/// Default wall time between lease renewals inside one tick
/// ([`SchedulerLoop::with_renew_every`]): a minute, far inside [`DEFAULT_LEASE`].
pub const DEFAULT_RENEW_EVERY: Duration = Duration::from_secs(60);

/// Drives any number of schedules against one claim table and one [`ScheduledRuns`].
///
/// One `SchedulerLoop` per process. `claimant` identifies this process's claims in
/// the shared claim table, so two app instances (or a CLI `run-now` invocation and
/// the background app) pointed at the same [`ScheduleStore`] file race safely: the
/// store's own fencing decides the winner, not this loop (acceptance: "a schedule
/// fires once across two app instances").
///
/// `store` is `Arc<Mutex<_>>`, not a bare `Arc`: `ScheduleStore` wraps a
/// `rusqlite::Connection`, which is `Send` but not `Sync` (its own module doc says
/// "one `ScheduleStore` per process/thread"), so sharing one across an async
/// runtime's worker threads needs the mutex - `tick`'s own await points (`start_run`)
/// mean this method can resume on a different thread than it started on even with a
/// single logical caller.
pub struct SchedulerLoop {
    store: Arc<Mutex<ScheduleStore>>,
    starter: Arc<ScheduledRuns>,
    claimant: String,
    lease: Duration,
    on_time_window: TimeDelta,
    /// Wall time between lease renewals inside one tick ([`Self::with_renew_every`]).
    renew_every: Duration,
    pending: BTreeMap<RunId, Pending>,
}

impl SchedulerLoop {
    pub fn new(
        store: Arc<Mutex<ScheduleStore>>,
        starter: Arc<ScheduledRuns>,
        claimant: impl Into<String>,
    ) -> Self {
        Self {
            store,
            starter,
            claimant: claimant.into(),
            lease: DEFAULT_LEASE,
            on_time_window: DEFAULT_ON_TIME_WINDOW,
            renew_every: DEFAULT_RENEW_EVERY,
            pending: BTreeMap::new(),
        }
    }

    /// How much wall time the starts of one tick may take before the pending
    /// runs' leases are renewed again, between two starts (default
    /// [`DEFAULT_RENEW_EVERY`]). A start builds a run (git, route probes, MCP
    /// servers), so a tick with several due schedules can take minutes; without
    /// this, a pending run's lease is renewed only at the start of the tick.
    pub fn with_renew_every(mut self, every: Duration) -> Self {
        self.renew_every = every;
        self
    }

    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// Sets the on-time window. A caller ticking on a real interval should pass
    /// that interval plus some grace (a slow tick, GC pause, or similar must not be
    /// misread as "the app was closed").
    pub fn with_on_time_window(mut self, window: TimeDelta) -> Self {
        self.on_time_window = window;
        self
    }

    /// Occurrences still awaiting `ScheduledRuns::take_finished`, for tests and
    /// diagnostics.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// One evaluation pass at `now`. `now` is a parameter, not the wall clock, so a
    /// test drives time exactly; a caller looping in real time passes `Utc::now()` on
    /// an interval it owns - this module has no timer of its own.
    ///
    /// Never returns an error: a failure evaluating or starting *one* schedule (a
    /// pathological recurrence rule, a transient store error) becomes that
    /// schedule's own `TickEvent::Failed` and the tick continues with the rest.
    /// The previous version used `?` throughout the per-schedule loop body, so one
    /// bad schedule - a `croner` pattern whose backward search failed, a stale
    /// pattern imported from an old format - silently starved every other schedule
    /// in the same tick, forever, since the failing one is evaluated first every
    /// time.
    pub async fn tick(
        &mut self,
        schedules: &[TrackedSchedule],
        now: DateTime<Utc>,
    ) -> Vec<TickEvent> {
        let mut events = self.drain_finished();

        // Heartbeat every still-pending run's lease before it can expire. A
        // run that takes longer than `DEFAULT_LEASE` (a real worker round can) must
        // never look abandoned to `expired_claims` just because nobody told the claim
        // table it was still alive - and if the lease had already lapsed by the time
        // this tick ran at all (a suspend longer than the lease, D1), re-claiming it
        // under the same claimant here, before the expired-claims scan below ever
        // runs, is what stops this instance's own still-live run from being reclaimed
        // out from under itself.
        events.extend(self.renew_pending_leases(now));

        // A process-wide readiness check, before any schedule is even looked at.
        // A schedule is never claimed, and so never disabled, for a fact that is true
        // of the whole build and not of it in particular. A separate check splits this
        // (`configured`) from the per-schedule check `start_run` runs internally
        // (`ready(schedule_id)`) - see `OrchestratorFactory`'s own doc.
        if let Err(failure) = self.starter.configured().await {
            let detail = failure.to_string();
            for tracked in schedules {
                if tracked.enabled {
                    events.push(TickEvent::NotRunnable {
                        schedule_id: tracked.schedule.id.clone(),
                        detail: detail.clone(),
                    });
                }
            }
            return events;
        }

        // Starts can be slow (each builds a run), so the leases of pending runs
        // are renewed again between starts once `renew_every` of wall time has
        // passed, at `now` advanced by that wall time: a long tick never lets a
        // live run's lease lapse.
        let started = std::time::Instant::now();
        let mut renewed = started;
        for tracked in schedules {
            if !tracked.enabled {
                continue;
            }
            match self.tick_one(tracked, now).await {
                Ok(event) => events.extend(event),
                Err(err) => events.push(TickEvent::Failed {
                    schedule_id: tracked.schedule.id.clone(),
                    due_at: now,
                    detail: err.to_string(),
                    disabled: false,
                }),
            }
            if renewed.elapsed() >= self.renew_every {
                let elapsed = TimeDelta::from_std(started.elapsed()).unwrap_or(TimeDelta::zero());
                events.extend(self.renew_pending_leases(now + elapsed));
                renewed = std::time::Instant::now();
            }
        }
        events.extend(self.drain_finished());
        events
    }

    /// One schedule's share of a tick. Isolated by `tick`'s `match` above: any
    /// `?` in here only ever aborts *this* schedule's own turn.
    async fn tick_one(
        &mut self,
        tracked: &TrackedSchedule,
        now: DateTime<Utc>,
    ) -> Result<Vec<TickEvent>, ScheduleError> {
        let id = tracked.schedule.id.clone();
        let mut events = Vec::new();

        let status = self.lock_store().schedule_status(&id)?;
        if let Some(reason) = status.disabled_reason {
            events.push(TickEvent::Disabled {
                schedule_id: id,
                reason,
            });
            return Ok(events);
        }

        // A `last_seen` ahead of `now` (a forward clock jump, later corrected)
        // would otherwise wedge this schedule - `latest_occurrence_since` never finds
        // anything past a baseline that is itself in the future. Corrected, visibly,
        // before the normal decision below (which then sees the corrected value:
        // `clamp_last_seen` and `tick_schedule` are two statements, not one
        // transaction, but a clock anomaly is an environment fault, not a
        // concurrency hazard this loop needs to fence against).
        if self.lock_store().clamp_last_seen(&id, now)? {
            events.push(TickEvent::ClockAnomaly {
                schedule_id: id.clone(),
                detail: format!(
                    "last_seen was ahead of {now}; clamped back (a forward clock jump?)"
                ),
            });
        }

        // Reclaim occurrences whose lease already expired *before* evaluating
        // what is newly due. This must be a separate scan, not folded into
        // `decide` below: once `tick_schedule` commits a claim, `last_seen` already
        // names that occurrence (the claim's own atomicity), so `latest_occurrence_since`
        // can never propose it again to `decide` - there is no occurrence *later*
        // than `last_seen` to find. `expired_claims` looks at the claim table's
        // per-occurrence status directly, independent of `last_seen`.
        //
        // An occurrence this instance still
        // has in `self.pending` is excluded here even though `renew_pending_leases`
        // (run earlier in `tick`) is what actually keeps its lease fresh - belt and
        // suspenders: if renewal's own self-heal somehow left a pending entry's lease
        // stale (it should not, but this scan is the one place a genuinely abandoned
        // occurrence is found, and a bug in renewal must never turn into this
        // instance reclaiming - and thus re-`start_run`-ing - its own still-live run).
        let expired_list = self.lock_store().expired_claims(&id, now)?;
        for expired in expired_list {
            if self.is_pending(&id, expired.due_at) {
                continue;
            }
            // An occurrence set aside for a manual run skipped the policy check
            // when it fell due. Under `Skip`, one that is now outside the on-time
            // window is a missed run: dropped, never run late.
            if expired.held_for_manual_run
                && tracked.schedule.missed_run_policy == MissedRunPolicy::Skip
                && now.signed_duration_since(expired.due_at) > self.on_time_window
            {
                self.lock_store()
                    .drop_held_for_manual_run(&id, expired.due_at, now)?;
                events.push(TickEvent::Dropped {
                    schedule_id: id.clone(),
                    due_at: expired.due_at,
                    detail: "held for a manual run, and past the on-time window under Skip \
                             when it came back; dropped, not run late"
                        .to_owned(),
                });
                continue;
            }
            let claim = self.lock_store().claim_occurrence(
                &id,
                expired.due_at,
                &self.claimant,
                now,
                self.lease,
            )?;
            let kgraf_schedule::ClaimOutcome::Claimed { generation, .. } = claim else {
                // Someone else reclaimed it first, or it settled between the scan
                // and this attempt.
                continue;
            };
            events.extend(
                self.start_and_track(
                    tracked,
                    expired.due_at,
                    generation,
                    true,
                    expired.deferred_count,
                    now,
                )
                .await?,
            );
        }

        // On-time is judged from the occurrence's own due time, not from when
        // this schedule was last ticked - a heartbeat-based measure (an earlier
        // version of this module) reported "on time" for an occurrence that was
        // actually missed by hours, as long as the loop itself had ticked recently.
        let schedule = &tracked.schedule;
        let created_at = tracked.created_at;
        let policy = schedule.missed_run_policy;
        let on_time_window = self.on_time_window;
        let decide = move |last_seen: Option<DateTime<Utc>>| -> Result<Option<(DateTime<Utc>, bool)>, ScheduleError> {
            let baseline = last_seen.unwrap_or(created_at);
            let Some(due) = schedule.latest_occurrence_since(baseline, now)? else {
                return Ok(None);
            };
            let on_time = now.signed_duration_since(due) <= on_time_window;
            let should_claim = on_time || policy == MissedRunPolicy::RunOnceOnWake;
            Ok(Some((due, should_claim)))
        };

        let decision =
            self.lock_store()
                .tick_schedule(&id, now, &self.claimant, self.lease, decide)?;
        match decision {
            TickDecision::NothingDue | TickDecision::Dropped { .. } => {}
            TickDecision::Evaluated { due, claim } => {
                if let kgraf_schedule::ClaimOutcome::Claimed { generation, .. } = claim {
                    events.extend(
                        self.start_and_track(tracked, due, generation, false, 0, now)
                            .await?,
                    );
                }
                // `ClaimOutcome::NotClaimed`: another instance holds a still-live
                // claim on it; nothing for this tick to do.
            }
        }
        Ok(events)
    }

    /// Claims `generation`'s worth of `(schedule_id, due)` and hands it to
    /// `ScheduledRuns`, tracking it in `self.pending` on success. `found_via_expired_scan`
    /// is true when the caller found this occurrence through `expired_claims`, not
    /// through the normal `decide`/`tick_schedule` path; `deferred_count` (from that
    /// same scan, `0` when not applicable) tells a genuine crash/hang reclaim
    /// (`deferred_count == 0`) apart from a capacity-deferral retry
    /// (`deferred_count > 0`) - the two are reported as different
    /// `TickEvent`s (`Reclaimed` vs `Deferred`) even when the retry succeeds, since a
    /// deferral's own story this run is "it was deferred, then it started", not
    /// "it crashed, then someone reclaimed it".
    async fn start_and_track(
        &mut self,
        tracked: &TrackedSchedule,
        due: DateTime<Utc>,
        generation: i64,
        found_via_expired_scan: bool,
        deferred_count: u32,
        now: DateTime<Utc>,
    ) -> Result<Vec<TickEvent>, ScheduleError> {
        let schedule_id = tracked.schedule.id.as_str();
        let is_reclaim = found_via_expired_scan || generation > 1;
        match self.starter.start_run(schedule_id, due).await? {
            RunStartOutcome::Started => {
                let mut events = Vec::new();
                let run_id = ScheduledRuns::run_id(schedule_id, due);
                // Attach the run id this claim actually started, so a caller
                // (the CLI's `schedule list`) can look up its spend by exact run id
                // instead of guessing one from `schedule_id` and `due` itself. The
                // run above has already started either way, so a failure to record
                // this is never fatal to the tick - but it must be visible (an ignored error would
                // hide it), since it means this occurrence's spend will not be found under its own
                // schedule.
                if let Err(e) = self.lock_store().set_run_id(
                    schedule_id,
                    due,
                    &self.claimant,
                    generation,
                    run_id.as_str(),
                ) {
                    events.push(TickEvent::RunIdNotRecorded {
                        schedule_id: schedule_id.to_owned(),
                        due_at: due,
                        detail: e.to_string(),
                    });
                }
                self.pending.insert(
                    run_id,
                    Pending {
                        schedule_id: schedule_id.to_owned(),
                        due_at: due,
                        claimant: self.claimant.clone(),
                        generation,
                    },
                );
                // A retry that finally succeeds is its own event, never
                // `Reclaimed` - it was not abandoned, it was capacity-limited - and
                // never `Deferred`, which means not started.
                events.push(if deferred_count > 0 {
                    TickEvent::StartedAfterDeferral {
                        schedule_id: schedule_id.to_owned(),
                        due_at: due,
                        deferred_count,
                    }
                } else if is_reclaim {
                    TickEvent::Reclaimed {
                        schedule_id: schedule_id.to_owned(),
                        due_at: due,
                    }
                } else {
                    TickEvent::Started {
                        schedule_id: schedule_id.to_owned(),
                        due_at: due,
                    }
                });
                Ok(events)
            }
            // A capacity refusal never finalizes the occurrence through
            // `record_outcome` (which would count it toward the pause threshold like
            // any other failure) while it is still worth retrying. `decide_deferral`
            // answers, from this schedule's own policy and timing - not a fixed tick
            // count - whether it still is.
            RunStartOutcome::Failed(FailureClass::Deferred { detail }) => {
                match self.decide_deferral(tracked, due, now) {
                    DeferralDecision::Drop => {
                        self.lock_store().drop_deferred(
                            schedule_id,
                            due,
                            &self.claimant,
                            generation,
                        )?;
                        Ok(vec![TickEvent::Dropped {
                            schedule_id: schedule_id.to_owned(),
                            due_at: due,
                            detail: format!(
                                "{detail} (past the on-time window under Skip; dropped, not run late)"
                            ),
                        }])
                    }
                    decision @ (DeferralDecision::KeepDeferring | DeferralDecision::GiveUp) => {
                        let give_up = matches!(decision, DeferralDecision::GiveUp);
                        match self.lock_store().record_deferral(
                            schedule_id,
                            due,
                            &self.claimant,
                            generation,
                            now,
                            give_up,
                        )? {
                            kgraf_schedule::DeferralOutcome::Deferred { deferred_count } => {
                                Ok(vec![TickEvent::Deferred {
                                    schedule_id: schedule_id.to_owned(),
                                    due_at: due,
                                    detail,
                                    deferred_count,
                                }])
                            }
                            kgraf_schedule::DeferralOutcome::Exhausted { deferred_count } => {
                                Ok(vec![TickEvent::Failed {
                                    schedule_id: schedule_id.to_owned(),
                                    due_at: due,
                                    detail: format!(
                                        "{detail} (deferred {deferred_count} time(s); a later occurrence is already due; giving up)"
                                    ),
                                    disabled: false,
                                }])
                            }
                        }
                    }
                }
            }
            RunStartOutcome::Failed(class) => {
                let disabled = class.disable_reason().is_some();
                let detail = describe_failure(&class);
                self.lock_store().record_outcome(
                    schedule_id,
                    due,
                    &self.claimant,
                    generation,
                    &RunOutcome::Failed(class),
                )?;
                Ok(vec![TickEvent::Failed {
                    schedule_id: schedule_id.to_owned(),
                    due_at: due,
                    detail,
                    disabled,
                }])
            }
        }
    }

    /// Whether a still-refused capacity deferral should keep being retried, be given
    /// up on (finalized as a counted failure), or - under `Skip` past the on-time
    /// window - dropped outright.
    fn decide_deferral(
        &self,
        tracked: &TrackedSchedule,
        due: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> DeferralDecision {
        let on_time = now.signed_duration_since(due) <= self.on_time_window;
        if tracked.schedule.missed_run_policy == MissedRunPolicy::Skip && !on_time {
            return DeferralDecision::Drop;
        }
        // "Until the next occurrence of that schedule": once a later occurrence has
        // already come due, retrying this one further would only ever fall further
        // behind - a one-shot schedule has no later occurrence, ever, so this bound
        // never applies to it; it defers until a slot frees, however long that takes.
        match tracked.schedule.latest_occurrence_since(due, now) {
            Ok(Some(_)) => DeferralDecision::GiveUp,
            Ok(None) => DeferralDecision::KeepDeferring,
            // A recurrence error here is `latest_occurrence_since`'s own concern, not
            // this decision's - the read that already found `due` itself stands, and
            // a caller that later re-evaluates this schedule from scratch will
            // surface the same error as its own `TickEvent::Failed` regardless.
            Err(_) => DeferralDecision::KeepDeferring,
        }
    }

    /// Renews the lease on every occurrence still in `self.pending`: a run that
    /// is genuinely still executing must never look abandoned to `expired_claims`.
    /// `renew_lease` is itself fenced (claimant and generation must still match).
    ///
    /// **A renewal failure is never ignored.**
    /// The first version silently dropped it, which hid a real defect: a suspend
    /// longer than the lease (the desktop sleeping, the process paused) means the
    /// lease has *already* expired by the time this runs, so `renew_lease` itself
    /// refuses (its own fencing requires the lease not to have already lapsed) - and
    /// with the error ignored, this instance's own still-live run looked abandoned to
    /// the very next `expired_claims` scan in the same tick, which reclaimed it and
    /// called `start_run` a *second* time for the same run id. The orchestrator's own
    /// run lease then refused that second start, and *that* refusal - not the real
    /// run's actual result, which was still legitimately in flight - was what got
    /// recorded, silently dropping the real one once it eventually arrived (fenced
    /// off as stale by the newer generation).
    ///
    /// On a renewal failure, this now re-takes the claim under the same claimant
    /// (`claim_occurrence`'s own WHERE clause accepts a claimed-with-an-expired-lease
    /// row from *any* claimant, so re-claiming under the same one heals a lapsed
    /// lease exactly as it would a stranger's): the row's lease is fresh again before
    /// `tick_one`'s own `expired_claims` scan ever runs later in this same tick, and
    /// the pending entry's `generation` is updated so its eventual `record_outcome`
    /// call is fenced against the *new* claim, not the stale one. Only if that
    /// re-claim itself fails - someone else has *already* reclaimed it, a genuine
    /// conflict, not just this instance's own lapsed lease - is the entry dropped
    /// from `self.pending` and a visible `TickEvent::LeaseLost` emitted: this
    /// occurrence's own eventual result is no longer this instance's to record.
    fn renew_pending_leases(&mut self, now: DateTime<Utc>) -> Vec<TickEvent> {
        let mut events = Vec::new();
        let mut lost = Vec::new();
        let lease = self.lease;
        {
            // A direct field lock, not `self.lock_store()`: that method takes `&self`
            // as a whole, which the borrow checker cannot see is disjoint from
            // `self.pending.iter_mut()` below even though the two fields plainly are.
            let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            for (run_id, pending) in self.pending.iter_mut() {
                let renewed = store.renew_lease(
                    &pending.schedule_id,
                    pending.due_at,
                    &pending.claimant,
                    pending.generation,
                    now,
                    lease,
                );
                if renewed.is_ok() {
                    continue;
                }
                match store.claim_occurrence(
                    &pending.schedule_id,
                    pending.due_at,
                    &pending.claimant,
                    now,
                    lease,
                ) {
                    Ok(kgraf_schedule::ClaimOutcome::Claimed { generation, .. }) => {
                        pending.generation = generation;
                    }
                    Ok(kgraf_schedule::ClaimOutcome::NotClaimed) | Err(_) => {
                        events.push(TickEvent::LeaseLost {
                            schedule_id: pending.schedule_id.clone(),
                            due_at: pending.due_at,
                        });
                        lost.push(run_id.clone());
                    }
                }
            }
        }
        for run_id in lost {
            self.pending.remove(&run_id);
        }
        events
    }

    /// Releases every occurrence still genuinely pending after collecting whatever
    /// has already finished.
    ///
    /// **Order matters.** The first version released every pending occurrence
    /// without ever checking whether `ScheduledRuns` had already finished it: a run
    /// that completed just before shutdown but had not yet been drained was released,
    /// handed back as reclaimable, instead of recorded as `done`/`failed`, so its real,
    /// already-known outcome was thrown away and replaced with "abandoned, reclaim
    /// me". Draining first means only what is *still actually running* ever reaches
    /// the release loop below.
    ///
    /// **Status stays `claimed`.** The first version set `status = 'pending'`, which
    /// `expired_claims` (the only scan that ever looks for an abandoned-looking
    /// occurrence) never selects - it only looks at rows still `claimed`. A released
    /// occurrence was reachable by no future tick, on this instance or any other, at
    /// all. `release_claim` now expires the lease instead, so the exact same scan a
    /// crashed claimant's occurrence goes through picks up a released one too.
    ///
    /// Does not wait for the spawned tasks themselves to actually stop - that is
    /// `ScheduledRuns`'s concern, and a run already in flight completing after its
    /// occurrence was released just means its own eventual `record_outcome` call is
    /// fenced off as stale (harmless: whoever reclaimed it in the meantime already
    /// owns it).
    pub fn release_pending(&mut self, now: DateTime<Utc>) -> Vec<TickEvent> {
        let events = self.drain_finished();
        let store = self.lock_store();
        for pending in self.pending.values() {
            let _ = store.release_claim(
                &pending.schedule_id,
                pending.due_at,
                &pending.claimant,
                pending.generation,
                now,
            );
        }
        drop(store);
        self.pending.clear();
        events
    }

    /// Whether `(schedule_id, due)` is one of this instance's own still-pending
    /// occurrences - used to keep the `expired_claims` scan from ever
    /// proposing this instance's own still-live run back to itself as abandoned.
    fn is_pending(&self, schedule_id: &str, due: DateTime<Utc>) -> bool {
        self.pending
            .values()
            .any(|p| p.schedule_id == schedule_id && p.due_at == due)
    }

    /// Collects whatever `ScheduledRuns` has finished since the last call and
    /// records each outcome in the claim table, fenced by the `(claimant,
    /// generation)` pair recorded when it was claimed.
    ///
    /// One run's settlement failing to record (a store error here used to drop
    /// every *other* finished run still waiting in the same batch, via `?`) is now
    /// its own visible `TickEvent::Failed` and the loop moves on to the rest.
    fn drain_finished(&mut self) -> Vec<TickEvent> {
        let mut events = Vec::new();
        for (run_id, result) in self.starter.take_finished() {
            let Some(pending) = self.pending.remove(&run_id) else {
                // Not one of ours (a resumed or otherwise unrelated run through the
                // same `ScheduledRuns`); nothing for the claim table to record.
                continue;
            };
            let outcome = match &result {
                Ok(report) if report.status == RunStatus::Completed => RunOutcome::Success,
                Ok(report) => RunOutcome::Failed(FailureClass::Transient {
                    detail: format!("run ended {:?}, not completed", report.status),
                }),
                Err(e) => RunOutcome::Failed(FailureClass::Transient { detail: e.clone() }),
            };
            let event = match &outcome {
                RunOutcome::Success => TickEvent::Completed {
                    schedule_id: pending.schedule_id.clone(),
                    due_at: pending.due_at,
                },
                RunOutcome::Failed(class) => TickEvent::Failed {
                    schedule_id: pending.schedule_id.clone(),
                    due_at: pending.due_at,
                    detail: describe_failure(class),
                    // A threshold pause from this failure, if any, is visible from
                    // `schedule_status` starting next tick, not asserted here: this
                    // failure alone is never `FailureClass::Disabling` (a run
                    // ending badly is always transient bookkeeping, per decision
                    // 0052; only a readiness failure is typed as disabling).
                    disabled: false,
                },
            };
            let recorded = self.lock_store().record_outcome(
                &pending.schedule_id,
                pending.due_at,
                &pending.claimant,
                pending.generation,
                &outcome,
            );
            match recorded {
                Ok(()) => events.push(event),
                Err(e) => events.push(TickEvent::Failed {
                    schedule_id: pending.schedule_id,
                    due_at: pending.due_at,
                    detail: format!("run settled but could not be recorded: {e}"),
                    disabled: false,
                }),
            }
        }
        events
    }

    /// A short-lived guard on the shared store; never held across an `.await`
    /// (`tick_one`'s own re-borrows are each their own statement, so the lock is
    /// released before `start_run`'s await point).
    fn lock_store(&self) -> std::sync::MutexGuard<'_, ScheduleStore> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn describe_failure(class: &FailureClass) -> String {
    match class {
        FailureClass::Disabling(reason) => reason.to_string(),
        FailureClass::Transient { detail } | FailureClass::Deferred { detail } => detail.clone(),
    }
}

/// What [`SchedulerLoop::decide_deferral`] decided about a
/// still-refused capacity deferral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferralDecision {
    /// Still worth retrying: neither bound has been reached.
    KeepDeferring,
    /// A later occurrence of the same schedule is already due; finalize as a
    /// counted failure rather than fall further behind.
    GiveUp,
    /// `Skip` and past the on-time window: drop it, uncounted, rather than run it
    /// late.
    Drop,
}
