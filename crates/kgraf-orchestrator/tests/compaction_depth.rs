//! Graceful degradation at the delegation depth cap (decision 0095).
//!
//! A worker whose context passes its stop point hands its remainder back, and
//! the remainder becomes a child contract (`<parent>.hN`) through the
//! delegation guards. A chain of hand-backs ends at the depth cap, where the
//! re-split is refused and the contract was held. Now the worker at the cap is
//! spawned to compact instead, and goes on. At a later stop point it compacts
//! again while it makes measured progress (amendment of 2026-10-06); one that
//! overflows again with no progress has its tree verified as it stands: a
//! pass merges it, a failure holds it with the reason, its work and its full
//! transcript named.

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

/// A compacted worker that overflows again with no progress hands its work
/// back, and its re-split is refused. Its tree already holds the contract's
/// file: it is verified as it stands instead of held unjudged, passes, and
/// merges, with a notice naming why it was verified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compacted_worker_that_overflows_again_is_verified_and_merges_when_its_tree_passes() {
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
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.compactions.len(), 1, "compacted once, never twice");
    let entry = &record.compactions[0];
    assert_eq!(report.verified, vec![entry.contract.clone()]);
    let notice = report
        .notices
        .iter()
        .find(|n| n.contains("handed its work back, and no continuation will run"))
        .unwrap_or_else(|| panic!("{:?}", report.notices));
    assert!(notice.contains("re-split refused"), "{notice}");
    assert!(notice.contains("accepted as verified"), "{notice}");
    assert!(trace(&p).await.iter().any(|e| matches!(
        e,
        TraceEvent::Verdict { contract, passed: true, .. } if *contract == entry.contract
    )));
}

/// As above, but the tree fails its check: the contract is held as before,
/// with its work and transcript named, and the hold says the tree was
/// verified and failed.
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
    *p.verifier.require.lock().expect("lock") = Some("NEVER-WRITTEN".into());
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
    assert!(
        reason
            .contains("its tree was verified as it stood and failed: src/big.rs missing or wrong"),
        "{reason}"
    );
    assert!(reason.contains("already compacted once"), "{reason}");
    // It wrote the same content again after its compaction: no net change,
    // so it was not compacted again, and the reason says so.
    assert!(
        reason.contains("no measured progress since compaction 1"),
        "{reason}"
    );
    assert!(trace(&p).await.iter().any(|e| matches!(
        e,
        TraceEvent::Compaction {
            compaction_stop: Some(kgraf_runtime::CompactionStop::NoProgress { .. }),
            ..
        }
    )));
    assert!(
        reason.contains(".jsonl"),
        "the transcript is named: {reason}"
    );
    assert!(
        held.workspace.is_some(),
        "the worktree with its work is kept"
    );
}

/// A worker at the depth cap that keeps writing new content passes its stop
/// point again after its compaction. It made measured progress since, so it
/// is compacted again instead of held, and finishes: two compactions, one
/// sizing miss for the worker, and the contract verified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compacted_worker_that_keeps_making_progress_compacts_again_and_finishes() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan("ALWAYSBIG PERSISTENT part WRITE src/big.rs");
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
    let deepest: Vec<_> = record
        .compactions
        .iter()
        .filter(|c| c.contract == record.compactions[0].contract)
        .collect();
    assert_eq!(deepest.len(), 2, "{:?}", record.compactions);
    assert_eq!(
        deepest.iter().map(|c| c.notice.number).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(deepest.iter().all(|c| c.worker == deepest[0].worker));
    assert_eq!(report.verified, vec![deepest[0].contract.clone()]);
    let events = trace(&p).await;
    let compacted = events
        .iter()
        .filter(|e| matches!(e, TraceEvent::WorkerCompacted { contract, .. } if *contract == deepest[0].contract))
        .count();
    assert_eq!(compacted, 2, "each compaction is traced");
    // One sizing miss per worker, however often it compacted.
    let handbacks = events
        .iter()
        .filter(
            |e| matches!(e, TraceEvent::WorkerFinished { outcome, .. } if outcome == "handed_back"),
        )
        .count() as u64;
    assert_eq!(record.counters.sizing_misses, handbacks + 1);
}
