//! A brief whose pinned context (prefix, slices, instruction) reaches the
//! worker's stop point cannot run a round, and the same brief fails the same
//! way on every attempt. The contract is held at once with the remedy, not
//! retried until the failure count runs out.
//!
//! Round evidence: a worker's pinned context of 21,001 tokens against a stop
//! point of 11,468 failed an attempt and was retried as a second attempt with
//! the same result.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus, TraceEvent, TraceLog};
use std::time::Duration;
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contract_whose_pinned_context_cannot_fit_is_held_after_one_attempt() {
    let dir = tempfile::tempdir().expect("dir");
    // About 15,000 tokens of instruction against a 6,000-token window.
    let padding = "the quick brown fox jumps over the lazy dog ".repeat(1_500);
    let plan = serde_json::json!({"contracts":[
        {"id":"big","instruction":format!("write the file WRITE src/big.rs {padding}"),
         "acceptance":"src/big.rs exists",
         "writes":[{"path":"src/big.rs"}],"tools":["write_file"]}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(1),
    );
    let o = Orchestrator::new(config(), p.services(6_000)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "write the file".into())
        .await
        .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    let (id, reason) = report.held.first().expect("the contract is held");
    assert_eq!(id, "big");
    assert!(reason.contains("pinned context"), "{reason}");
    assert!(reason.contains("not retried"), "{reason}");
    assert!(reason.contains("Split the contract"), "{reason}");
    assert!(
        !reason.contains("attempts failed"),
        "not the retry-count reason: {reason}"
    );
    let trace = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let failed = trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::WorkerFinished { outcome, .. } if outcome == "failed"))
        .count();
    assert_eq!(failed, 1, "one attempt, no second one");
    assert!(
        p.stats.requests().is_empty(),
        "no worker model call was made"
    );
}
