//! Interaction timing (decision 0100, section 1): a run writes a
//! `dispatch_latency` event for every dispatch attempt, a `dispatch_gap` event
//! per contract, and `latency` events for the interactions it measures, each
//! summing to its wall time. Measuring changes nothing the run does.
//!
//! Time is never asserted here, only presence, order and sums: a stub run on a
//! busy host has no stable wall time.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::latency::LatencyKind;
use kgraf_orchestrator::{
    Limits, Orchestrator, OrchestratorConfig, RunReport, RunStatus, TraceEvent, TraceLog,
};
use std::collections::BTreeMap;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// The steps of one successful dispatch of a one-worker contract, in the order
/// they end.
const DISPATCH_STEPS: [&str; 18] = [
    "time_limit_gate",
    "contract_gate",
    "reground",
    "refresh_slices",
    "route_pick",
    "readiness",
    "cluster_cap",
    "admission",
    "sizing",
    "leases",
    "budget_and_record",
    "record_save",
    "integration_tip",
    "worktree_prepare",
    "worktree_record",
    "brief_build",
    "worker_spawn",
    "dispatch_trace",
];

fn config() -> OrchestratorConfig {
    OrchestratorConfig {
        tick: Duration::from_millis(10),
        step_budget: 6,
        // Speculation has its own admissions and trace events.
        speculate_every: None,
        ..OrchestratorConfig::default()
    }
}

struct Outcome {
    report: RunReport,
    trace: Vec<TraceEvent>,
    status_text: String,
    /// Contracts in the order their dispatch was traced.
    dispatch_order: Vec<String>,
    /// Ledger rows per call kind.
    ledger_calls: BTreeMap<String, usize>,
}

/// One stub run: `a`, `x` (independent) and `b` (after `a`), with a note sent
/// while workers run.
async fn stub_run(instrumented: bool) -> Outcome {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("x", "make x", "src/x.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(120),
    );
    let mut o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    if !instrumented {
        o = o.without_latency_events();
    }
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a, x and b".into()));
    tokio::time::sleep(Duration::from_millis(60)).await;
    handle.steer("prefer small functions").await.expect("steer");
    let status_text = handle.status().await.map(|s| s.text).unwrap_or_default();
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let trace = TraceLog::open(&dir.path().join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let dispatch_order = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Dispatch { contract, .. } => Some(contract.clone()),
            _ => None,
        })
        .collect();
    let mut ledger_calls: BTreeMap<String, usize> = BTreeMap::new();
    for call in p.ledger.calls().expect("calls") {
        let key = format!(
            "{}:{}",
            call.agent_id.split(&['-', ':'][..]).next().unwrap_or(""),
            call.trace.kind.unwrap_or_default()
        );
        *ledger_calls.entry(key).or_default() += 1;
    }
    Outcome {
        report,
        trace,
        status_text,
        dispatch_order,
        ledger_calls,
    }
}

fn latency_events(trace: &[TraceEvent]) -> Vec<&kgraf_orchestrator::latency::LatencyEvent> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Latency(l) => Some(l),
            _ => None,
        })
        .collect()
}

/// Every dispatch attempt is timed, step by step, in order, and the steps and
/// the residual sum to the total. The attempt that dispatched passes every
/// step; every contract has one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_dispatch_attempt_is_timed_in_order_and_sums() {
    let out = stub_run(true).await;
    let attempts: Vec<_> = out
        .trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::DispatchLatency(d) => Some(d),
            _ => None,
        })
        .collect();
    assert!(!attempts.is_empty());
    for d in &attempts {
        assert!(d.sums(), "{d:?}");
        assert!(d.total_us >= 0 && d.residual_us >= 0);
        let offsets: Vec<i64> = d.steps.iter().map(|s| s.offset_us).collect();
        assert!(offsets.windows(2).all(|w| w[0] <= w[1]), "{d:?}");
        assert!(
            ["dispatched", "held", "skipped", "error"].contains(&d.outcome.as_str())
                || d.outcome.starts_with("deferred:"),
            "{}",
            d.outcome
        );
    }
    for contract in ["a", "x", "b"] {
        let done: Vec<_> = attempts
            .iter()
            .filter(|d| d.contract == contract && d.outcome == "dispatched")
            .collect();
        assert_eq!(done.len(), 1, "{contract}: one dispatching attempt");
        let names: Vec<&str> = done[0].steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(names, DISPATCH_STEPS, "{contract}");
    }
    // The attempt is traced after the dispatch it describes.
    let first_dispatch = out
        .trace
        .iter()
        .position(|e| matches!(e, TraceEvent::Dispatch { .. }))
        .expect("a dispatch");
    let first_timing = out
        .trace
        .iter()
        .position(|e| matches!(e, TraceEvent::DispatchLatency(_)))
        .expect("a timing");
    assert!(first_dispatch < first_timing);
}

/// The measured interactions each have an event whose parts sum to the wall
/// time; the gap between the last planner call and the first worker call is
/// written once per contract; `/status` and the report carry the newest
/// split per kind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measured_interactions_are_written_with_parts_that_sum() {
    let out = stub_run(true).await;
    let events = latency_events(&out.trace);
    assert!(events.iter().all(|e| e.sums()), "{events:?}");
    assert!(
        events
            .iter()
            .all(|e| e.kgraf_us >= 0 && e.model_us >= 0 && e.tools_us >= 0)
    );
    let kinds: Vec<LatencyKind> = events.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&LatencyKind::NoteAck), "{kinds:?}");
    assert!(kinds.contains(&LatencyKind::ContractMerged), "{kinds:?}");
    assert!(
        kinds.iter().all(|k| k.is_emitted()),
        "only the kinds the build measures: {kinds:?}"
    );
    let merged: Vec<_> = events
        .iter()
        .filter(|e| e.kind == LatencyKind::ContractMerged)
        .collect();
    assert_eq!(merged.len(), 3, "one per contract: {merged:?}");
    assert!(
        merged.iter().all(|e| e.model_us > 0),
        "the worker's calls are model time: {merged:?}"
    );
    for contract in ["a", "x", "b"] {
        let gaps: Vec<_> = out
            .trace
            .iter()
            .filter_map(|e| match e {
                TraceEvent::DispatchGap {
                    contract: c,
                    planner_ended_ms,
                    worker_started_ms,
                    gap_us,
                    ..
                } if c == contract => Some((*planner_ended_ms, *worker_started_ms, *gap_us)),
                _ => None,
            })
            .collect();
        assert_eq!(gaps.len(), 1, "{contract}: one gap");
        let (ended, started, gap) = gaps[0];
        assert!(ended <= started);
        assert_eq!(gap, (started - ended) * 1_000);
    }
    // No contract or gate clock outlives its contract or gate.
    assert_eq!(out.report.latency_clocks_held, 0);
    // Each contract's dispatch split is written, and it sums.
    for contract in ["a", "x", "b"] {
        assert!(
            events
                .iter()
                .any(|e| e.kind == LatencyKind::SmallRequestDispatch
                    && e.contract.as_deref() == Some(contract)),
            "{contract}"
        );
    }
    // `/status` printed the note's split, one line.
    assert!(
        out.status_text.contains("latency note_ack (B1):"),
        "{}",
        out.status_text
    );
    assert!(
        out.report
            .latency
            .iter()
            .any(|l| l.starts_with("latency contract_merged (B10):")),
        "{:?}",
        out.report.latency
    );
    let kinds_in_report: Vec<&str> = out
        .report
        .latency
        .iter()
        .filter_map(|l| l.split(' ').nth(1))
        .collect();
    let mut unique = kinds_in_report.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), kinds_in_report.len(), "one line per kind");
}

/// Measuring is output only: the same stub run with the timing off dispatches
/// in the same order and makes the same ledger calls, and writes none of the
/// new events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measuring_changes_neither_dispatch_order_nor_ledger_calls() {
    let on = stub_run(true).await;
    let off = stub_run(false).await;
    assert_eq!(
        on.dispatch_order,
        ["a", "x", "b"].map(String::from).to_vec()
    );
    assert_eq!(on.dispatch_order, off.dispatch_order);
    assert_eq!(on.ledger_calls, off.ledger_calls);
    assert_eq!(
        on.report.counters.dispatches,
        off.report.counters.dispatches
    );
    assert_eq!(on.report.verified.len(), off.report.verified.len());
    assert!(!off.trace.iter().any(|e| matches!(
        e,
        TraceEvent::Latency(_) | TraceEvent::DispatchLatency(_) | TraceEvent::DispatchGap { .. }
    )));
    assert!(off.report.latency.is_empty());
    assert!(!off.status_text.contains("latency"));
}

/// A contract that waited for a slot says so: with room for one agent, `x` is
/// deferred while `a` works, and its dispatch split names that wait as
/// `capacity_wait_us`, apart from kGraf's own time. `a` waited for nothing.
/// The parts sum, and the `dispatch_gap` event carries the same figures and
/// the moment the contract became eligible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_for_a_slot_is_named_apart_from_kgrafs_own_time() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("x", "make x", "src/x.rs", &[]),
    ]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(120),
    );
    p.admission = admission(Limits::new(1, 1).expect("limits"));
    let report = tokio::time::timeout(
        Duration::from_secs(60),
        Orchestrator::new(config(), p.services(WINDOW))
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a and x".into()),
    )
    .await
    .expect("the run ended")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let trace = TraceLog::open(&dir.path().join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let events = latency_events(&trace);
    let b2 = |contract: &str| {
        events
            .iter()
            .find(|e| {
                e.kind == LatencyKind::SmallRequestDispatch
                    && e.contract.as_deref() == Some(contract)
            })
            .copied()
            .unwrap_or_else(|| panic!("no dispatch split for {contract}"))
    };
    let (a, x) = (b2("a"), b2("x"));
    assert!(a.sums() && x.sums(), "{a:?} {x:?}");
    assert_eq!(a.capacity_wait_us, 0, "{a:?}");
    assert!(x.capacity_wait_us > 0, "x waited for a slot: {x:?}");
    assert!(
        x.kgraf_us + x.capacity_wait_us <= x.wall_us && x.kgraf_us >= 0,
        "{x:?}"
    );
    let gap = trace
        .iter()
        .find_map(|e| match e {
            TraceEvent::DispatchGap {
                contract,
                eligible_ms,
                capacity_wait_us,
                checks_us,
                ..
            } if contract == "x" => Some((*eligible_ms, *capacity_wait_us, *checks_us)),
            _ => None,
        })
        .expect("the gap of x");
    assert!(gap.0.is_some(), "a root contract is eligible from the plan");
    assert_eq!((gap.1, gap.2), (x.capacity_wait_us, x.checks_us));
    assert!(
        x.line().contains("slot wait"),
        "the line names the wait: {}",
        x.line()
    );
}
