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
use kgraf_orchestrator::{
    Admission, Limits, MachineGate, Orchestrator, RunStore, TraceEvent, TraceLog,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
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

/// A run that ends on an error still reports the notes nothing carried
/// (decision 0067): `a`'s report is addressed to `b`, which waits on `x`
/// (parked) and never starts. The loop then fails on a refused record save.
/// The note is traced `undelivered` with the error as the reason, and the
/// sender gets a notice, while the loop's own error is what the run returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loop_error_still_reports_the_notes_nothing_carried() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a HOLDA", "src/a.rs", &[]),
        c("x", "make x HANG", "src/x.rs", &[]),
        c("y", "make y LATEY", "src/y.rs", &[]),
        c("b", "make b", "src/b.rs", &["a", "x"]),
    ]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(1),
    );
    parts.alpha.hang.store(true, Ordering::SeqCst);
    parts.beta.hang.store(true, Ordering::SeqCst);
    let hold_a = parts.hold_workers("HOLDA");
    let hold_y = parts.hold_workers_late("LATEY");
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let orchestrator = Orchestrator::new(config(), parts.services(WINDOW))
        .expect("orchestrator")
        .with_session_notices(Arc::new(move |t: String| {
            sink.lock().expect("lock").push(t);
        }));
    let run = tokio::spawn(orchestrator.start(RunId::new("r1"), "make a, x, y and b".into()));

    wait_for("x parks", || {
        parts.alpha.hang_reached.load(Ordering::SeqCst)
            || parts.beta.hang_reached.load(Ordering::SeqCst)
    })
    .await;
    wait_for("y parks in round 1", || parts.late_parked()).await;
    hold_a.add_permits(1);
    let record = || {
        RunStore::new(parts.graph.clone())
            .load(&RunId::new("r1"))
            .ok()
            .flatten()
    };
    wait_for("a's report to b is saved", || {
        record().is_some_and(|r| {
            r.coordination
                .mailbox
                .messages
                .iter()
                .any(|m| m.from_contract == "a" && m.to.iter().any(|t| t.contract == "b"))
        })
    })
    .await;
    let msg = record()
        .expect("record")
        .coordination
        .mailbox
        .messages
        .into_iter()
        .find(|m| m.from_contract == "a")
        .expect("a's report");

    // From now on the store refuses the run record; `y` finishing makes the
    // loop save it.
    let db = rusqlite::Connection::open(dir.path().join("graph.db")).expect("db");
    db.execute_batch(
        "CREATE TRIGGER refuse_run_record BEFORE UPDATE ON meta \
         WHEN NEW.key LIKE 'orchestrator/run/%' \
         BEGIN SELECT RAISE(ABORT, 'the store refuses the run record'); END;",
    )
    .expect("trigger");
    hold_y.add_permits(1);

    let ended = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the run ended in time")
        .expect("no panic");
    let error = ended.expect_err("the loop's error is returned");
    assert!(
        format!("{error:?}").contains("refuses the run record"),
        "the original error, not a report one: {error:?}"
    );

    let trace = TraceLog::open(&parts.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let undelivered: Vec<&str> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Message {
                id,
                action,
                to,
                detail,
                ..
            } if action == "undelivered" && *id == msg.id && to.iter().any(|t| t == "b") => {
                Some(detail.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(undelivered.len(), 1, "{undelivered:?}");
    assert!(
        undelivered[0].contains("the recipient b never started before the run ended with an error"),
        "{undelivered:?}"
    );
    let reported = |t: &String| t.contains(&msg.id) && t.contains("to b was not delivered");
    assert!(
        trace.iter().any(|e| matches!(
            e,
            TraceEvent::Notice { contract, detail, .. } if contract == "a" && reported(detail)
        )),
        "a notice on the sender's record"
    );
    let seen = seen.lock().expect("lock").clone();
    assert!(seen.iter().any(reported), "{seen:?}");
}
