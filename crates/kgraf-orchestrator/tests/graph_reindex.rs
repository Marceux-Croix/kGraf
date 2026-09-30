//! A merge makes a new symbol sliceable for the next contract: once a
//! contract's attempt is accepted onto the integration branch
//! (`Coord::integrate_winner`), the code it wrote is indexed from the run's
//! integration view, or from its own worktree when the provider keeps no view
//! (`Coord::reindex_merged`), so a later contract's planner slices and
//! `graph_slice` tool see it immediately, not only after a separate
//! `kgraf index` pass. A tree that is gone is a traced failure, never a
//! deletion.

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

    // The merge is indexed from the integration worktree, so the symbol is in
    // the graph now.
    assert!(
        p.graph
            .lock()
            .expect("lock")
            .resolve_symbol("merged_fn")
            .is_ok(),
        "merged_fn is indexed once the merge that wrote it has run"
    );
    // The merge has not reached the checkout the store describes, yet the slice
    // carries the merged function's source: the file's recorded source root is
    // the worktree it was indexed from, and the hash is verified against it.
    assert!(
        !dir.path().join("src/a.rs").exists(),
        "the merged file is not at the store root"
    );
    let slice =
        kgraf_graph::slice::slice_for_symbol(&p.graph.lock().expect("lock"), "merged_fn", 2_000)
            .expect("merged_fn is sliceable before the merge reaches the checkout");
    let requested = slice
        .members
        .iter()
        .find(|m| m.reason == kgraf_graph::SliceReason::Requested)
        .expect("the requested node is in the slice");
    assert!(
        requested.source.contains("42"),
        "the slice does not carry the merged function: {}",
        slice.render()
    );
    // Content that differs from what was indexed is still refused: a checkout
    // file edited since is not read as the merged one when the worktree is gone.
    let landed = dir.path().join("src/a.rs");
    std::fs::create_dir_all(landed.parent().expect("parent")).expect("mkdir");
    std::fs::write(&landed, "pub fn merged_fn() -> u32 {\n    7\n}\n").expect("write");
    let worktree = p
        .graph
        .lock()
        .expect("lock")
        .file_source_root("src/a.rs")
        .expect("root lookup")
        .expect("the merged file records the worktree it was indexed from");
    std::fs::remove_file(worktree.join("src/a.rs")).expect("remove worktree copy");
    let stale =
        kgraf_graph::slice::slice_for_symbol(&p.graph.lock().expect("lock"), "merged_fn", 2_000);
    assert!(
        stale.is_err(),
        "an edited checkout file must not pass for the indexed content: {stale:?}"
    );
    // Once the merge has landed in the checkout, the slice reads from there.
    std::fs::write(&landed, source).expect("write");
    let slice =
        kgraf_graph::slice::slice_for_symbol(&p.graph.lock().expect("lock"), "merged_fn", 2_000)
            .expect("merged_fn is sliceable once the merge has landed at the store root");
    assert!(
        slice.render().contains("42"),
        "the slice does not carry the merged function: {}",
        slice.render()
    );
}

/// Run one contract writing `src/a.rs` against `integration`; the merge
/// removes the winner's worktree, as merge-back does under the default
/// worktree policy (decision 0088).
async fn run_with_removal(
    dir: &std::path::Path,
    integration: std::sync::Arc<FakeIntegration>,
    source: &str,
) -> (Parts, Vec<kgraf_orchestrator::TraceEvent>) {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir,
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.set_content(source);
    p.beta.set_content(source);
    integration
        .remove_on_merge
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "build a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["a".to_owned()]);
    let events = kgraf_orchestrator::TraceLog::open(&dir.join("runs/r1/trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    (p, events)
}

/// The merge removed the winner's worktree: the merged file is indexed from
/// the integration view, stays in the graph, and its slice reads from the
/// view, which outlives the merge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_that_removes_its_worktree_keeps_the_merged_file_in_the_graph() {
    let dir = tempfile::tempdir().expect("dir");
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    let view = dir.path().join("view");
    *integration.view.lock().expect("lock") = Some(view.clone());
    let source = "pub fn a(x: u8) -> u8 {\n    x + 42\n}\n";
    let (p, _) = run_with_removal(dir.path(), integration.clone(), source).await;
    let g = p.graph.lock().expect("lock");
    assert!(
        g.file("src/a.rs").expect("file").is_some(),
        "the merged file is in the graph"
    );
    assert!(g.resolve_symbol("a").is_ok(), "its function resolves");
    assert_eq!(
        g.file_source_root("src/a.rs").expect("root").as_deref(),
        Some(view.as_path()),
        "the file is read from the view"
    );
    let slice = kgraf_graph::slice::slice_for_symbol(&g, "a", 2_000).expect("sliceable");
    assert!(slice.render().contains("x + 42"), "{}", slice.render());
}

/// With no view and the worktree removed by the merge, the tree to read is
/// gone: the reindex fails and says so, and the file the graph already held
/// is kept, never deleted as if the merge had removed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_whose_tree_is_gone_deletes_nothing_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/a.rs"), "pub fn before() {}\n").expect("write");
    {
        let g = GraphStoreProbe::open(dir.path());
        kgraf_graph::Indexer::new(&g)
            .index_paths(&["src/a.rs".to_owned()])
            .expect("index");
    }
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    let (p, events) = run_with_removal(
        dir.path(),
        integration,
        "pub fn a(x: u8) -> u8 {\n    x\n}\n",
    )
    .await;
    let g = p.graph.lock().expect("lock");
    assert!(
        g.file("src/a.rs").expect("file").is_some(),
        "the file is kept"
    );
    assert!(g.resolve_symbol("before").is_ok(), "its nodes are kept");
    assert!(
        events.iter().any(|e| matches!(e,
            kgraf_orchestrator::TraceEvent::Notice { detail, .. }
                if detail.contains("reindex after merge failed"))),
        "the failed reindex is traced: {events:?}"
    );
}

/// The graph file the run opens, for indexing before it starts.
struct GraphStoreProbe;

impl GraphStoreProbe {
    fn open(dir: &std::path::Path) -> kgraf_graph::GraphStore {
        kgraf_graph::GraphStore::open(&dir.join("graph.db"), dir).expect("graph")
    }
}
