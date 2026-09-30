//! Decision 0020: the orchestrator records what it observes. Every finished
//! attempt's write set lands in `task_writes`, and a merge conflict between two
//! contracts on one path lands in `collisions`, so the co-change relation
//! and the observed-collision count grow from real runs.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// `a` writes `src/shared.rs` and merges. `b`, which depends on `a` and writes
/// the same path, conflicts once at merge-back and is resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_attempts_writes_and_a_merge_conflict_are_recorded() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/shared.rs", &[]),
        c("b", "make b", "src/shared.rs", &["a"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    integration
        .conflict_once
        .lock()
        .expect("lock")
        .insert("b".into());
    let mut cfg = config();
    cfg.max_retries = 2;
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "make a and b".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let graph = p.graph.lock().expect("graph");
    // Both contracts' attempts wrote the path.
    let writers = kgraf_graph::cochange::task_writers(&graph, "src/shared.rs").expect("writers");
    assert!(writers.contains(&"r1/a".to_owned()), "{writers:?}");
    assert!(writers.contains(&"r1/b".to_owned()), "{writers:?}");
    // The conflict between `b` and the already merged `a` is one collision.
    assert_eq!(
        kgraf_graph::cochange::collision_count(&graph).expect("count"),
        1
    );
}
