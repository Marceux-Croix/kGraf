//! Timing changes nothing the run does, checked against the code before timing
//! existed.
//!
//! The scenario below (six contracts, three dependency edges, orderings that
//! are free within a wave) was run on the commit this work branched from
//! (`12fb1f76`, the merge base with `main`) with this same test body and
//! nothing else changed. What it recorded is pinned here: the dispatch order
//! and the ledger's calls by agent and kind. The same run on this tree, with
//! timing on and with it off, must match it.
//!
//! The order is pinned as the constraints the loop guarantees plus the order
//! the baseline produced: the first wave is `a`, `c`, `e` in plan order; a
//! dependent is dispatched only after its dependencies; `b` and `d` may swap.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, OrchestratorConfig, RunStatus, TraceEvent, TraceLog};
use std::collections::BTreeMap;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// The ledger calls the baseline made in this scenario, by `<agent>:<kind>`.
/// Agent is the first `-` or `:` separated word of the agent id.
const BASELINE_CALLS: &[(&str, usize)] = &[
    ("orchestrator:decompose", 1),
    ("orchestrator:reground", 1),
    ("r1:worker_round", 12),
];

/// The dispatch order the baseline traced.
const BASELINE_ORDER: &[&str] = &["a", "c", "e", "b", "d", "f"];

fn config() -> OrchestratorConfig {
    OrchestratorConfig {
        tick: Duration::from_millis(10),
        step_budget: 6,
        speculate_every: None,
        ..OrchestratorConfig::default()
    }
}

async fn scenario(timing_off: bool) -> (Vec<String>, BTreeMap<String, usize>) {
    scenario_with(timing_off, config()).await
}

async fn scenario_with(
    timing_off: bool,
    cfg: OrchestratorConfig,
) -> (Vec<String>, BTreeMap<String, usize>) {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
        c("c", "make c", "src/c.rs", &[]),
        c("d", "make d", "src/d.rs", &["c"]),
        c("e", "make e", "src/e.rs", &[]),
        c("f", "make f", "src/f.rs", &["b", "d"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(60),
    );
    let mut o = Orchestrator::new(cfg, p.services(WINDOW)).expect("orchestrator");
    if timing_off {
        o = o.without_latency_events();
    }
    let report = o
        .start(RunId::new("r1"), "make a to f".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let trace = TraceLog::open(&dir.path().join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let order = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Dispatch { contract, .. } => Some(contract.clone()),
            _ => None,
        })
        .collect();
    let mut calls: BTreeMap<String, usize> = BTreeMap::new();
    for call in p.ledger.calls().expect("calls") {
        let agent = call.agent_id.split(&['-', ':'][..]).next().unwrap_or("");
        let key = format!("{agent}:{}", call.trace.kind.unwrap_or_default());
        *calls.entry(key).or_default() += 1;
    }
    (order, calls)
}

fn position(order: &[String], c: &str) -> usize {
    order.iter().position(|x| x == c).expect("dispatched")
}

fn check(order: &[String], calls: &BTreeMap<String, usize>) {
    println!("GOLDEN order={order:?} calls={calls:?}");
    assert_eq!(order.len(), 6, "{order:?}");
    for (c, deps) in [("b", &["a"][..]), ("d", &["c"]), ("f", &["b", "d"])] {
        for d in deps {
            assert!(
                position(order, d) < position(order, c),
                "{c} after {d}: {order:?}"
            );
        }
    }
    assert_eq!(
        &order[..3],
        ["a", "c", "e"],
        "the first wave is in plan order"
    );
    assert_eq!(order[5], "f");
    let mut got: Vec<&str> = order.iter().map(String::as_str).collect();
    got.sort_unstable();
    assert_eq!(got, ["a", "b", "c", "d", "e", "f"]);
    if !BASELINE_ORDER.is_empty() {
        let mut base = order[3..5].to_vec();
        base.sort();
        let mut want: Vec<String> = BASELINE_ORDER[3..5]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        want.sort();
        assert_eq!(base, want);
        assert_eq!(&order[..3], &BASELINE_ORDER[..3]);
    }
    let want: BTreeMap<String, usize> = BASELINE_CALLS
        .iter()
        .map(|(k, n)| ((*k).to_owned(), *n))
        .collect();
    assert_eq!(calls, &want, "the ledger's calls by agent and kind");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_with_timing_matches_the_run_before_timing_existed() {
    let (order, calls) = scenario(false).await;
    check(&order, &calls);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_with_timing_off_matches_it_too() {
    let (order, calls) = scenario(true).await;
    check(&order, &calls);
}

/// `--early-nonparty` with no interface checks has no party to move: the
/// order, and the ledger's calls, are the baseline's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn early_nonparty_with_no_interface_checks_matches_the_baseline() {
    let cfg = OrchestratorConfig {
        early_nonparty: true,
        ..config()
    };
    let (order, calls) = scenario_with(false, cfg).await;
    check(&order, &calls);
}
