//! A straight chain of a fresh plan is one contract when its steps run on one
//! route and one worker can carry it (decision 0014, amendment of
//! 2026-10-14). Observed in the evidence runs: plans of two to five contracts,
//! each waiting on the one before, spent most of the run in interface asks,
//! handshakes and re-plans on edges that gained no parallelism.
//!
//! The planner and the workers are fake providers through the real ledger.
//! Each test drives the whole loop and reads what was dispatched.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    Orchestrator, OrchestratorConfig, RunReport, RunStatus, RunStore, TraceEvent, TraceLog,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;

struct Out {
    report: RunReport,
    events: Vec<TraceEvent>,
    /// The contracts workers were called for (ledger task ids under the run).
    worked: BTreeSet<String>,
}

async fn trace(dir: &Path) -> Vec<TraceEvent> {
    let path = dir.join("runs").join("r1").join("trace.jsonl");
    let started = Instant::now();
    loop {
        match TraceLog::open(&path).await {
            Ok(log) => return log.load().await.expect("load"),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("trace: {e:?}"),
        }
    }
}

/// Runs `plan` to its end with `routes` worker routes (1 or 2) under `window`.
async fn run_plan(plan: String, routes: usize, window: u64, cfg: OrchestratorConfig) -> Out {
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut services = p.services(window);
    services.routes.truncate(routes);
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "make the files".into())
        .await
        .expect("run");
    let worked = p
        .ledger
        .calls()
        .expect("calls")
        .into_iter()
        .filter_map(|c| c.task_id.strip_prefix("r1/").map(str::to_owned))
        .collect();
    Out {
        report,
        events: trace(dir.path()).await,
        worked,
    }
}

fn accepted(events: &[TraceEvent]) -> (Vec<kgraf_orchestrator::PlannedContract>, Vec<String>) {
    events
        .iter()
        .find_map(|e| match e {
            TraceEvent::PlanAccepted {
                contracts, notices, ..
            } => Some((contracts.clone(), notices.clone())),
            _ => None,
        })
        .expect("a plan was accepted")
}

fn collapsed(events: &[TraceEvent]) -> Vec<(String, Vec<String>, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::ChainCollapsed {
                contract,
                merged,
                route,
                ..
            } => Some((contract.clone(), merged.clone(), route.clone())),
            _ => None,
        })
        .collect()
}

fn notices(events: &[TraceEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Notice { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .collect()
}

fn ids(events: &[TraceEvent]) -> Vec<String> {
    accepted(events).0.into_iter().map(|c| c.id).collect()
}

fn chain3() -> String {
    plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
        c("c", "make c", "src/c.rs", &["b"]),
    ])
}

/// `plan` with each contract's `route` set, in order.
fn pinned(plan: &str, routes: &[&str]) -> String {
    let mut v: serde_json::Value = serde_json::from_str(plan).expect("plan");
    for (c, r) in v["contracts"]
        .as_array_mut()
        .expect("contracts")
        .iter_mut()
        .zip(routes)
    {
        c["route"] = serde_json::json!(r);
    }
    v.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_three_step_chain_on_one_route_is_one_contract_and_one_worker() {
    let out = run_plan(chain3(), 1, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert_eq!(ids(&out.events), ["a"]);
    assert_eq!(
        collapsed(&out.events),
        [(
            "a".into(),
            vec!["a".into(), "b".into(), "c".into()],
            "alpha".into()
        )]
    );
    let (contracts, notes) = accepted(&out.events);
    // Every step is in the merged brief and acceptance, in order, and its writes.
    let c = &contracts[0];
    let at = |s: &str| c.instruction.find(s).unwrap_or_else(|| panic!("{s}"));
    assert!(at("Step 1 of 3 (`a`)") < at("Step 2 of 3 (`b`)"));
    assert!(at("Step 2 of 3 (`b`)") < at("Step 3 of 3 (`c`)"));
    assert!(c.acceptance.contains("src/c.rs exists"));
    assert_eq!(c.writes, ["src/a.rs", "src/b.rs", "src/c.rs"]);
    assert!(c.depends_on.is_empty());
    // The reader sees it in the run's notices, and one worker did the work.
    assert!(
        notes
            .iter()
            .any(|n| n.contains("a -> b -> c") && n.contains("`a`")),
        "{notes:?}"
    );
    assert_eq!(out.worked, BTreeSet::from(["a".to_owned()]), "one worker");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_diamond_is_not_collapsed() {
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
        c("c", "make c", "src/c.rs", &["a"]),
        c("d", "make d", "src/d.rs", &["b", "c"]),
    ]);
    let out = run_plan(plan, 1, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert!(collapsed(&out.events).is_empty());
    assert_eq!(ids(&out.events), ["a", "b", "c", "d"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_on_a_rotating_roster_is_not_collapsed_and_says_why() {
    // Two routes, nothing pinned: rotation hands the steps to different models.
    let out = run_plan(chain3(), 2, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert!(collapsed(&out.events).is_empty());
    assert_eq!(ids(&out.events), ["a", "b", "c"]);
    assert!(
        notices(&out.events)
            .iter()
            .any(|n| n.contains("kept as planned") && n.contains("different routes")),
        "{:?}",
        notices(&out.events)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_under_fitness_routing_is_not_collapsed() {
    let mut cfg = config();
    cfg.routing = kgraf_orchestrator::RoutingPolicy::Fitness;
    let out = run_plan(chain3(), 2, WINDOW, cfg).await;
    assert!(collapsed(&out.events).is_empty());
    assert_eq!(ids(&out.events), ["a", "b", "c"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steps_pinned_to_one_route_collapse_and_to_two_routes_do_not() {
    let same = run_plan(
        pinned(&chain3(), &["beta", "beta", "beta"]),
        2,
        WINDOW,
        config(),
    )
    .await;
    assert_eq!(
        same.report.status,
        RunStatus::Completed,
        "{:?}",
        same.report
    );
    assert_eq!(
        collapsed(&same.events),
        [(
            "a".into(),
            vec!["a".into(), "b".into(), "c".into()],
            "beta".into()
        )]
    );
    let (contracts, _) = accepted(&same.events);
    assert_eq!(
        contracts[0].route.as_deref(),
        Some("beta"),
        "the pin is kept"
    );

    let mixed = run_plan(
        pinned(&chain3(), &["alpha", "alpha", "beta"]),
        2,
        WINDOW,
        config(),
    )
    .await;
    assert!(collapsed(&mixed.events).is_empty());
    assert_eq!(ids(&mixed.events), ["a", "b", "c"]);

    // Some steps pinned, some not, on several routes: not known.
    let partial = run_plan(pinned(&chain3(), &["alpha", "alpha"]), 2, WINDOW, config()).await;
    assert!(collapsed(&partial.events).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_too_big_for_one_worker_is_kept_as_planned() {
    // Each step fits under the plan target (40k of a 100k window) alone; the three together are over it.
    // The steps write documents: steps this large writing code would keep each
    // link split for a parallel start before the size rule is reached.
    let step = "pad ".repeat(6_000);
    let big = plan_json(&[
        c("a", &step, "docs/a.md", &[]),
        c("b", &step, "docs/b.md", &["a"]),
        c("c", &step, "docs/c.md", &["b"]),
    ]);
    let out = run_plan(big, 1, 100_000, config()).await;
    assert!(
        collapsed(&out.events).is_empty(),
        "{:?} {:?}",
        accepted(&out.events).1,
        notices(&out.events)
    );
    assert_eq!(ids(&out.events), ["a", "b", "c"]);
    assert!(
        notices(&out.events)
            .iter()
            .any(|n| n.contains("kept as planned") && n.contains("one worker cannot carry it")),
        "{:?}",
        notices(&out.events)
    );
}

/// Where the worker prefix and the projected growth alone are over the plan
/// target, no step fits and no split could make one fit: the chain is merged
/// all the same, since splitting only adds interfaces and handshakes. (Seen in
/// the first canary run of the rule: a 32k window, a 13k target, a 29k fixed
/// part.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_no_split_could_make_fit_is_still_one_contract() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = chain3();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut services = p.services(1_500);
    services.routes.truncate(1);
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let h = o.handle();
    let run = tokio::spawn(async move { o.start(RunId::new("r1"), "make the files".into()).await });
    assert!(h.status().await.is_some(), "a plan was accepted");
    h.stop().await;
    let _ = run.await.expect("join");
    let events = trace(dir.path()).await;
    assert!(
        notices(&events)
            .iter()
            .any(|n| n.contains("not split: projected")),
        "the fixed part is over the target: {:?}",
        notices(&events)
    );
    assert_eq!(ids(&events), ["a"], "{:?}", notices(&events));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_switch_keeps_every_plan_as_written() {
    let mut cfg = config();
    cfg.collapse_chains = false;
    let out = run_plan(chain3(), 1, WINDOW, cfg).await;
    assert!(collapsed(&out.events).is_empty());
    assert_eq!(ids(&out.events), ["a", "b", "c"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_from_the_root_merges_whole_and_its_dependents_wait_on_the_merge() {
    // root -> a -> b, then x and y: all three of the run merge; x and y wait on it.
    let plan = plan_json(&[
        c("root", "make root", "src/r.rs", &[]),
        c("a", "make a", "src/a.rs", &["root"]),
        c("b", "make b", "src/b.rs", &["a"]),
        c("x", "make x", "src/x.rs", &["b"]),
        c("y", "make y", "src/y.rs", &["b"]),
    ]);
    let out = run_plan(plan, 1, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert_eq!(ids(&out.events), ["root", "x", "y"]);
    let (contracts, _) = accepted(&out.events);
    assert_eq!(contracts[1].depends_on, ["root"]);
    assert_eq!(contracts[2].depends_on, ["root"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_stopped_with_a_collapsed_contract_resumes_and_finishes_it() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = chain3();
    let p1 = Parts::open_with(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    // The worker is held at its first call, so the stop lands with the merged
    // contract planned and dispatched, not finished.
    let _held = p1.hold_workers("Step 1 of 3");
    let mut services = p1.services(WINDOW);
    services.routes.truncate(1);
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let h = o.handle();
    let run = tokio::spawn(async move { o.start(RunId::new("r1"), "make the files".into()).await });
    assert!(h.status().await.is_some(), "the plan was accepted");
    h.stop().await;
    let _ = run.await.expect("join");
    drop(p1);

    // A fresh process: the plan is not made again, and the one contract ends.
    let p2 = Parts::open_with(
        dir.path(),
        |_| panic!("the resumed run must not decompose again"),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let mut services = p2.services(WINDOW);
    services.routes.truncate(1);
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await;
    // The worker was held through the stop, so the run was not complete: an
    // `AlreadyCompleted` here would mean the resume proved nothing.
    let report = report.expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let store = RunStore::new(p2.graph.clone());
    let record = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert!(record.is_verified("a"), "the merged contract was verified");
    assert!(
        !record.is_verified("b") && !record.is_verified("c"),
        "no link of its own"
    );
    assert_eq!(
        collapsed(&trace(dir.path()).await)
            .into_iter()
            .map(|(_, merged, _)| merged)
            .collect::<Vec<_>>(),
        [["a", "b", "c"]]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_head_keeps_the_dependencies_it_had_outside_the_chain() {
    // o feeds both z and a, so o -> a is not a link; a -> b is.
    let plan = plan_json(&[
        c("o", "make o", "src/o.rs", &[]),
        c("z", "make z", "src/z.rs", &["o"]),
        c("a", "make a", "src/a.rs", &["o"]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let out = run_plan(plan, 1, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert_eq!(ids(&out.events), ["o", "z", "a"]);
    let (contracts, _) = accepted(&out.events);
    assert_eq!(contracts[2].depends_on, ["o"]);
    assert_eq!(collapsed(&out.events)[0].1, ["a", "b"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_review_keeps_the_plan_as_the_planner_wrote_it() {
    // The user reads the planner's plan and may ask for its shape.
    let dir = tempfile::tempdir().expect("dir");
    let plan = chain3();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut services = p.services(WINDOW);
    services.routes.truncate(1);
    let control = std::sync::Arc::new(kgraf_tools::PermissionControl::manual());
    control.set_plan_review(true, kgraf_tools::ModeSource::UserInterface);
    let mut cfg = config();
    cfg.approve_plan = true;
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .with_permissions(control)
        .start(RunId::new("r1"), "make the files".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let events = trace(dir.path()).await;
    assert!(collapsed(&events).is_empty());
    assert_eq!(ids(&events), ["a", "b", "c"]);
    assert!(
        notices(&events)
            .iter()
            .any(|n| n.contains("plan review is on")),
        "{:?}",
        notices(&events)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_order_the_user_chose_keeps_a_later_plan_as_written() {
    let dir = tempfile::tempdir().expect("dir");
    // First plan: two independent contracts, held, then the user orders them.
    let first = plan_json(&[
        c("x", "make x", "src/x.rs", &[]),
        c("y", "make y", "src/y.rs", &[]),
    ]);
    let p1 = Parts::open_with(
        dir.path(),
        move |_| first.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let _held = p1.hold_workers("make");
    let mut services = p1.services(WINDOW);
    services.routes.truncate(1);
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let h = o.handle();
    let run = tokio::spawn(async move { o.start(RunId::new("r1"), "make the files".into()).await });
    assert!(h.status().await.is_some(), "the plan was accepted");
    h.set_order(vec!["y".into(), "x".into()])
        .await
        .expect("order");
    h.stop().await;
    let _ = run.await.expect("join");
    let store = RunStore::new(p1.graph.clone());
    let mut record = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.coordination.user_order, ["y", "x"]);
    // The run is taken back to before its plan (as when it was stopped in the
    // planner call), with the user's order kept: the plan made now is a chain.
    record.planned = false;
    record.open_contracts.clear();
    record.plan_progress = None;
    store.save(&record).expect("save");
    drop(store);
    drop(p1);

    let second = chain3();
    let p2 = Parts::open_with(
        dir.path(),
        move |_| second.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let mut services = p2.services(WINDOW);
    services.routes.truncate(1);
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let h = o.handle();
    let run = tokio::spawn(async move { o.resume(RunId::new("r1")).await });
    assert!(h.status().await.is_some(), "the second plan was accepted");
    h.stop().await;
    let _ = run.await.expect("join");
    let events = trace(dir.path()).await;
    assert!(collapsed(&events).is_empty());
    let plans: Vec<Vec<String>> = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PlanAccepted { contracts, .. } => {
                Some(contracts.iter().map(|c| c.id.clone()).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(plans.last().expect("second plan"), &["a", "b", "c"]);
    assert!(
        notices(&events)
            .iter()
            .any(|n| n.contains("the user chose an order")),
        "{:?}",
        notices(&events)
    );
}

/// An instruction whose own part of the work block (`bound_tokens`: half its
/// length) is about `tokens`.
fn sized(tokens: usize) -> String {
    "pad ".repeat(tokens / 2)
}

/// Own work per large step: over a quarter of the plan target (40k under the
/// 200k window, so 10k), and two of them with the projected growth still under
/// the target, so the size rule alone would merge them.
const LARGE: usize = 10_500;

fn kept_split(events: &[TraceEvent]) -> Vec<String> {
    notices(events)
        .into_iter()
        .filter(|n| n.contains("kept split:"))
        .collect()
}

/// A chain of code where both steps are large keeps its link: producer and
/// consumer can run at once against the producer's interface stub (0081),
/// which a merge would give up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_chain_of_two_large_steps_stays_split_and_says_why() {
    let plan = plan_json(&[
        c("a", &sized(LARGE), "src/a.rs", &[]),
        c("b", &sized(LARGE), "src/b.rs", &["a"]),
    ]);
    let out = run_plan(plan, 1, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert!(
        collapsed(&out.events).is_empty(),
        "{:?}",
        notices(&out.events)
    );
    assert_eq!(ids(&out.events), ["a", "b"]);
    let kept = kept_split(&out.events);
    assert_eq!(kept.len(), 1, "{:?}", notices(&out.events));
    assert!(
        kept[0].contains(
            "link a -> b kept split: a writes interface-checkable code (src/a.rs) and both steps \
             are large enough to run in parallel against its stub"
        ),
        "{kept:?}"
    );
    assert!(kept[0].contains("of the plan target of 40000"), "{kept:?}");
}

/// One small step is enough to merge: the parallel start saves at most the
/// shorter step's time, which a small step does not repay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_chain_with_a_small_step_is_one_contract() {
    for (first, second) in [(LARGE, 100), (100, LARGE)] {
        let plan = plan_json(&[
            c("a", &sized(first), "src/a.rs", &[]),
            c("b", &sized(second), "src/b.rs", &["a"]),
        ]);
        let out = run_plan(plan, 1, WINDOW, config()).await;
        assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
        assert!(kept_split(&out.events).is_empty(), "{first} {second}");
        assert_eq!(ids(&out.events), ["a"], "{first} {second}");
        assert_eq!(
            collapsed(&out.events),
            [("a".into(), vec!["a".into(), "b".into()], "alpha".into())]
        );
    }
}

/// A producer that writes no file an interface can check has no stub to start
/// its consumer against, so its link merges however large the steps are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_without_checkable_code_is_one_contract_whatever_its_size() {
    // The consumer writes code; only the producer's writes decide.
    let plan = plan_json(&[
        c("a", &sized(LARGE), "docs/a.md", &[]),
        c("b", &sized(LARGE), "src/b.rs", &["a"]),
    ]);
    let out = run_plan(plan, 1, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert!(kept_split(&out.events).is_empty());
    assert_eq!(ids(&out.events), ["a"]);
    assert_eq!(collapsed(&out.events)[0].1, ["a", "b"]);
}

/// A chain is cut only at the link that qualifies; the rest of it merges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mixed_chain_splits_only_at_the_qualifying_link() {
    // a -> b: both large, a writes code: kept. b -> c: c is small: merged.
    let plan = plan_json(&[
        c("a", &sized(LARGE), "src/a.rs", &[]),
        c("b", &sized(LARGE), "src/b.rs", &["a"]),
        c("c", &sized(100), "src/c.rs", &["b"]),
    ]);
    let out = run_plan(plan, 1, WINDOW, config()).await;
    assert_eq!(out.report.status, RunStatus::Completed, "{:?}", out.report);
    assert_eq!(ids(&out.events), ["a", "b"]);
    assert_eq!(
        collapsed(&out.events),
        [("b".into(), vec!["b".into(), "c".into()], "alpha".into())]
    );
    let (contracts, _) = accepted(&out.events);
    assert_eq!(contracts[1].depends_on, ["a"]);
    let kept = kept_split(&out.events);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(
        kept[0].starts_with("straight chain a -> b -> c: link a -> b kept split"),
        "{kept:?}"
    );
}
