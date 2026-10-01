//! Progress-gated round budgets, seen from the run loop (decision 0096).
//!
//! A worker that keeps producing results runs past its round budget in the
//! same attempt; the orchestrator records that once as a sizing miss, traces
//! the checkpoints, and counts the extension.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus, RunStore, TraceEvent, TraceLog};
use std::time::Duration;
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_productive_worker_past_its_budget_finishes_in_one_attempt_with_one_sizing_notice() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = serde_json::json!({"contracts":[
        {"id":"long","instruction":"LONGRUN part WRITE src/long.rs",
         "acceptance":"src/long.rs exists",
         "writes":[{"path":"src/long.rs"}],"tools":["write_file"]}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(1),
    );
    // config(): six rounds per worker; the worker needs eleven.
    let o = Orchestrator::new(config(), p.services(200_000)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "a long part".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["long".to_owned()]);

    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.counters.sizing_misses, 1, "one miss per worker");
    assert_eq!(record.counters.round_extensions, 1);
    assert_eq!(record.counters.checkpoint_stops, 0);

    let trace = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let finished: Vec<(u32, bool, String)> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::WorkerFinished {
                steps,
                sizing_miss,
                outcome,
                ..
            } => Some((*steps, *sizing_miss, outcome.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![(11, true, "completed".to_owned())],
        "one worker, one attempt, past its six rounds"
    );
    let checkpoints: Vec<bool> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::WorkerCheckpoints { extended, .. } => Some(*extended),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints, vec![true], "one notice for the worker");
}
