//! Decision 0022's cross-feature pairing preference, driven through the loop
//! (the effect, not the setting): with one work-item slot left beside
//! a running contract, the scheduler fills it with work from another feature
//! rather than with the running contract's sibling.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Limits, Orchestrator, RunStatus, TraceEvent, TraceLog};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

async fn dispatched(p: &Parts) -> Vec<String> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
        .into_iter()
        .filter_map(|e| match e {
            TraceEvent::Dispatch { contract, .. } => Some(contract),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_slot_beside_a_running_contract_goes_to_another_feature_not_a_sibling() {
    let dir = tempfile::tempdir().expect("dir");
    // Planned order a, b, c. a and b are two parts of one feature (they share
    // "parser", "tokenize" and "literals"); c is another feature.
    let plan = plan_json(&[
        c("a", "parser: tokenize string literals", "a.txt", &[]),
        c("b", "parser: tokenize numeric literals", "b.txt", &[]),
        c("c", "billing: export invoices", "c.txt", &[]),
    ]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(150),
    );
    p.admission = admission(Limits::new(2, 8).expect("limits"));
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "three edits".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let order = dispatched(&p).await;
    // The first planned contract goes first; the second slot goes to the
    // cross-feature contract, and the sibling waits for a slot.
    assert_eq!(order, vec!["a", "c", "b"], "{order:?}");
    assert!(
        p.stats
            .overlapped("parser: tokenize string", "billing: export"),
        "a and c ran concurrently"
    );
}
