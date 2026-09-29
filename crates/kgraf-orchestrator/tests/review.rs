//! Tests for the findings of the orchestrator review. Each fails on the
//! code before its fix.

mod support;

use kgraf_core::{Money, RunId};
use kgraf_orchestrator::{
    Admission, ContractStatus, Limits, Orchestrator, OrchestratorError, PlanError, RunStatus,
    RunStore, TraceEvent, TraceLog,
};
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

fn record_of(p: &Parts) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

async fn until(what: &str, f: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_pinning_an_unconfigured_route_is_refused_before_any_worker_runs() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = serde_json::json!({"contracts":[
        {"id":"a","instruction":"make a WRITE src/a.rs","acceptance":"ok",
         "writes":[{"path":"src/a.rs"}],"route":"openai"}
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
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect_err("refused");
    assert!(
        matches!(&err, OrchestratorError::Plan(PlanError::Invalid(m)) if m.contains("not configured")),
        "{err}"
    );
    assert!(
        p.stats.requests().is_empty(),
        "never replaced by another route"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contract_ids_colliding_after_sanitizing_are_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a b", "make one", "src/one.rs", &[]),
        c("a-b", "make two", "src/two.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let err = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "two parts".into())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("both become `a-b`"), "{err}");
    assert!(p.stats.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leases_and_holds_survive_waits_longer_than_their_ttl() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        // Each worker round takes 300 ms: longer than a hold's 100 ms TTL.
        Duration::from_millis(300),
    );
    p.admission = Admission::with_ttl(Limits::default(), 100);
    // The verifier takes 700 ms: longer than a lease's 200 ms TTL.
    *p.verifier.delay.lock().expect("lock") = Duration::from_millis(700);
    *p.verifier.probe.lock().expect("lock") = Some((p.graph.clone(), p.admission.clone()));
    let mut cfg = config();
    cfg.lease_ttl_ms = 200;
    let o = Orchestrator::new(cfg, p.services(WINDOW)).expect("orchestrator");
    let admission = p.admission.clone();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("the worker is running", || !p.stats.requests().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        admission.budget().held() > Money::ZERO,
        "an in-flight worker's hold does not lapse"
    );
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let probed = p.verifier.probed.lock().expect("lock").clone();
    assert_eq!(probed.len(), 1);
    assert!(
        probed[0].0,
        "the write lease is still held when verification ends"
    );
    assert_eq!(report.counters.leases_lost, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_orchestrator_on_the_same_run_is_refused() {
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
    let first = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = first.handle();
    let run = tokio::spawn(first.start(RunId::new("r1"), "make a".into()));
    until("the first run's worker is mid-round", || {
        p.alpha.hang_reached.load(Ordering::SeqCst) || p.beta.hang_reached.load(Ordering::SeqCst)
    })
    .await;
    let second = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await
        .expect_err("refused");
    assert!(
        matches!(second, OrchestratorError::RunBusy { .. }),
        "{second}"
    );
    // Starting the same run id again is refused the same way.
    let third = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect_err("refused");
    assert!(
        matches!(third, OrchestratorError::RunBusy { .. }),
        "{third}"
    );
    handle.stop().await;
    assert_eq!(
        run.await.expect("join").expect("run").status,
        RunStatus::Stopped
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_waiting_through_a_failed_re_grounding_stays_undelivered() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| "I would rather not say.".into(),
        Duration::from_millis(150),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a then b".into()));
    until("a is dispatched", || !p.stats.requests().is_empty()).await;
    handle.steer("keep b small").await.expect("steer");
    let report = run.await.expect("join").expect("run");
    assert!(matches!(report.status, RunStatus::Blocked { .. }));
    let record = record_of(&p);
    assert!(
        !record.notes[0].delivered,
        "the grounder never answered, so the note never reached a model"
    );
    let b = record.contract("b").expect("b");
    assert!(
        matches!(&b.status, ContractStatus::Held { reason } if reason.contains("re-grounding failed"))
    );
    // It was offered, though: the failed call's prompt carried it.
    assert!(p.grounder.prompts()[0].contains("keep b small"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_lease_cancels_the_workers_holding_it() {
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
    cfg.lease_ttl_ms = 200;
    let o = Orchestrator::new(cfg, p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("the worker is mid-round", || {
        p.alpha.hang_reached.load(Ordering::SeqCst) || p.beta.hang_reached.load(Ordering::SeqCst)
    })
    .await;
    // Another holder takes the file (as after an expiry), for a short while.
    {
        let g = p.graph.lock().expect("lock");
        let reg = kgraf_graph::LeaseRegistry::new(&g).with_ttl_ms(300);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as i64;
        let holder = reg
            .holder_of("src/a.rs", now)
            .expect("registry")
            .expect("the worker's lease")
            .agent_id;
        assert!(holder.starts_with("r1/a@"), "{holder}");
        reg.release_all(&holder).expect("release");
        let got = reg
            .acquire_all("thief", "t", &["src/a.rs".to_owned()], &[], now)
            .expect("acquire");
        assert!(matches!(got, kgraf_graph::Acquisition::Granted(_)));
    }
    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("in time")
        .expect("join")
        .expect("run");
    assert!(report.counters.leases_lost >= 1, "{report:?}");
    let trace = trace_of(&p).await;
    let outcomes: Vec<&str> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::WorkerFinished { outcome, .. } => Some(outcome.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.first(), Some(&"cancelled"), "{outcomes:?}");
    // Once the other holder's lease lapsed, the contract ran again and passed.
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
}

/// Decision 0080: kGraf does not limit spend. Admission carries no cap, so a
/// run whose calls would have exceeded any of the old per-task, per-run or
/// per-hour caps runs to completion, and every call is still ledgered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_would_have_exceeded_every_old_cap_still_completes_and_is_ledgered() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("no cap refuses it");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(p.planner.calls() > 0, "the decomposition call was made");
    let prices = prices();
    let ledger_cost = p
        .ledger
        .calls()
        .expect("calls")
        .iter()
        .map(|c| prices.cost(&c.model, &c.usage).expect("priced"))
        .fold(Money::ZERO, |a, b| a + b);
    assert!(
        ledger_cost > Money::ZERO,
        "every call's cost is in the ledger"
    );
    assert_eq!(report.spent, ledger_cost);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_runs_in_one_process_share_one_admission_gate() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(60),
    );
    p.admission = Admission::new(Limits::new(1, 8).expect("limits"));
    // Two runs over one admission: one work item at a time across both.
    let o1 = Orchestrator::new(config(), p.services(WINDOW)).expect("o1");
    let o2 = Orchestrator::new(config(), p.services(WINDOW)).expect("o2");
    let (r1, r2) = tokio::join!(
        o1.start(RunId::new("r1"), "make a".into()),
        o2.start(RunId::new("r2"), "make a".into())
    );
    assert_eq!(r1.expect("r1").status, RunStatus::Completed);
    assert_eq!(r2.expect("r2").status, RunStatus::Completed);
    assert_eq!(p.stats.max_in_flight.load(Ordering::SeqCst), 1);
}

/// Index a file with two functions into the parts' graph; return their node ids.
fn two_functions(p: &Parts) -> (String, String) {
    std::fs::create_dir_all(p.dir.join("src")).expect("dir");
    std::fs::write(
        p.dir.join("src/lib.rs"),
        "pub fn first() -> u32 {\n    1\n}\n\npub fn second() -> u32 {\n    2\n}\n",
    )
    .expect("write");
    let g = p.graph.lock().expect("lock");
    kgraf_graph::Indexer::new(&g).index().expect("index");
    let nodes = g.nodes_in_file("src/lib.rs").expect("nodes");
    let id = |name: &str| {
        nodes
            .iter()
            .find(|n| n.name == name)
            .map(|n| n.id.clone())
            .expect("indexed")
    };
    (id("first"), id("second"))
}

fn node_plan(writes: &[(&str, &str, Option<&str>)]) -> String {
    let list: Vec<serde_json::Value> = writes
        .iter()
        .map(|(id, ins, node)| {
            let mut w = serde_json::json!({"path": "src/lib.rs"});
            if let Some(n) = node {
                w["node"] = serde_json::json!(n);
            }
            serde_json::json!({
                "id": id,
                "instruction": format!("{ins} WRITE src/lib.rs"),
                "acceptance": "src/lib.rs exists",
                "writes": [w],
                "tools": ["write_file"],
            })
        })
        .collect();
    serde_json::json!({ "contracts": list }).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sub_file_leases_let_two_functions_of_one_file_run_at_once_and_a_whole_file_waits() {
    // Two contracts on different functions of one file: node leases, concurrent.
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        |_| String::new(),
        |_| SERVES.into(),
        Duration::from_millis(60),
    );
    let (first, second) = two_functions(&p);
    let plan = node_plan(&[
        ("x", "edit first", Some(&first)),
        ("y", "edit second", Some(&second)),
    ]);
    let p = Parts {
        planner: ScriptFake::new(move |_| plan.clone()),
        ..p
    };
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "edit both".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(p.stats.overlapped("edit first", "edit second"));
    assert_eq!(report.counters.serialized, 0);

    // One contract on a function and one on the whole file: they exclude.
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        |_| String::new(),
        |_| SERVES.into(),
        Duration::from_millis(60),
    );
    let (first, _) = two_functions(&p);
    let plan = node_plan(&[
        ("x", "edit first", Some(&first)),
        ("y", "rewrite all", None),
    ]);
    let p = Parts {
        planner: ScriptFake::new(move |_| plan.clone()),
        ..p
    };
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "edit one, rewrite all".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(!p.stats.overlapped("edit first", "rewrite all"));
    assert_eq!(report.counters.serialized, 1);
    let record = record_of(&p);
    assert!(record.admitted_leases.is_empty(), "every lease released");
}
