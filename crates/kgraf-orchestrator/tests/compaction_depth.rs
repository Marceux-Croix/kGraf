//! Graceful degradation at the delegation depth cap (decision 0095).
//!
//! A worker whose context passes its stop point hands its remainder back, and
//! the remainder becomes a child contract (`<parent>.hN`) through the
//! delegation guards. A chain of hand-backs ends at the depth cap, where the
//! re-split is refused and the contract was held. Now the worker at the cap is
//! spawned to compact once instead, and goes on; one that overflows again is
//! held with its work and its full transcript named.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{ContractStatus, Orchestrator, RunStatus, RunStore, TraceEvent, TraceLog};
use std::time::Duration;
use support::*;

/// Window 10,000: stop point 8,000; an `ALWAYSBIG` round 0 reports 9,000.
const WINDOW: u64 = 10_000;

fn plan(instruction: &str) -> String {
    serde_json::json!({"contracts":[
        {"id":"big","instruction":instruction,"acceptance":"src/big.rs exists",
         "writes":[{"path":"src/big.rs"}],"tools":["write_file"]}
    ]})
    .to_string()
}

async fn trace(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_at_the_depth_cap_compacts_and_finishes_instead_of_holding() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan("ALWAYSBIG part WRITE src/big.rs");
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut cfg = config();
    cfg.max_split_depth = 0;
    let o = Orchestrator::new(cfg, p.services(WINDOW)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "a big part".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    // Hand-backs while the guards allow a child, then one compaction at the
    // cap, on the deepest contract, which then finished.
    assert_eq!(record.compactions.len(), 1, "{:?}", record.compactions);
    let entry = &record.compactions[0];
    assert!(entry.sizing_miss, "the contract was larger than planned");
    assert!(entry.contract.starts_with("big.h1"), "{entry:?}");
    assert_eq!(report.verified, vec![entry.contract.clone()]);
    let path = entry
        .notice
        .transcript_path
        .as_ref()
        .expect("the transcript is named");
    assert!(
        path.is_file(),
        "the transcript is on disk: {}",
        path.display()
    );
    let text = std::fs::read_to_string(path).expect("read");
    let at = text
        .find("\"compacted\"")
        .expect("the notice is in the transcript");
    assert!(
        text[..at].contains("tool_call_started") && text[..at].contains("src/big.rs"),
        "the pre-compaction round is kept before the notice: {text}"
    );
    // Every hand-back and the compaction are sizing misses, one per worker.
    let handbacks = trace(&p)
        .await
        .iter()
        .filter(
            |e| matches!(e, TraceEvent::WorkerFinished { outcome, .. } if outcome == "handed_back"),
        )
        .count() as u64;
    assert!(
        handbacks >= 2,
        "a chain of hand-backs ran first: {handbacks}"
    );
    assert_eq!(record.counters.sizing_misses, handbacks + 1);
    assert!(trace(&p).await.iter().any(|e| matches!(
        e,
        TraceEvent::WorkerCompacted { contract, .. } if *contract == entry.contract
    )));
    assert!(trace(&p).await.iter().any(|e| matches!(
        e,
        TraceEvent::WorkerFinished { contract, sizing_miss: true, outcome, .. }
            if *contract == entry.contract && outcome == "completed"
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compacted_worker_that_overflows_again_is_held_with_its_work_and_transcript_named() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan("ALWAYSBIG STUBBORN part WRITE src/big.rs");
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut cfg = config();
    cfg.max_split_depth = 0;
    let o = Orchestrator::new(cfg, p.services(WINDOW)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "a big part".into())
        .await
        .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.compactions.len(), 1, "compacted once, never twice");
    let entry = &record.compactions[0];
    let held = record.contract(&entry.contract).expect("still open");
    let ContractStatus::Held { reason } = &held.status else {
        panic!("expected a hold, got {:?}", held.status);
    };
    assert!(reason.contains("re-split refused"), "{reason}");
    assert!(reason.contains("already compacted once"), "{reason}");
    assert!(
        reason.contains(".jsonl"),
        "the transcript is named: {reason}"
    );
    assert!(
        held.workspace.is_some(),
        "the worktree with its work is kept"
    );
}
