//! A slice whose file cannot be read never ends a worker's spawn. Seen live: a
//! contract's two attempts both failed at spawn with "cannot render slice for
//! node ...: cannot read `iface/src/lib.rs`", before any model call, and the
//! contract was held. The node is left out instead: the worker starts, its
//! opening names the node, its file and the reason, and the run's trace has a
//! notice saying the same.
//!
//! The staleness behind one such read is fixed too: the message bus indexes a
//! conversation file from a worktree it removes as soon as the file is
//! committed, so the file's recorded source root named a tree that was gone and
//! the checkout never had the file. After the commit the file is indexed from
//! the integration view and dead records are re-pointed there.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus, TraceEvent};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

async fn trace(dir: &std::path::Path) -> Vec<TraceEvent> {
    kgraf_orchestrator::TraceLog::open(&dir.join("runs/r1/trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slice_whose_file_is_missing_at_dispatch_is_left_out_and_the_worker_starts() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src/rate.rs"),
        "pub fn rate() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("src/other.rs"),
        "pub fn other() -> u32 {\n    77\n}\n",
    )
    .expect("write");
    {
        let g =
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
        kgraf_graph::Indexer::new(&g).index().expect("index");
    }
    let plan = json!({"contracts": [{
        "id": "c1",
        "instruction": "use rate WRITE src/use_rate.rs",
        "acceptance": "src/use_rate.rs exists",
        "writes": [{"path": "src/use_rate.rs"}],
        "depends_on": [],
        "slices": ["rate", "other"],
        "tools": ["write_file"],
        "attempts": 1,
    }]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let rate = p
        .graph
        .lock()
        .expect("lock")
        .resolve_symbol("rate")
        .expect("indexed")[0]
        .id
        .clone();
    // The graph still holds `rate`, but its file is gone when c1 is
    // dispatched: every read of it fails.
    std::fs::remove_file(dir.path().join("src/rate.rs")).expect("rm");

    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "use rate".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["c1".to_owned()]);
    assert_eq!(report.counters.slices_left_out, 1);

    let opening = p
        .stats
        .openings
        .lock()
        .expect("lock")
        .iter()
        .find(|o| o.contains("use rate"))
        .cloned()
        .expect("c1's worker made a model call");
    assert!(
        opening.contains("77"),
        "the readable slice is given: {opening}"
    );
    assert!(opening.contains("slices left out"), "{opening}");
    assert!(
        opening.contains(&format!("node `{rate}` in `src/rate.rs`")),
        "the opening names the node and its file: {opening}"
    );

    let events = trace(dir.path()).await;
    let notice = events
        .iter()
        .find_map(|e| match e {
            TraceEvent::Notice {
                contract, detail, ..
            } if contract == "c1" && detail.starts_with("slice left out") => Some(detail.clone()),
            _ => None,
        })
        .expect("a notice names the slice left out");
    assert!(
        notice.contains(&format!(
            "node `{rate}` in `src/rate.rs`: cannot read `src/rate.rs`: "
        )),
        "{notice}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            TraceEvent::WorkerFinished { contract, outcome, tool_calls, .. }
                if contract == "c1" && outcome == "completed" && *tool_calls > 0
        )),
        "the worker worked: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_committed_conversation_file_stays_readable_after_its_worktree_is_removed() {
    let dir = tempfile::tempdir().expect("dir");
    // Files in no indexed language: their merges index nothing, so the only
    // step that can keep the conversation file readable is the bus's own,
    // after its commit.
    let plan = plan_json(&[
        c("a", "make a", "notes/a.txt", &[]),
        c("b", "make b", "notes/b.txt", &["a"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    let view = dir.path().join("view");
    *integration.view.lock().expect("lock") = Some(view.clone());
    integration.comms_worktrees.store(true, Ordering::SeqCst);
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a, then b".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let committed = integration.comms.lock().expect("lock").clone();
    let rel = format!("{}/a--b.md", kgraf_graph::BUS_OWNED_DIRS[0]);
    assert!(
        committed.iter().any(|files| files.contains(&rel)),
        "a's report to b was committed: {committed:?}"
    );
    assert!(
        !dir.path().join(&rel).exists(),
        "the checkout never received the conversation file"
    );
    assert!(view.join(&rel).is_file(), "the view holds it");

    let g = kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
    let nodes = g.nodes_in_file(&rel).expect("nodes");
    let heading = nodes
        .iter()
        .find(|n| n.name.contains("done from a to b"))
        .expect("the message is a node");
    let text = g
        .source_of(heading)
        .expect("the message node is readable after its worktree is gone");
    assert!(text.contains("done from a to b"), "{text}");
    assert_eq!(
        g.file_source_root(&rel).expect("root"),
        Some(view),
        "the record names the view, not the removed conversation worktree"
    );
}
