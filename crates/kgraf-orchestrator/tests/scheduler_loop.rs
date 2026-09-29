//! `SchedulerLoop`: the claim table and `ScheduledRuns` driven together (decision
//! 0052). Each test matches one line of the scheduler acceptance list, or one finding
//! from a later review.

mod support;

use chrono::{TimeDelta, TimeZone, Utc};
use kgraf_orchestrator::{
    Orchestrator, OrchestratorError, OrchestratorFactory, ReadinessFailure, ScheduledRuns,
    SchedulerLoop, TickEvent, TrackedSchedule,
};
use kgraf_schedule::{
    ClaimOutcome, DisableReason, FailureClass, MissedRunPolicy, RunOutcome, Schedule,
    ScheduleStore, TickDecision,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

struct Factory {
    parts: Parts,
}

#[async_trait::async_trait]
impl OrchestratorFactory for Factory {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        Ok(())
    }
    fn build(&self, _run: &kgraf_core::RunId) -> Result<Orchestrator, OrchestratorError> {
        Orchestrator::new(config(), self.parts.services(200_000))
    }
}

fn factory(dir: &Path, broken: bool) -> Arc<Factory> {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    Arc::new(Factory {
        parts: Parts::open(
            dir,
            move |prompt| {
                if broken && prompt.contains("BROKEN") {
                    "no plan today".into()
                } else {
                    plan.clone()
                }
            },
            |_| SERVES.into(),
            Duration::from_millis(20),
        ),
    })
}

/// Like `factory`, with a caller-chosen worker delay: a test that needs
/// a run slot to stay genuinely busy across a deterministic span of scheduled time -
/// not just "hopefully longer than however fast the default 20ms fake happens to be"
/// - holds it with a delay chosen far longer than that span.
fn factory_with_delay(dir: &Path, delay: Duration) -> Arc<Factory> {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    Arc::new(Factory {
        parts: Parts::open(dir, move |_| plan.clone(), |_| SERVES.into(), delay),
    })
}

fn scheduled_runs(factory: Arc<Factory>, ids: &[&str]) -> Arc<ScheduledRuns> {
    let instructions = ids
        .iter()
        .map(|id| (id.to_string(), "make a".to_string()))
        .collect();
    Arc::new(ScheduledRuns::new(factory, instructions, 4))
}

fn scheduled_runs_with(factory: Arc<Factory>, pairs: &[(&str, &str)]) -> Arc<ScheduledRuns> {
    let instructions = pairs
        .iter()
        .map(|(id, instr)| (id.to_string(), instr.to_string()))
        .collect();
    Arc::new(ScheduledRuns::new(factory, instructions, 4))
}

/// Like `scheduled_runs`, with an explicit concurrency cap (N7: low enough to force a
/// capacity refusal deterministically, rather than one this test would otherwise
/// never actually exercise).
fn scheduled_runs_capped(
    factory: Arc<Factory>,
    ids: &[&str],
    max_concurrent: usize,
) -> Arc<ScheduledRuns> {
    let instructions = ids
        .iter()
        .map(|id| (id.to_string(), "make a".to_string()))
        .collect();
    Arc::new(ScheduledRuns::new(factory, instructions, max_concurrent))
}

/// A factory whose build is slow for one schedule's runs (a real build probes
/// every route and connects every MCP server).
struct SlowBuild {
    inner: Arc<Factory>,
    slow_for: &'static str,
    delay: Duration,
}

#[async_trait::async_trait]
impl OrchestratorFactory for SlowBuild {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        Ok(())
    }
    fn build(&self, run: &kgraf_core::RunId) -> Result<Orchestrator, OrchestratorError> {
        if run.as_str().contains(self.slow_for) {
            std::thread::sleep(self.delay);
        }
        self.inner.build(run)
    }
}

/// A tick whose starts take long renews the pending runs'
/// leases between starts, so a live run's lease does not lapse behind a slow
/// build. Run `a` is pending (its worker is held for minutes); in the next tick
/// `b`'s build takes 400 ms of wall time. After it, `a`'s lease was renewed at
/// the tick's time advanced by that wall time, so it has not expired just past
/// what the tick-start renewal alone would give.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leases_are_renewed_between_slow_starts() {
    let dir = tempfile::tempdir().expect("dir");
    let slow = Arc::new(SlowBuild {
        inner: factory_with_delay(dir.path(), Duration::from_secs(600)),
        slow_for: "sched-b-",
        delay: Duration::from_millis(400),
    });
    let starter = Arc::new(ScheduledRuns::new(
        slow,
        [("a", "make a"), ("b", "make a")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        4,
    ));
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let lease = Duration::from_secs(60);
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "a")
        .with_lease(lease)
        .with_renew_every(Duration::from_millis(10));
    let due_a = tm(3, 0, 0);
    let due_b = tm(3, 0, 59);
    let schedules = vec![
        tracked(
            Schedule::one_shot("a", due_a, "UTC").expect("valid"),
            tm(2, 59, 50),
        ),
        tracked(
            Schedule::one_shot("b", due_b, "UTC").expect("valid"),
            tm(2, 59, 50),
        ),
    ];
    let first = loop_.tick(&schedules, due_a).await;
    assert!(
        first
            .iter()
            .any(|e| matches!(e, TickEvent::Started { schedule_id, .. } if schedule_id == "a")),
        "{first:?}"
    );
    let second = loop_.tick(&schedules, due_b).await;
    assert!(
        second
            .iter()
            .any(|e| matches!(e, TickEvent::Started { schedule_id, .. } if schedule_id == "b")),
        "{second:?}"
    );
    // The tick-start renewal alone would expire `a` at due_b + lease.
    let just_after =
        due_b + TimeDelta::from_std(lease).expect("lease") + TimeDelta::milliseconds(200);
    let expired = store
        .lock()
        .expect("store")
        .expired_claims("a", just_after)
        .expect("scan");
    assert!(
        expired.is_empty(),
        "a's lease was renewed after b's slow start: {expired:?}"
    );
}

fn tracked(schedule: Schedule, created_at: chrono::DateTime<Utc>) -> TrackedSchedule {
    TrackedSchedule {
        schedule,
        enabled: true,
        created_at,
    }
}

/// Ticks until every claimed occurrence has settled, or gives up after a generous
/// number of attempts (a hung test fails loudly instead of looping forever).
async fn drain(
    loop_: &mut SchedulerLoop,
    schedules: &[TrackedSchedule],
    now: chrono::DateTime<Utc>,
) -> Vec<TickEvent> {
    let mut all = Vec::new();
    for _ in 0..200 {
        let events = loop_.tick(schedules, now).await;
        let done = loop_.pending_count() == 0;
        all.extend(events);
        if done {
            return all;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("scheduled run never settled: {all:?}");
}

fn t(h: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 26, h, 0, 0)
        .single()
        .expect("time")
}

fn tm(h: u32, m: u32, s: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 26, h, m, s)
        .single()
        .expect("time")
}

/// C1: a `Skip`-policy schedule, ticked steadily right across its own due time, must
/// still fire exactly once - the defect this replaces treated every tick as a "wake"
/// evaluation, under which `Skip` (which only ever catches up on a real gap) never
/// fired an on-time occurrence at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skip_policy_schedule_ticked_across_its_due_time_fires_exactly_once() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["skip"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ =
        SchedulerLoop::new(store, starter, "a").with_on_time_window(TimeDelta::seconds(30));

    let schedule = Schedule::recurring("skip", "0 0 * * * *", "UTC")
        .expect("valid")
        .with_missed_run_policy(MissedRunPolicy::Skip);
    let created_at = t(0);
    let schedules = [tracked(schedule, created_at)];

    // Twenty-second ticks from just before the hour to just after it.
    let mut started = 0;
    let mut now = tm(0, 59, 40);
    let end = tm(1, 0, 40);
    while now <= end {
        started += loop_
            .tick(&schedules, now)
            .await
            .iter()
            .filter(|e| matches!(e, TickEvent::Started { .. }))
            .count();
        now += chrono::Duration::seconds(20);
    }
    assert_eq!(
        started, 1,
        "an on-time Skip-policy firing must happen exactly once, not zero and not more than once"
    );
}

/// C2: one schedule failing to progress (here, a claim it cannot win) must not stop
/// another schedule in the same tick from firing. The exact reviewer-cited defect - a
/// cron pattern that constructs but whose backward search fails - is now refused at
/// construction instead (see `kgraf_schedule::recurrence`'s own regression test),
/// so it can no longer reach a tick at all; this test instead proves the tick loop's
/// per-schedule isolation directly, with `tick`'s `match` (not `?`) doing the work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_schedule_unable_to_claim_does_not_stop_another_from_firing() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["blocked", "good"]);
    let store = ScheduleStore::open_in_memory().expect("store");

    let due = t(3);
    // "blocked" is claimed by a foreign instance with a lease that has not expired,
    // so this tick's own attempt on it can never win.
    store
        .claim_occurrence(
            "blocked",
            due,
            "someone-else",
            due,
            Duration::from_secs(900),
        )
        .expect("foreign claim");

    let store = Arc::new(Mutex::new(store));
    let mut loop_ = SchedulerLoop::new(store, starter, "a");
    let blocked = Schedule::one_shot("blocked", due, "UTC").expect("valid");
    let good = Schedule::one_shot("good", due, "UTC").expect("valid");
    let schedules = [tracked(blocked, t(0)), tracked(good, t(0))];

    let events = drain(&mut loop_, &schedules, due).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { schedule_id, .. } if schedule_id == "good")),
        "the good schedule must still fire even though the blocked one made no progress, got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, TickEvent::Failed { schedule_id, .. } if schedule_id == "blocked")
        ),
        "a claim someone else already holds is not this loop's failure to report"
    );
}

/// A capacity refusal must be visible and bounded, and
/// must never count toward the losing schedule's own consecutive-failure pause
/// threshold - it is a fact about the whole scheduler's concurrency, not about
/// whether "second"'s configuration works. It also must not need a full lease to
/// retry: the very next tick reclaims it, the same as a crashed claimant's lease
/// expiring would.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capacity_refusal_defers_visibly_and_never_counts_toward_the_pause_threshold() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    // Only one run slot: "first" takes it, "second" must be refused this tick.
    let starter = scheduled_runs_capped(factory.clone(), &["first", "second"], 1);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "a");
    let due = t(1);
    let first = Schedule::one_shot("first", due, "UTC").expect("valid");
    let second = Schedule::one_shot("second", due, "UTC").expect("valid");
    let schedules = [tracked(first, t(0)), tracked(second, t(0))];

    let events = loop_.tick(&schedules, due).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { schedule_id, .. } if schedule_id == "first")),
        "got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            TickEvent::Deferred { schedule_id, deferred_count: 1, .. } if schedule_id == "second"
        )),
        "the refused schedule must report a visible, counted deferral, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TickEvent::Failed { schedule_id, .. } if schedule_id == "second")),
        "a capacity refusal under the bound is a deferral, not a failure, got {events:?}"
    );
    {
        let store = store.lock().unwrap();
        assert_eq!(
            store
                .schedule_status("second")
                .unwrap()
                .consecutive_failures,
            0,
            "a capacity refusal must never count toward the pause threshold"
        );
    }

    // Let "first" finish and free the one slot, then tick again (a moment later, not
    // a full lease later): "second" must be reclaimed and started on this very next
    // tick.
    drain(&mut loop_, &schedules, due).await;
    let next_tick = due + chrono::TimeDelta::seconds(1);
    let events = loop_.tick(&schedules, next_tick).await;
    // A deferral retry that succeeds is reported as a start after
    // deferral, never `Reclaimed` - "second" was never abandoned, only
    // capacity-limited - and never `Deferred`, which means not started (H4 M1).
    assert!(
        events.iter().any(|e| matches!(
            e,
            TickEvent::StartedAfterDeferral { schedule_id, deferred_count: 1, .. }
                if schedule_id == "second"
        )),
        "a deferred occurrence must be retried on the very next tick, got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, TickEvent::Reclaimed { schedule_id, .. } if schedule_id == "second")
        ),
        "a deferral retry is not a crash reclaim, got {events:?}"
    );
    drain(&mut loop_, &schedules, next_tick).await;
    assert_eq!(
        store
            .lock()
            .unwrap()
            .schedule_status("second")
            .unwrap()
            .consecutive_failures,
        0
    );
}

/// M6: a `false` `enabled` flag must be a real gate, proven against a control that
/// really would fire if enabled - not merely "the disabled one did nothing", which a
/// broken gate that skipped every schedule would also satisfy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disabled_in_definition_schedule_never_fires_while_its_enabled_twin_does() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["off", "on"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store, starter, "a");

    let due = t(3);
    let off = TrackedSchedule {
        schedule: Schedule::one_shot("off", due, "UTC").expect("valid"),
        enabled: false,
        created_at: t(0),
    };
    let on = tracked(Schedule::one_shot("on", due, "UTC").expect("valid"), t(0));
    let schedules = [off, on];

    let events = drain(&mut loop_, &schedules, due).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { schedule_id, .. } if schedule_id == "on")),
        "the enabled control must fire, proving the gate is not simply skipping everything"
    );
    assert!(
        !events.iter().any(|e| matches!(e,
            TickEvent::Started { schedule_id, .. } | TickEvent::Failed { schedule_id, .. }
            if schedule_id == "off"
        )),
        "a schedule with enabled: false must never be evaluated at all, got {events:?}"
    );
}

/// Acceptance: "a recurring schedule fires on time" and "a crash between claim and
/// run does not fire the occurrence twice" - two independent `SchedulerLoop`s (two
/// app instances) racing for real, concurrently, on the same on-disk claim table.
/// M6: the previous version of this test ran the two ticks strictly sequentially (A
/// then B), so B's own tick always saw A's already-advanced `last_seen` and never
/// even attempted `claim_occurrence` - the test would have passed with fencing
/// removed entirely. A `tokio::sync::Barrier` now forces both into their claim
/// attempt at the same instant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_fires_once_across_two_app_instances() {
    let root = tempfile::tempdir().expect("dir");
    let db_path = root.path().join("schedule.sqlite3");
    ScheduleStore::open(&db_path).expect("create the claim table file");

    let due = t(3);
    let created_at = due - chrono::Duration::hours(2);
    let schedule = Schedule::one_shot("nightly", due, "UTC").expect("valid schedule");
    let schedules = Arc::new([tracked(schedule, created_at)]);

    let dir_a = tempfile::tempdir().expect("dir a");
    let dir_b = tempfile::tempdir().expect("dir b");
    let factory_a = factory(dir_a.path(), false);
    let factory_b = factory(dir_b.path(), false);
    let starter_a = scheduled_runs(factory_a.clone(), &["nightly"]);
    let starter_b = scheduled_runs(factory_b.clone(), &["nightly"]);

    let store_a = Arc::new(Mutex::new(ScheduleStore::open(&db_path).expect("open a")));
    let store_b = Arc::new(Mutex::new(ScheduleStore::open(&db_path).expect("open b")));
    let mut loop_a = SchedulerLoop::new(store_a, starter_a, "instance-a");
    let mut loop_b = SchedulerLoop::new(store_b, starter_b, "instance-b");

    // Prime both, well before `due`, so the race below is the schedule's on-time
    // firing rather than either instance's own first-ever-evaluation baseline.
    let prime = due - chrono::Duration::hours(1);
    loop_a.tick(schedules.as_slice(), prime).await;
    loop_b.tick(schedules.as_slice(), prime).await;

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (ba, bb) = (barrier.clone(), barrier.clone());
    let (sa, sb) = (schedules.clone(), schedules.clone());
    let handle_a = tokio::spawn(async move {
        ba.wait().await;
        let events = loop_a.tick(sa.as_slice(), due).await;
        (loop_a, events)
    });
    let handle_b = tokio::spawn(async move {
        bb.wait().await;
        let events = loop_b.tick(sb.as_slice(), due).await;
        (loop_b, events)
    });
    let (mut loop_a, events_a) = handle_a.await.expect("join a");
    let (mut loop_b, events_b) = handle_b.await.expect("join b");

    let started = events_a
        .iter()
        .chain(events_b.iter())
        .filter(|e| matches!(e, TickEvent::Started { .. }))
        .count();
    assert_eq!(
        started, 1,
        "exactly one instance must have claimed and started the run, got a={events_a:?} b={events_b:?}"
    );

    drain(&mut loop_a, schedules.as_slice(), due).await;
    drain(&mut loop_b, schedules.as_slice(), due).await;
    // Exactly one of the two factories did any work at all: the claim table, not
    // this loop, is what decided the winner (see the store's own module doc).
    let a_ran = !factory_a.parts.stats.requests().is_empty();
    let b_ran = !factory_b.parts.stats.requests().is_empty();
    assert!(
        a_ran ^ b_ran,
        "exactly one instance's orchestrator must have run, got a={a_ran} b={b_ran}"
    );
}

/// C3: a claim whose lease expired (the claimant crashed or hung) is reclaimed by a
/// live loop on a later tick, and the reclaim is visible as its own event - not
/// indistinguishable from an ordinary fresh firing, and not permanently stranded (the
/// defect the review found: `advance_last_seen` running ahead of the claim meant a
/// crash between the two left the occurrence accounted for but never actually
/// claimed again by anyone).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_claimants_occurrence_is_reclaimed_and_visibly_marked() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = ScheduleStore::open_in_memory().expect("store");

    let due = t(1);
    // What a claimant that then crashed did: claimed with a short lease, never
    // reported back.
    let claimed = store
        .claim_occurrence("h", due, "crashed", due, Duration::from_secs(30))
        .expect("claim");
    assert!(matches!(
        claimed,
        ClaimOutcome::Claimed { generation: 1, .. }
    ));

    let store = Arc::new(Mutex::new(store));
    let mut loop_ = SchedulerLoop::new(store, starter, "b");
    let schedule = Schedule::recurring("h", "0 0 * * * *", "UTC").expect("valid");
    let schedules = [tracked(schedule, due - chrono::Duration::hours(1))];

    // Fifty minutes later: past the 30s lease.
    let later = due + chrono::Duration::minutes(50);
    let events = drain(&mut loop_, &schedules, later).await;
    assert!(
        events.iter().any(|e| matches!(e, TickEvent::Reclaimed { schedule_id, due_at } if schedule_id == "h" && *due_at == due)),
        "a lease-expired claim must be reclaimed and reported as such, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { .. })),
        "a reclaim is reported as Reclaimed, not conflated with a fresh Started"
    );
}

/// H1: a one-shot due ten seconds after it was created must fire once a tick reaches
/// it, even though it was never "missed" in the `resolve_wake` sense (nothing had
/// ever been claimed for it before). Without `created_at` as the fallback baseline,
/// `latest_occurrence_since(None, now)` compares against `last_seen == None`, and the
/// very first evaluation - whenever it happens to land - always treats the schedule's
/// own due time as "not yet due" relative to nothing, i.e. never catches up to an
/// already-past due time on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_shot_ten_seconds_out_fires_on_a_twenty_second_tick() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["soon"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store, starter, "a");

    let created_at = tm(0, 0, 0);
    let at = created_at + chrono::Duration::seconds(10);
    let schedule = Schedule::one_shot("soon", at, "UTC").expect("valid");
    let schedules = [tracked(schedule, created_at)];

    // No tick before creation; the first tick at all lands twenty seconds later.
    let events = drain(
        &mut loop_,
        &schedules,
        created_at + chrono::Duration::seconds(20),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { .. } | TickEvent::Completed { .. })),
        "a one-shot due 10s after creation must fire on the first 20s tick, got {events:?}"
    );
}

/// H1: a daily schedule added while the app is closed must fire once the app opens,
/// even though the app's *first-ever* evaluation of it happens well after its first
/// intended occurrence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daily_schedule_added_while_closed_fires_when_the_app_opens() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["daily"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store, starter, "a");

    // Added (in the graph store, by the CLI) at 14:00, well after that day's 03:00
    // occurrence; the app does not open - so this schedule is never ticked - until
    // the next afternoon.
    let created_at = tm(14, 0, 0);
    let schedule = Schedule::recurring("daily", "0 0 3 * * *", "UTC").expect("valid");
    let schedules = [tracked(schedule, created_at)];

    let opened = created_at + chrono::Duration::hours(24);
    let events = drain(&mut loop_, &schedules, opened).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { .. } | TickEvent::Completed { .. })),
        "a daily schedule added while closed must fire once the app opens, got {events:?}"
    );
}

/// Acceptance: a missed run policy applied on wake - "run once on wake, not a
/// backlog storm". A run recorded hours ago, then a big forward jump in `now`
/// (simulating the app being closed), fires exactly one catch-up run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missed_run_fires_once_on_start() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["hourly"]);
    let store = ScheduleStore::open_in_memory().expect("store");

    // Simulate a successful run recorded before the app closed.
    let last_run = t(0);
    if let ClaimOutcome::Claimed { generation, .. } = store
        .claim_occurrence(
            "hourly",
            last_run,
            "earlier-instance",
            last_run,
            Duration::from_secs(60),
        )
        .expect("claim")
    {
        store
            .record_outcome(
                "hourly",
                last_run,
                "earlier-instance",
                generation,
                &RunOutcome::Success,
            )
            .expect("record");
    } else {
        panic!("the seed claim must succeed");
    }
    // The earlier instance's own loop would have advanced this on every tick, not
    // only when something was claimed; simulate that directly since this test seeds
    // history without running a real `SchedulerLoop` for it.
    store
        .advance_last_seen("hourly", last_run)
        .expect("advance");

    let schedule = Schedule::recurring("hourly", "0 0 * * * *", "UTC")
        .expect("valid")
        .with_missed_run_policy(MissedRunPolicy::RunOnceOnWake);
    let schedules = [tracked(schedule, t(0))];

    // The app "reopens" six hours later: six occurrences (01:00..06:00) were missed.
    let now = t(6);
    let mut loop_ = SchedulerLoop::new(Arc::new(Mutex::new(store)), starter, "instance-a");
    let events = loop_.tick(&schedules, now).await;
    let started: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TickEvent::Started { .. }))
        .collect();
    assert_eq!(
        started.len(),
        1,
        "exactly one catch-up run must fire, got {events:?}"
    );

    let drained = drain(&mut loop_, &schedules, now).await;
    // A single contract's own worker round-trip (a tool-use round, then a closing
    // round) is more than one provider request; what this asserts is one *run*, not
    // one *request*: exactly one `Completed` settles the one `Started` claimed above,
    // never once per missed hourly tick.
    let completed = drained
        .iter()
        .filter(|e| matches!(e, TickEvent::Completed { .. }))
        .count();
    assert_eq!(
        completed, 1,
        "one missed-run wake must settle exactly one run, got {drained:?}"
    );
}

struct NotRunnable;

#[async_trait::async_trait]
impl OrchestratorFactory for NotRunnable {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        Err(ReadinessFailure::MissingConfiguration(
            "no routes configured in this build".into(),
        ))
    }
    fn build(&self, _run: &kgraf_core::RunId) -> Result<Orchestrator, OrchestratorError> {
        unreachable!("configured() always fails first")
    }
}

/// H2: a whole-process readiness failure (no routes configured at all, say) must
/// never claim an occurrence, and so must never disable any schedule - the previous
/// behavior claimed every due schedule and let each one's own `record_outcome` call
/// disable it individually for a fact that was true of the whole build, not of any
/// one of them, with no path back except `schedule enable`-ing each one by hand once
/// the environment fact changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_process_readiness_failure_never_claims_or_disables_anything() {
    let mut instructions = std::collections::BTreeMap::new();
    instructions.insert("a".to_owned(), "make a".to_owned());
    instructions.insert("b".to_owned(), "make a".to_owned());
    let starter = Arc::new(ScheduledRuns::new(Arc::new(NotRunnable), instructions, 4));
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "instance-a");

    let due = t(3);
    let schedules = [
        tracked(Schedule::one_shot("a", due, "UTC").expect("valid"), t(0)),
        tracked(Schedule::one_shot("b", due, "UTC").expect("valid"), t(0)),
    ];
    let events = loop_.tick(&schedules, due).await;
    assert_eq!(events.len(), 2, "got {events:?}");
    assert!(
        events
            .iter()
            .all(|e| matches!(e, TickEvent::NotRunnable { .. })),
        "got {events:?}"
    );

    let locked = store.lock().expect("lock");
    for id in ["a", "b"] {
        let status = locked.schedule_status(id).expect("status");
        assert!(
            !status.is_disabled(),
            "`{id}` must not be disabled, got {status:?}"
        );
        assert_eq!(
            locked.occurrence_status(id, due).expect("status"),
            None,
            "`{id}`'s occurrence must never have been claimed at all"
        );
    }
}

struct NoCredential;
#[async_trait::async_trait]
impl OrchestratorFactory for NoCredential {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        Ok(())
    }
    async fn ready(&self, _schedule_id: &str) -> Result<(), ReadinessFailure> {
        Err(ReadinessFailure::MissingCredential("KIMI_API_KEY".into()))
    }
    fn build(&self, _run: &kgraf_core::RunId) -> Result<Orchestrator, OrchestratorError> {
        unreachable!("ready() always fails first")
    }
}

/// N3 (port of probe P3): a per-schedule credential failure - `configured()` still
/// succeeds (the process as a whole is set up), but `ready(schedule_id)` fails - must
/// still disable *that* schedule with the reason shown (acceptance item 3: "a
/// schedule whose credential is removed disables itself"). This is the behavior the
/// H2 whole-process pre-check must never swallow: N3 splits `configured` from
/// `ready` precisely so this path stays reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_per_schedule_credential_failure_disables_that_schedule() {
    let mut instructions = std::collections::BTreeMap::new();
    instructions.insert("a".to_owned(), "make a".to_owned());
    let starter = Arc::new(ScheduledRuns::new(Arc::new(NoCredential), instructions, 4));
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "a");
    let schedules = [tracked(
        Schedule::one_shot("a", t(3), "UTC").expect("valid"),
        t(0),
    )];
    for k in 0..5 {
        loop_
            .tick(&schedules, t(3) + chrono::Duration::seconds(20 * k))
            .await;
    }
    let status = store
        .lock()
        .expect("lock")
        .schedule_status("a")
        .expect("status");
    assert!(
        status.is_disabled(),
        "a missing credential must disable the schedule, got {status:?}"
    );
}

/// Acceptance: "a schedule whose credential is removed disables itself" - and once
/// disabled, it never fires again, even when it is still due.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disabled_schedule_never_fires() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["nightly"]);
    let store = ScheduleStore::open_in_memory().expect("store");
    store.register_schedule("nightly").expect("register");
    // Disable it directly, as a readiness failure would (starter.rs already covers
    // that path from a real `OrchestratorFactory::ready` failure).
    if let ClaimOutcome::Claimed { generation, .. } = store
        .claim_occurrence("nightly", t(1), "seed", t(1), Duration::from_secs(60))
        .expect("claim")
    {
        store
            .record_outcome(
                "nightly",
                t(1),
                "seed",
                generation,
                &RunOutcome::Failed(FailureClass::Disabling(DisableReason::MissingCredential {
                    detail: "KIMI_API_KEY".into(),
                })),
            )
            .expect("record");
    }

    let schedule = Schedule::one_shot("nightly", t(3), "UTC").expect("valid");
    let schedules = [tracked(schedule, t(0))];

    let events = SchedulerLoop::new(Arc::new(Mutex::new(store)), starter, "instance-a")
        .tick(&schedules, t(3))
        .await;
    assert!(
        events.iter().any(
            |e| matches!(e, TickEvent::Disabled { reason, .. } if reason.contains("KIMI_API_KEY"))
        ),
        "the disable reason must be visible on the tick event, got {events:?}"
    );
    assert!(
        factory.parts.stats.requests().is_empty(),
        "a disabled schedule must never dispatch"
    );
}

/// Acceptance (decision 0052): a run's failure is visible, not silently
/// swallowed by the loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_run_is_visible_and_counted() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), true);
    let starter = scheduled_runs_with(factory.clone(), &[("broken", "BROKEN make a")]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));

    let schedule = Schedule::one_shot("broken", t(1), "UTC").expect("valid");
    let schedules = [tracked(schedule, t(0))];

    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "instance-a");
    loop_.tick(&schedules, t(0)).await;
    let events = drain(&mut loop_, &schedules, t(1)).await;
    assert!(
        events.iter().any(|e| matches!(e, TickEvent::Failed { .. })),
        "a failed run must produce a visible TickEvent::Failed, got {events:?}"
    );
    let status = store
        .lock()
        .expect("lock")
        .schedule_status("broken")
        .expect("status");
    assert_eq!(
        status.consecutive_failures, 1,
        "the failure must be counted"
    );
}

// ---------------- ported adversarial probes ----------------

/// Port of probe P1: a `Skip`-policy schedule after a real gap (the app closed from
/// 00:50 to 04:30) must drop the missed occurrence and never fire it, on the tick
/// that first sees it *or* any later one - not "drop on the first tick after wake,
/// then fire it on the second because last_seen never advanced" (the exact defect a
/// heartbeat-based on-time measure produced: N1's fix computes on-time from `now -
/// due`, so a missed occurrence stays "not on time" no matter how many ticks pass).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_p1_skip_after_wake_never_fires_the_missed_occurrence() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["skip"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ =
        SchedulerLoop::new(store, starter, "a").with_on_time_window(TimeDelta::seconds(40));
    let schedule = Schedule::recurring("skip", "0 0 * * * *", "UTC")
        .expect("valid")
        .with_missed_run_policy(MissedRunPolicy::Skip);
    let schedules = [tracked(schedule, t(0))];
    loop_.tick(&schedules, tm(0, 50, 0)).await;
    let mut started = Vec::new();
    for s in [0, 20, 40] {
        let ev = loop_.tick(&schedules, tm(4, 30, s)).await;
        started.extend(
            ev.into_iter()
                .filter(|e| matches!(e, TickEvent::Started { .. })),
        );
    }
    assert!(
        started.is_empty(),
        "Skip must drop a missed occurrence permanently, got {started:?}"
    );
}

/// Port of probe P2: the real crash-after-commit case, driven through
/// `tick_schedule` itself (not a hand-seeded claim row), so `last_seen` and the claim
/// really do commit together the way a live loop's first tick would leave them. A
/// second loop, waking fifty minutes later (the 30s lease long expired), must reclaim
/// it and report `TickEvent::Reclaimed` - the defect N2 fixes: with the claim table's
/// old behavior, `last_seen` already named `due`, so nothing ever proposed it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_p2_a_claim_committed_by_tick_schedule_then_crash_is_reclaimed() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = ScheduleStore::open_in_memory().expect("store");
    let due = t(1);
    store
        .tick_schedule("h", due, "crashed", Duration::from_secs(30), |_| {
            Ok(Some((due, true)))
        })
        .expect("tick");
    let store = Arc::new(Mutex::new(store));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "b");
    let schedule = Schedule::recurring("h", "0 0 * * * *", "UTC").expect("valid");
    let schedules = [tracked(schedule, t(0))];
    let events = drain(&mut loop_, &schedules, due + chrono::Duration::minutes(50)).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Reclaimed { .. })),
        "never reclaimed, got {events:?}"
    );
}

/// N2: a run that is genuinely still executing longer than the default lease must
/// never be reclaimed out from under itself - the lease heartbeat
/// (`renew_pending_leases`) is what this test proves exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_live_run_is_never_reclaimed_out_from_under_itself() {
    let dir = tempfile::tempdir().expect("dir");
    // The worker fake's own delay stands in for "longer than one lease": shorter
    // than the test's patience, but this test uses a short lease (`with_lease`) so a
    // delay of a couple of hundred milliseconds already exceeds several leases many
    // times over while the run is still in flight.
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store, starter, "a").with_lease(Duration::from_millis(50));
    let due = t(1);
    let schedule = Schedule::one_shot("h", due, "UTC").expect("valid");
    let schedules = [tracked(schedule, t(0))];

    let started = loop_.tick(&schedules, due).await;
    assert!(
        started
            .iter()
            .any(|e| matches!(e, TickEvent::Started { .. })),
        "got {started:?}"
    );
    // Tick repeatedly, well past several 50ms leases, while the run is still active;
    // the heartbeat must keep it from ever looking abandoned.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let events = loop_.tick(&schedules, due).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, TickEvent::Reclaimed { .. })),
            "a still-running run must never be reclaimed, got {events:?}"
        );
    }
}

/// Releasing pending occurrences (a
/// graceful shutdown) makes each one reclaimable on the very next tick rather than
/// waiting out the whole lease. D2 corrected what that release actually does: the
/// first version set status back to `pending`, which the N2 `expired_claims` scan -
/// the only scan that ever looks for an abandoned-looking occurrence - never
/// selects; a released occurrence was findable by no future tick at all. Status now
/// stays `claimed` with an already-expired lease, so the same scan a crashed
/// claimant's occurrence goes through picks up a released one too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_pending_lets_a_new_loop_reclaim_it_on_the_next_tick() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ =
        SchedulerLoop::new(store.clone(), starter, "a").with_lease(Duration::from_secs(900));
    let due = t(1);
    let schedule = Schedule::one_shot("h", due, "UTC").expect("valid");
    let schedules = [tracked(schedule, t(0))];

    let started = loop_.tick(&schedules, due).await;
    assert!(
        started
            .iter()
            .any(|e| matches!(e, TickEvent::Started { .. }))
    );
    assert_eq!(loop_.pending_count(), 1);

    loop_.release_pending(due);
    assert_eq!(loop_.pending_count(), 0);
    assert_eq!(
        store
            .lock()
            .expect("lock")
            .occurrence_status("h", due)
            .expect("status"),
        Some("claimed".to_owned()),
        "status must stay `claimed`; a `pending` row is invisible to expired_claims"
    );

    let starter2 = scheduled_runs(factory, &["h"]);
    let mut loop2 = SchedulerLoop::new(store.clone(), starter2, "b");
    let next_tick = due + chrono::TimeDelta::seconds(1);
    let events = drain(&mut loop2, &schedules, next_tick).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Reclaimed { schedule_id, .. } if schedule_id == "h")),
        "a released occurrence must be reclaimed on the very next tick, not stuck waiting on its 900s lease, got {events:?}"
    );
}

/// Shutdown must not throw away a real,
/// already-known result. Port of probe P6: a run finishes but is not yet drained
/// when `release_pending` runs; the finished run's outcome must be recorded, not
/// released as though it were still abandoned mid-flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_p6_release_after_finish_then_restart() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter.clone(), "a");
    let schedules = [tracked(
        Schedule::recurring("h", "0 0 * * * *", "UTC").expect("valid"),
        t(0),
    )];
    let e1 = loop_.tick(&schedules, tm(1, 0, 5)).await;
    for _ in 0..400 {
        if starter.active() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    eprintln!("P6 e1={e1:?} active_after_wait={}", starter.active());
    loop_.release_pending(tm(1, 0, 5));
    let st = store.lock().unwrap().occurrence_status("h", t(1)).unwrap();
    assert_eq!(
        st.as_deref(),
        Some("done"),
        "a finished run must be recorded, not released as though it were still running"
    );

    // Restart: a new loop, same store, before the schedule's next occurrence.
    let starter2 = scheduled_runs(factory, &["h"]);
    let mut loop2 = SchedulerLoop::new(store.clone(), starter2, "b");
    let ev = drain(&mut loop2, &schedules, tm(1, 40, 0)).await;
    let counts = store.lock().unwrap().occurrence_counts("h").unwrap();
    eprintln!("P6 restart events={ev:?} counts(done,failed)={counts:?}");
    assert_eq!(
        counts,
        (1, 0),
        "the already-recorded occurrence must not be run or counted a second time"
    );
}

/// A claim released this way (status stays
/// `claimed`, lease already expired) must actually be picked up by a fresh instance
/// through the normal loop, at the store level directly - port of probe P7.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_p7_released_pending_row_is_picked_up() {
    let store = ScheduleStore::open_in_memory().expect("store");
    let due = t(1);
    let d = store
        .tick_schedule("h", due, "a", Duration::from_secs(900), |_| {
            Ok(Some((due, true)))
        })
        .unwrap();
    let TickDecision::Evaluated {
        claim: ClaimOutcome::Claimed { generation, .. },
        ..
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(store.release_claim("h", due, "a", generation, due).unwrap());
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory, &["h"]);
    let store = Arc::new(Mutex::new(store));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "b");
    let schedules = [tracked(
        Schedule::recurring("h", "0 0 * * * *", "UTC").expect("valid"),
        t(0),
    )];
    let ev = drain(&mut loop_, &schedules, tm(1, 20, 0)).await;
    let st = store.lock().unwrap().occurrence_status("h", due).unwrap();
    eprintln!("P7 events={ev:?} status={st:?}");
    assert!(
        ev.iter()
            .any(|e| matches!(e, TickEvent::Started { .. } | TickEvent::Reclaimed { .. })),
        "released row never run; status {st:?}"
    );
}

/// A suspend longer than the lease must never
/// make the loop reclaim its own still-live run - port of probe P5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_p5_self_reclaim_after_suspend() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter.clone(), "a");
    let schedules = [tracked(
        Schedule::recurring("h", "0 0 * * * *", "UTC").expect("valid"),
        t(0),
    )];
    let e1 = loop_.tick(&schedules, tm(1, 0, 5)).await;
    let e2 = loop_.tick(&schedules, tm(1, 30, 0)).await; // woke 30 min later, lease 15 min
    eprintln!("P5 e1={e1:?}\nP5 e2={e2:?} active={}", starter.active());
    let rest = drain(&mut loop_, &schedules, tm(1, 30, 0)).await;
    let st = store.lock().unwrap().occurrence_status("h", t(1)).unwrap();
    eprintln!("P5 drained={rest:?} status={st:?}");
    assert!(
        !e2.iter().any(|e| matches!(e, TickEvent::Reclaimed { .. })),
        "own live run reclaimed, got {e2:?}"
    );
    assert!(
        rest.iter()
            .any(|e| matches!(e, TickEvent::Completed { schedule_id, .. } if schedule_id == "h")),
        "the real run's own result must still be recorded, not dropped, got {rest:?}"
    );
}

/// A renewal failure must genuinely re-claim
/// the row in the claim table, not merely be papered over by this one instance's own
/// local exclusion of its `self.pending` occurrences from the `expired_claims` scan.
/// `probe_p5_self_reclaim_after_suspend` alone cannot tell the two apart - a single
/// instance's own exclusion already hides the symptom from itself - so this test
/// inspects the claim table directly, the way a *second*, independent instance
/// sharing the same store would see it. This is exactly the test the coordinator
/// asked for: it fails if the renewal call (and its self-heal reclaim on failure) is
/// removed, even though `probe_p5` alone would not catch that regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lapsed_lease_is_genuinely_refreshed_in_the_claim_table_not_only_excluded_locally() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "a");
    let schedules = [tracked(
        Schedule::recurring("h", "0 0 * * * *", "UTC").expect("valid"),
        t(0),
    )];

    loop_.tick(&schedules, tm(1, 0, 5)).await;
    // A suspend longer than the default 15-minute lease.
    loop_.tick(&schedules, tm(1, 30, 0)).await;

    // Ask the claim table itself, independent of this loop's own `self.pending`
    // bookkeeping, whether the row still looks abandoned a moment later - which is
    // exactly what a second `SchedulerLoop` instance sharing this same store would
    // ask on its own next tick.
    let still_looks_abandoned = store
        .lock()
        .unwrap()
        .expired_claims("h", tm(1, 30, 1))
        .unwrap();
    assert!(
        still_looks_abandoned.is_empty(),
        "the lease must be genuinely refreshed in the claim table (a real re-claim, \
         not only excluded from this one instance's own scan), so a second instance \
         sharing this store never reclaims this instance's still-live run either; \
         found: {still_looks_abandoned:?}"
    );
}

/// The run id `start_run` actually started
/// must be recorded against its occurrence, visibly and reliably - not silently
/// dropped on a store error, and provably present, not merely "probably set because
/// the code path exists".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_run_id_is_recorded_when_a_scheduled_run_starts() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), false);
    let starter = scheduled_runs(factory.clone(), &["h"]);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "a");
    let due = t(1);
    let schedule = Schedule::one_shot("h", due, "UTC").expect("valid");
    let schedules = [tracked(schedule, t(0))];

    let events = loop_.tick(&schedules, due).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { .. }))
    );

    let expected_run_id = kgraf_orchestrator::ScheduledRuns::run_id("h", due);
    assert_eq!(
        store.lock().unwrap().run_ids_for_schedule("h").unwrap(),
        vec![expected_run_id.as_str().to_owned()],
        "the run id `start_run` actually used must be recorded against this occurrence"
    );
}

/// Under `Skip`, a capacity deferral that has
/// fallen outside the on-time window is dropped - not run late, and not counted
/// toward the pause threshold - the same "missed means dropped" rule `Skip` already
/// applies to an occurrence that was never even claimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skip_policy_deferral_past_the_on_time_window_is_dropped_not_run_late() {
    let dir = tempfile::tempdir().expect("dir");
    // "first" holds the one run slot for far longer than the on-time window (2
    // minutes), so "second" is refused capacity for the whole test, deterministically
    // (no dependence on how fast the worker fake actually is).
    let factory = factory_with_delay(dir.path(), Duration::from_secs(600));
    let starter = scheduled_runs_capped(factory.clone(), &["first", "second"], 1);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "a");
    let due = t(1);
    let first = Schedule::one_shot("first", due, "UTC").expect("valid");
    let second = Schedule::one_shot("second", due, "UTC")
        .expect("valid")
        .with_missed_run_policy(MissedRunPolicy::Skip);
    let schedules = [tracked(first, t(0)), tracked(second, t(0))];

    let events = loop_.tick(&schedules, due).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TickEvent::Started { schedule_id, .. } if schedule_id == "first")),
        "got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, TickEvent::Deferred { schedule_id, .. } if schedule_id == "second")
        ),
        "got {events:?}"
    );

    // Past the on-time window, still refused: dropped, not run late.
    let past_window = due + chrono::TimeDelta::minutes(3);
    let events = loop_.tick(&schedules, past_window).await;
    assert!(
        events.iter().any(
            |e| matches!(e, TickEvent::Dropped { schedule_id, .. } if schedule_id == "second")
        ),
        "got {events:?}"
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .schedule_status("second")
            .unwrap()
            .consecutive_failures,
        0,
        "a drop must never count toward the pause threshold"
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .occurrence_status("second", due)
            .unwrap(),
        Some("dropped".to_owned())
    );
}

/// Port of probe P4: DST correctness under the new on-time/wake path, both policies,
/// twenty-second ticks, across both the fall-back and spring-forward transitions and
/// a plain half-hourly schedule - each combination must fire at most once per real
/// occurrence (no duplicate `Started` for the same `due_at`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_p4_dst_new_path_fires_each_occurrence_at_most_once() {
    for (pat, start, end) in [
        (
            "0 30 1 * * *",
            Utc.with_ymd_and_hms(2026, 11, 1, 4, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 1, 8, 0, 0).unwrap(),
        ),
        (
            "0 30 2 * * *",
            Utc.with_ymd_and_hms(2026, 3, 8, 5, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 3, 8, 9, 0, 0).unwrap(),
        ),
        (
            "0 */30 * * * *",
            Utc.with_ymd_and_hms(2026, 11, 1, 4, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 1, 8, 0, 0).unwrap(),
        ),
    ] {
        for policy in [MissedRunPolicy::Skip, MissedRunPolicy::RunOnceOnWake] {
            let dir = tempfile::tempdir().expect("dir");
            let factory = factory(dir.path(), false);
            let starter = scheduled_runs(factory.clone(), &["d"]);
            let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
            let mut loop_ =
                SchedulerLoop::new(store, starter, "a").with_on_time_window(TimeDelta::seconds(40));
            let s = Schedule::recurring("d", pat, "America/New_York")
                .expect("valid")
                .with_missed_run_policy(policy);
            let schedules = [tracked(s, start - chrono::Duration::hours(1))];
            let mut now = start;
            let mut fired = Vec::new();
            while now <= end {
                for e in loop_.tick(&schedules, now).await {
                    if let TickEvent::Started { due_at, .. } = e {
                        fired.push(due_at);
                    }
                }
                now += chrono::Duration::seconds(20);
            }
            let mut sorted = fired.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                fired.len(),
                "pattern {pat} policy {policy:?}: duplicate firing, got {fired:?}"
            );
        }
    }
}

/// The claim table stores milliseconds, so a deferral retry reads
/// its occurrence back truncated. A one-shot instant with sub-millisecond digits used
/// to look later than its own stored occurrence ("a later occurrence is already
/// due"), and the deferral was given up at once. Instants are now cut to whole
/// milliseconds at creation and on load, so the occurrence keeps deferring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_shot_with_sub_millisecond_digits_keeps_deferring() {
    let dir = tempfile::tempdir().expect("dir");
    // "first" holds the one slot for the whole test.
    let factory = factory_with_delay(dir.path(), Duration::from_secs(600));
    let starter = scheduled_runs_capped(factory.clone(), &["first", "second"], 1);
    let store = Arc::new(Mutex::new(ScheduleStore::open_in_memory().expect("store")));
    let mut loop_ = SchedulerLoop::new(store.clone(), starter, "a");
    let due = t(1) + chrono::TimeDelta::microseconds(123_456);
    let first = Schedule::one_shot("first", due, "UTC").expect("valid");
    let second = Schedule::one_shot("second", due, "UTC").expect("valid");
    let schedules = [tracked(first, t(0)), tracked(second, t(0))];

    let events = loop_.tick(&schedules, due).await;
    assert!(
        events.iter().any(|e| matches!(e,
            TickEvent::Deferred { schedule_id, deferred_count: 1, .. } if schedule_id == "second")),
        "{events:?}"
    );
    for n in 1..=3 {
        let events = loop_
            .tick(&schedules, due + chrono::TimeDelta::seconds(n))
            .await;
        assert!(
            !events.iter().any(
                |e| matches!(e, TickEvent::Failed { schedule_id, .. } if schedule_id == "second")
            ),
            "a one-shot has no later occurrence; it must keep deferring: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e,
                TickEvent::Deferred { schedule_id, .. } if schedule_id == "second")),
            "{events:?}"
        );
    }
}
