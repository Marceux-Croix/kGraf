//! Route picks move the rotation cursor only when they are used, and a route
//! that lacks a tool the contract names is passed over.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    FnProbe, Orchestrator, ReadinessFailure, RoutingPolicy, RunStatus, TraceEvent, TraceLog,
};
use kgraf_tools::{Access, CallPermit, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    let log = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace");
    log.load().await.expect("load")
}

/// A contract whose first pick is transiently not ready asks again from the
/// next route: the rotation moves on, so one unready route cannot hold a
/// contract while another route is healthy. The first roster route is not
/// ready once; the contract then runs on the next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contract_moves_to_the_next_route_when_its_pick_is_not_ready() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut s = p.services(WINDOW);
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = probes.clone();
    s.routes[0].readiness = Arc::new(FnProbe(Arc::new(move || {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(ReadinessFailure::Transient("warming up".into()))
        } else {
            Ok(())
        }
    })));
    let report = Orchestrator::new(support::config(), s)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        probes.load(Ordering::SeqCst) >= 1,
        "the first pick was probed"
    );
    assert_eq!(
        p.stats.vendors(),
        ["beta".to_owned()].into(),
        "the unready pick was left behind"
    );
}

/// A route that stays not ready is left behind: the contract's next pick moves
/// on and runs on the healthy route, instead of waiting on the first one.
async fn run_with_first_route_never_ready(routing: RoutingPolicy) {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut s = p.services(WINDOW);
    s.routes[0].readiness = Arc::new(FnProbe(Arc::new(|| {
        Err(ReadinessFailure::Transient("never warms up".into()))
    })));
    let mut config = support::config();
    config.routing = routing;
    let report = tokio::time::timeout(
        Duration::from_secs(20),
        Orchestrator::new(config, s)
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run must not wait on the route that never becomes ready")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        p.stats.vendors(),
        ["beta".to_owned()].into(),
        "the work ran on the ready route"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotation_moves_past_a_route_that_never_becomes_ready() {
    run_with_first_route_never_ready(RoutingPolicy::Rotation).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fitness_passes_over_a_route_that_never_becomes_ready() {
    run_with_first_route_never_ready(RoutingPolicy::Fitness).await;
}

/// A contract stuck on unready routes moves the shared cursor once per route it
/// finds not ready, not once per check. Both routes fail their first three
/// checks together: the third check lands on the first route again, which the
/// contract already left behind, so the cursor stays put and the contract
/// runs on the first route once it answers, leaving the second for the next
/// contract in the rotation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contract_stuck_on_unready_routes_moves_the_cursor_once_per_route() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut s = p.services(WINDOW);
    let probes = Arc::new(AtomicUsize::new(0));
    for route in &mut s.routes {
        let seen = probes.clone();
        route.readiness = Arc::new(FnProbe(Arc::new(move || {
            if seen.fetch_add(1, Ordering::SeqCst) < 3 {
                Err(ReadinessFailure::Transient("warming up".into()))
            } else {
                Ok(())
            }
        })));
    }
    let report = Orchestrator::new(support::config(), s)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a then b".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let vendor_of = |instruction: &str| {
        p.stats
            .requests()
            .into_iter()
            .find(|r| r.1.contains(instruction))
            .map(|r| r.0)
    };
    let a = vendor_of("make a").expect("a ran");
    let b = vendor_of("make b").expect("b ran");
    assert_eq!(a, "alpha", "a stayed on the route it left behind once");
    assert_eq!(b, "beta", "b took the next route in the rotation");
}

/// Under `Fitness`, each dispatched pick moves the cursor, so the exploration
/// share is one pick in eight and not every pick.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fitness_picks_advance_the_cursor_so_exploration_is_a_share() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
        c("c", "make c", "src/c.rs", &[]),
        c("d", "make d", "src/d.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut config = support::config();
    config.routing = RoutingPolicy::Fitness;
    let report = Orchestrator::new(config, p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make four files".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let reasons: Vec<String> = trace_of(&p)
        .await
        .into_iter()
        .filter_map(|e| match e {
            TraceEvent::RouteDecision { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert!(reasons.len() >= 4, "{reasons:?}");
    let exploring = reasons
        .iter()
        .filter(|r| r.contains("exploration share"))
        .count();
    assert_eq!(
        exploring, 1,
        "only the pick at cursor zero explores among four picks: {reasons:?}"
    );
}

/// Names a tool only one route registers.
struct Marker;

#[async_trait::async_trait]
impl Tool for Marker {
    fn name(&self) -> &str {
        "marker_tool"
    }
    fn definition(&self) -> kgraf_provider::message::ToolDefinition {
        kgraf_provider::message::ToolDefinition {
            name: "marker_tool".into(),
            description: "Marks".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("marked"))
    }
}

/// The cursor points at alpha, which lacks the tool the contract names: the
/// pick passes over it to beta, which has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_without_the_contracts_tool_is_passed_over() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = json!({"contracts": [{
        "id": "a",
        "instruction": "make a WRITE src/a.rs",
        "acceptance": "src/a.rs exists",
        "writes": [{"path": "src/a.rs"}],
        "tools": ["write_file", "marker_tool"],
    }]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut s = p.services(WINDOW);
    s.routes[1] = route_with(
        "beta",
        "vendor-b",
        BETA_MODEL,
        p.beta.clone(),
        p.ledger.clone(),
        WINDOW,
        &[Arc::new(Marker)],
    );
    let report = Orchestrator::new(support::config(), s)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        p.stats.vendors(),
        ["beta".to_owned()].into(),
        "only beta offers the tool"
    );
}

/// Resume a crafted record whose one planned contract `a` names route `beta`
/// with `route_pinned` false, as a record from before the field existed reads:
/// the vendors that served workers.
async fn resume_with_route_recorded(failures: u32) -> std::collections::BTreeSet<String> {
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        |_| "{}".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut a = kgraf_orchestrator::OpenContract::planned(
        "a",
        "r1/a".into(),
        "make a WRITE src/a.rs",
        "ok",
    );
    a.writes = vec![kgraf_orchestrator::WriteTarget {
        path: "src/a.rs".into(),
        node: None,
    }];
    a.tools = vec!["write_file".into()];
    a.route = Some("beta".into());
    a.route_pinned = false;
    a.failures = failures;
    let mut record = kgraf_orchestrator::RunStateRecord::new(RunId::new("r1"), "crafted", 1);
    record.planned = true;
    record.open_contracts = vec![a];
    let store = kgraf_orchestrator::RunStore::new(p.graph.clone());
    store.save(&record).expect("save");
    store.set_active(&RunId::new("r1")).expect("active");
    let report = Orchestrator::new(support::config(), p.services(WINDOW))
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await
        .expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    p.stats.vendors()
}

/// A record from before `route_pinned` existed: a contract that never
/// dispatched got its route from the plan, so it resumes pinned. One that
/// failed before cannot be told from an assignment and is picked afresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_never_dispatched_contract_with_a_route_resumes_pinned() {
    assert_eq!(
        resume_with_route_recorded(0).await,
        ["beta".to_owned()].into(),
        "the plan's route is kept"
    );
    assert_eq!(
        resume_with_route_recorded(1).await,
        ["alpha".to_owned()].into(),
        "a contract that had failed is not taken for a pin"
    );
}
