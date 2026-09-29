//! A merge makes a new symbol sliceable for the next contract: once a
//! contract's attempt is accepted onto the integration branch
//! (`Coord::integrate_winner`), the code it wrote is indexed from its own
//! worktree (`Coord::reindex_merged`), so a later contract's planner slices
//! and `graph_slice` tool see it immediately, not only after a separate
//! `kgraf index` pass.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_makes_its_new_symbol_sliceable() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    // Either route may draw the contract; both must hand back real Rust
    // source, not the fake's default (its own vendor name as bare text).
    let source = "pub fn merged_fn() -> u32 {\n    42\n}\n";
    p.alpha.set_content(source);
    p.beta.set_content(source);

    // Nothing is indexed yet: the fixture has no files on disk at all before
    // the run writes them.
    assert!(
        kgraf_graph::slice::slice_for_symbol(&p.graph.lock().expect("lock"), "merged_fn", 2_000)
            .is_err(),
        "merged_fn must not be sliceable before any contract has run"
    );

    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "build a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["a".to_owned()]);
    assert!(
        integration
            .merges
            .lock()
            .expect("lock")
            .iter()
            .any(|(c, _)| c == "a"),
        "the attempt merged"
    );

    let slice =
        kgraf_graph::slice::slice_for_symbol(&p.graph.lock().expect("lock"), "merged_fn", 2_000)
            .expect("merged_fn is sliceable once the merge that wrote it has run");
    assert!(
        slice.render().contains("merged_fn"),
        "the slice does not carry the merged function: {}",
        slice.render()
    );
}
