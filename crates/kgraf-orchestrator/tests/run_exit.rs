//! A run loop that ends on an error still releases what it held. The
//! admission gate and its `MachineGate` are process-wide: a run that returned
//! on an error with a worker in flight used to keep that worker's agent slot
//! and work item for the life of the process, and leave the worker running.
//!
//! The failure is real, not mocked: the graph store refuses every write of
//! the run record (a trigger on its table), so the loop's next record save
//! fails the way a full disk or a locked database would.

mod support;

use kgraf_core::RunId;
use kgraf_core::parallel::ParallelismLimits;
use kgraf_orchestrator::{Admission, Limits, MachineGate, Orchestrator};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loop_error_with_a_worker_in_flight_cancels_it_and_frees_every_slot() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a HANG", "src/a.rs", &[]),
        c("b", "make b HOLDB", "src/b.rs", &[]),
    ]);
    let mut parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(1),
    );
    let machine = MachineGate::new(ParallelismLimits::derive(8, 8, 8));
    parts.admission = Admission::with_machine(Limits::default(), machine.clone());
    parts.alpha.hang.store(true, Ordering::SeqCst);
    parts.beta.hang.store(true, Ordering::SeqCst);
    let hold_b = parts.hold_workers("HOLDB");
    let orchestrator = Orchestrator::new(config(), parts.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(orchestrator.start(RunId::new("r1"), "make a and b".into()));

    // `a` is parked inside its second model call; `b` waits at its first.
    let parked = || {
        !parts.alpha.live.lock().expect("lock").is_empty()
            || !parts.beta.live.lock().expect("lock").is_empty()
    };
    wait_for("a parked", parked).await;
    wait_for("b dispatched", || {
        parts.stats.requests().iter().any(|r| r.1.contains("HOLDB"))
    })
    .await;
    // The slot is taken at admission, before the worker's first request, so
    // this is a bound on the real event and not a guess at its timing.
    wait_for("both workers to hold a machine slot", || {
        machine.agents_live() == 2
    })
    .await;

    // From now on the store refuses the run record; `b` finishing makes the
    // loop save it.
    let db = rusqlite::Connection::open(dir.path().join("graph.db")).expect("db");
    db.execute_batch(
        "CREATE TRIGGER refuse_run_record BEFORE UPDATE ON meta \
         WHEN NEW.key LIKE 'orchestrator/run/%' \
         BEGIN SELECT RAISE(ABORT, 'the store refuses the run record'); END;",
    )
    .expect("trigger");
    hold_b.add_permits(1);

    let ended = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the run ended in time")
        .expect("no panic");
    let error = ended.expect_err("the loop's error is returned");
    assert!(
        error.to_string().contains("refuses the run record")
            || format!("{error:?}").contains("refuses the run record"),
        "the original error, not a cleanup one: {error:?}"
    );

    // Every slot is back: the local gate and the process-wide one. A dropped
    // flight keeps its slot and holds until its worker's task has ended, then
    // returns them from a task of its own (`InFlight::drop`), so they come
    // back just after the run returns, not before. Wait for that release.
    wait_for("every slot and hold to be returned", || {
        parts.admission.gate().in_flight() == (0, 0)
            && machine.agents_live() == 0
            && parts.admission.budget().held() == kgraf_core::Money::ZERO
    })
    .await;
    assert_eq!(
        parts.admission.gate().in_flight(),
        (0, 0),
        "no work item or agent is still counted"
    );
    assert_eq!(machine.agents_live(), 0, "no machine slot leaked");
    assert_eq!(
        parts.admission.budget().held(),
        kgraf_core::Money::ZERO,
        "no budget hold left"
    );
    // The parked worker got its cancel: the runtime dropped its call.
    wait_for("the in-flight worker to be cancelled", || !parked()).await;
    // Its write lease and the run lease are released, not left renewing.
    let g = parts.graph.lock().expect("lock");
    let reg = kgraf_graph::LeaseRegistry::new(&g);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;
    for key in ["run:r1", "src/a.rs"] {
        assert!(
            reg.holder_of(key, now).expect("registry").is_none(),
            "{key} is still leased"
        );
    }
}
