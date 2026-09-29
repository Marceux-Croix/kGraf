//! Waits at dispatch that had no end, or no voice: a route that stays
//! transiently unready, and a flight wider than the agent ceiling.

mod support;

use kgraf_core::{ParallelismLimits, RunId};
use kgraf_orchestrator::{
    Admission, DeferReason, FnProbe, Limits, MachineGate, Orchestrator, ReadinessFailure,
    RunStatus, TraceEvent, TraceLog,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A contract with two parallel attempts, under a machine ceiling of one
/// agent (the slider set below the project's own limit): it runs at the
/// ceiling, one attempt at a time, and says so, instead of waiting for
/// capacity it can never get.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flight_wider_than_the_machine_ceiling_runs_at_the_ceiling() {
    let dir = tempfile::tempdir().expect("dir");
    let mut wide = c("a", "make a", "src/a.rs", &[]);
    wide.attempts = 2;
    let plan = plan_json(&[wide]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let machine = MachineGate::new(ParallelismLimits::derive(1, 1, 1));
    p.admission = Admission::with_machine(Limits::new(4, 8).expect("limits"), machine);
    let mut cfg = config();
    cfg.max_parallel_attempts = 2;
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(cfg, p.services(WINDOW))
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended: the wide flight was not deferred for ever")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("exceed the ceiling of 1 agents")),
        "{:?}",
        report.notices
    );
}

/// A route that stays transiently unready is not waited for in silence: the
/// wait is traced again, and noticed, every twenty checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_readiness_wait_is_traced_and_noticed_periodically() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut s = p.services(WINDOW);
    let checks = Arc::new(AtomicUsize::new(0));
    let seen = checks.clone();
    let not_ready_for = 25;
    let probe = Arc::new(FnProbe(Arc::new(move || {
        if seen.fetch_add(1, Ordering::SeqCst) < not_ready_for {
            Err(ReadinessFailure::Transient("warming up".into()))
        } else {
            Ok(())
        }
    })));
    // Both routes: whichever the rotation picks is the one that waits.
    for r in &mut s.routes {
        r.readiness = probe.clone();
    }
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(config(), s)
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("still not ready after 20 checks")),
        "{:?}",
        report.notices
    );
    let trace = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let deferrals: Vec<&String> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Deferred {
                reason: DeferReason::Readiness,
                detail,
                ..
            } => Some(detail),
            _ => None,
        })
        .collect();
    assert!(
        deferrals
            .iter()
            .any(|d| d.contains("still not ready after 20 checks")),
        "{deferrals:?}"
    );
    assert!(
        deferrals.len() < 5,
        "the wait is reported sparsely, not every check: {deferrals:?}"
    );
}
