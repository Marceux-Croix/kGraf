//! Guards on plans and run leases: a plan that declares repository
//! metadata is refused, a settlement over its hold never pauses dispatch,
//! and a run whose lease is lost writes nothing more.

mod support;

use kgraf_core::RunId;
use kgraf_graph::{Acquisition, LeaseRegistry};
use kgraf_orchestrator::{
    Admission, Limits, Orchestrator, OrchestratorError, PlanError, RunStatus, RunStore,
};
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A plan declares the repository metadata in
/// another case. On a case-insensitive volume `.GIT/config` is `.git/config`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_declaring_repository_metadata_in_any_spelling_is_refused() {
    let dot = |s: &str| format!(".{s}");
    for path in [
        format!("{}/config", dot("GIT")),
        format!("{}\u{200C}/config", dot("git")),
        format!("src/{}/x", dot("kGraf")),
        dot("GITATTRIBUTES"),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let plan = serde_json::json!({"contracts":[
            {"id":"a","instruction":"rewrite it","acceptance":"ok","writes":[{"path": path}]}
        ]})
        .to_string();
        let p = Parts::open(
            dir.path(),
            move |_| plan.clone(),
            |_| SERVES.into(),
            Duration::ZERO,
        );
        let err = Orchestrator::new(config(), p.services(WINDOW))
            .expect("orchestrator")
            .start(RunId::new("r1"), "rewrite".into())
            .await
            .expect_err("refused");
        assert!(
            matches!(&err, OrchestratorError::Plan(PlanError::Invalid(m))
                if m.contains("names protected state")
                    && !m.contains("not a relative path inside the repository")),
            "{path:?}: {err}"
        );
        assert!(p.stats.requests().is_empty());
    }
}

/// A worker whose first round cost more than its whole hold: the overrun is
/// counted and no further contract of the run is dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settlement_over_its_hold_is_counted_and_never_pauses_dispatch() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "COSTLY make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    // One work item at a time, so b waits for a's settlement.
    let p = Parts {
        admission: Admission::new(Limits::new(1, 8).expect("limits")),
        ..p
    };
    let mut cfg = config();
    cfg.step_budget = 2;
    let report = Orchestrator::new(cfg, p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "a and b".into())
        .await
        .expect("run");
    assert!(report.counters.overruns >= 1, "{report:?}");
    // Decision 0080: kGraf does not limit spend, so an overrun (the estimate
    // undercounted the call) is counted but never pauses dispatch; the run
    // completes and b still runs.
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        p.stats.requests().iter().any(|r| r.1.contains("make b")),
        "b still dispatched after the overrun"
    );
}

/// After another orchestrator takes the run's lease, this one writes nothing:
/// not the record, and not a release of the new owner's leases.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_losing_the_run_lease_the_old_owner_writes_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let mut cfg = config();
    cfg.lease_ttl_ms = 400;
    let o = Orchestrator::new(cfg, p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("the worker is mid-round", || {
        p.alpha.hang_reached.load(Ordering::SeqCst) || p.beta.hang_reached.load(Ordering::SeqCst)
    })
    .await;

    // The new owner takes the run lease (as after a lapse) and writes the record.
    let store = RunStore::new(p.graph.clone());
    {
        let g = p.graph.lock().expect("lock");
        let reg = LeaseRegistry::new(&g).with_ttl_ms(60_000);
        let old = reg
            .holder_of("run:r1", now_ms())
            .expect("registry")
            .expect("the run lease")
            .agent_id;
        reg.release_all(&old).expect("release");
        let got = reg
            .acquire_all("new-owner", "r1", &["run:r1".to_owned()], &[], now_ms())
            .expect("acquire");
        assert!(matches!(got, Acquisition::Granted(_)));
    }
    let mut record = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    record.updated_ms = 42;
    store.save(&record).expect("save");

    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("in time")
        .expect("join")
        .expect("run");
    assert!(
        matches!(&report.status, RunStatus::Blocked { reason } if reason.contains("run lease")),
        "{report:?}"
    );
    let after = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(
        after.updated_ms, 42,
        "the old owner saved over the new owner's record"
    );
    let g = p.graph.lock().expect("lock");
    let holder = LeaseRegistry::new(&g)
        .holder_of("run:r1", now_ms())
        .expect("registry")
        .expect("still held");
    assert_eq!(
        holder.agent_id, "new-owner",
        "the new owner's lease survived"
    );
}

/// A lease taken from a contract between the last renewal tick and the moment
/// its result is accepted: the synchronous check before acceptance catches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_lost_just_before_acceptance_voids_the_result() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    *p.verifier.delay.lock().expect("lock") = Duration::from_millis(400);
    let mut cfg = config();
    // Renewal every 15 s: the timer cannot notice a loss during the test.
    cfg.lease_ttl_ms = 60_000;
    let o = Orchestrator::new(cfg, p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    let verifier = p.verifier.clone();
    until("verification has begun", || {
        verifier.started.load(Ordering::SeqCst) >= 1
    })
    .await;
    {
        let g = p.graph.lock().expect("lock");
        let reg = LeaseRegistry::new(&g).with_ttl_ms(300);
        let holder = reg
            .holder_of("src/a.rs", now_ms())
            .expect("registry")
            .expect("held")
            .agent_id;
        reg.release_all(&holder).expect("release");
        let got = reg
            .acquire_all("thief", "t", &["src/a.rs".to_owned()], &[], now_ms())
            .expect("acquire");
        assert!(matches!(got, Acquisition::Granted(_)));
    }
    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("in time")
        .expect("join")
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(report.counters.leases_lost >= 1, "{report:?}");
    assert_eq!(
        p.verifier.verified.load(Ordering::SeqCst),
        2,
        "the first pass was voided; the contract verified again once the lease was back"
    );
}
