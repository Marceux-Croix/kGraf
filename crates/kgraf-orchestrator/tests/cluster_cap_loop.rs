//! At most two contracts of one coupling cluster in flight at once (decision
//! 0082). The pure cap arithmetic is unit
//! tested in `cluster_cap.rs`; this exercises it against the real dispatch
//! loop, real leases and a real co-change graph.
//!
//! Contracts here are clustered by a co-change edge (`kgraf_graph::cochange`)
//! between *distinct* write paths, not by write-set overlap: two contracts
//! that write the same path would already serialize on kGraf's own lease
//! rule (0048, "disjoint writes"), which would make a cap assertion trivial
//! and would not be testing this mechanism.

mod support;

use kgraf_core::RunId;
use kgraf_graph::cochange::{HistoryCommit, build_from_commits};
use kgraf_orchestrator::{
    DeferReason, Limits, OrchestratorConfig, RunStatus, RunStore, TraceEvent, TraceLog,
};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;
/// Long enough that three dispatched workers overlap in wall-clock time
/// (`Stats::max_in_flight` needs the overlap to observe anything).
const DELAY: Duration = Duration::from_millis(250);

fn config(max_in_flight_per_cluster: usize) -> OrchestratorConfig {
    OrchestratorConfig {
        tick: Duration::from_millis(10),
        step_budget: 6,
        // Speculation is a different mechanism (0074) with its own
        // work-item admission; off here so its trace events and slot use
        // cannot be mistaken for the cluster cap's.
        speculate_every: None,
        max_in_flight_per_cluster,
        ..OrchestratorConfig::default()
    }
}

/// Seed a co-change edge at or above `routing::CLUSTER_COCHANGE_MIN_COUNT`
/// between every consecutive pair in `paths`, chaining them into one cluster
/// (`cluster_contracts`'s union-find joins a-b and b-c into one tree even
/// with no direct a-c edge).
fn seed_cochange(p: &Parts, paths: &[&str]) {
    let commits: Vec<HistoryCommit> = paths
        .windows(2)
        .flat_map(|w| {
            std::iter::repeat_n(
                HistoryCommit {
                    timestamp: 1,
                    paths: vec![w[0].to_owned(), w[1].to_owned()],
                },
                kgraf_orchestrator::routing::CLUSTER_COCHANGE_MIN_COUNT as usize,
            )
        })
        .collect();
    let g = p.graph.lock().expect("lock");
    build_from_commits(&g, &commits, 1000).expect("build cochange");
}

async fn run(dir: &Path, cluster_cap: usize, seed: &[&str], contracts: &[C<'_>]) -> (Parts, u64) {
    let plan = plan_json(contracts);
    let mut p = Parts::open(dir, move |_| plan.clone(), |_| SERVES.into(), DELAY);
    // A work-item cap well above the contract count: only the cluster cap
    // under test should ever hold a contract back.
    p.admission = admission(Limits::new(8, 8).expect("limits"));
    if !seed.is_empty() {
        seed_cochange(&p, seed);
    }
    let services = p.services(WINDOW);
    let o =
        kgraf_orchestrator::Orchestrator::new(config(cluster_cap), services).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "edits".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let max_in_flight = p.stats.max_in_flight.load(Ordering::SeqCst) as u64;
    (p, max_in_flight)
}

async fn deferred_events(p: &Parts) -> Vec<(String, DeferReason, String)> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
        .into_iter()
        .filter_map(|e| match e {
            TraceEvent::Deferred {
                contract,
                reason,
                detail,
                ..
            } => Some((contract, reason, detail)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_coupled_contracts_run_at_most_two_at_once_and_all_finish() {
    let dir = tempfile::tempdir().expect("dir");
    let (p, max_in_flight) = run(
        dir.path(),
        2,
        &["a.txt", "b.txt", "c.txt"],
        &[
            c("a", "edit a", "a.txt", &[]),
            c("b", "edit b", "b.txt", &[]),
            c("x", "edit x", "c.txt", &[]),
        ],
    )
    .await;
    assert!(
        max_in_flight <= 2,
        "cluster cap of 2 should never let a third coupled contract run; saw {max_in_flight}"
    );
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.verified_contracts.len(), 3, "all three finished");
    assert!(
        record.counters.cluster_cap_refusals > 0,
        "the third contract must have been deferred by the cap at least once"
    );

    // Deferral is traced, naming the cluster and the cap; the
    // reason is `Capacity` (0082 reuses it rather than adding a `DeferReason`
    // variant that `orchestrator/coord.rs::wait_of_defer` would need to match
    // exhaustively, out of this change's scope), so the detail
    // string is what tells a cluster-cap deferral apart from a work-item one.
    let deferrals = deferred_events(&p).await;
    let cluster_deferrals: Vec<_> = deferrals
        .iter()
        .filter(|(_, reason, detail)| {
            *reason == DeferReason::Capacity && detail.starts_with("cluster cap:")
        })
        .collect();
    assert!(!cluster_deferrals.is_empty(), "{deferrals:?}");
    for (_, _, detail) in &cluster_deferrals {
        assert!(detail.contains("already in flight"), "{detail}");
        assert!(detail.contains("cap 2"), "{detail}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncoupled_contracts_are_not_limited_by_the_cluster_cap() {
    let dir = tempfile::tempdir().expect("dir");
    // No co-change seed and no shared write paths or dependencies: three
    // singleton clusters, each of size one, so the cap of 2 never applies.
    let (p, max_in_flight) = run(
        dir.path(),
        2,
        &[],
        &[
            c("a", "edit a", "a.txt", &[]),
            c("b", "edit b", "b.txt", &[]),
            c("x", "edit x", "c.txt", &[]),
        ],
    )
    .await;
    assert_eq!(
        max_in_flight, 3,
        "three uncoupled contracts should all run at once"
    );
    let deferrals = deferred_events(&p).await;
    assert!(
        !deferrals
            .iter()
            .any(|(_, _, detail)| detail.starts_with("cluster cap:")),
        "{deferrals:?}"
    );
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.counters.cluster_cap_refusals, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cluster_cap_of_zero_is_floored_to_one_and_never_stalls() {
    let dir = tempfile::tempdir().expect("dir");
    let (p, max_in_flight) = run(
        dir.path(),
        0,
        &["a.txt", "b.txt"],
        &[
            c("a", "edit a", "a.txt", &[]),
            c("b", "edit b", "b.txt", &[]),
        ],
    )
    .await;
    assert_eq!(
        max_in_flight, 1,
        "a configured cap of 0 is floored to 1 (0082), never silently stalling the run"
    );
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.verified_contracts.len(), 2, "both finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_contract_is_admitted_once_the_cluster_drains() {
    // Three coupled contracts with a cap of 1: strictly serialized, so this
    // is a direct test of the drain path (a held contract's turn comes only
    // after every earlier member of its cluster leaves `in_flight`) rather
    // than a timing inference from `max_in_flight`.
    let dir = tempfile::tempdir().expect("dir");
    let (p, max_in_flight) = run(
        dir.path(),
        1,
        &["a.txt", "b.txt", "c.txt"],
        &[
            c("a", "edit a", "a.txt", &[]),
            c("b", "edit b", "b.txt", &[]),
            c("x", "edit x", "c.txt", &[]),
        ],
    )
    .await;
    assert_eq!(max_in_flight, 1);
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(
        record.verified_contracts.len(),
        3,
        "every deferred contract was eventually admitted and finished; no deadlock"
    );
    assert!(record.counters.cluster_cap_refusals >= 2);
}
