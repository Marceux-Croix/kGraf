//! A contract whose named slices find nothing in the graph still gets graph
//! context: the symbols defined in the files it declares it writes. A planner
//! that copies a placeholder name (seen live: a local model wrote the prompt
//! example's slice name verbatim) or names none must not leave the worker with
//! no slice at all when the graph knows the very file it edits. The empty
//! lookup stays traced, the substitution is traced, and the run report counts
//! both, so slice use is visible.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus, TraceEvent};
use serde_json::json;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;
const BUDGET: &str = "pub fn estimate_tokens(text: &str) -> u64 {\n    \
                      if text.is_empty() { 0 } else { text.len().div_ceil(3) as u64 + 1 }\n}\n";

fn plan(slices: serde_json::Value) -> String {
    json!({"contracts": [{
        "id": "c1",
        "instruction": "add a test WRITE src/budget.rs",
        "acceptance": "src/budget.rs exists",
        "writes": [{"path": "src/budget.rs"}],
        "slices": slices,
        "tools": ["write_file"],
        "attempts": 1,
    }]})
    .to_string()
}

async fn run(
    slices: serde_json::Value,
) -> (
    kgraf_orchestrator::RunReport,
    Vec<TraceEvent>,
    String,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/budget.rs"), BUDGET).expect("write");
    // A second file with the same stem, so a bare `budget` is ambiguous.
    std::fs::create_dir_all(dir.path().join("other")).expect("mkdir");
    std::fs::write(
        dir.path().join("other/budget.rs"),
        "pub fn other_thing() {}\n",
    )
    .expect("write");
    {
        let g =
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
        kgraf_graph::Indexer::new(&g)
            .index_paths(&["src/budget.rs".to_owned(), "other/budget.rs".to_owned()])
            .expect("index");
    }
    let plan = plan(slices);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.set_content(BUDGET);
    p.beta.set_content(BUDGET);
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    let mut services = p.services(WINDOW);
    services.workspaces = integration;
    for r in &mut services.routes {
        let env = std::sync::Arc::get_mut(&mut r.env).expect("a fresh route");
        env.slices = std::sync::Arc::new(kgraf_runtime::GraphSlices::new(
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph"),
            2_000,
        ));
    }
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "add a test".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let events = kgraf_orchestrator::TraceLog::open(&dir.path().join("runs/r1/trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let opening = p
        .stats
        .openings
        .lock()
        .expect("lock")
        .iter()
        .find(|o| o.contains("add a test"))
        .cloned()
        .expect("c1 was dispatched");
    (report, events, opening, dir)
}

fn notices(events: &[TraceEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Notice { detail, .. } => Some(detail.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_slice_name_falls_back_to_the_declared_files_symbols() {
    let (report, events, opening, _dir) = run(json!(["SymbolName"])).await;
    assert!(
        opening.contains("// slice") && opening.contains("fn estimate_tokens"),
        "the worker gets the symbol of the file it writes: {opening}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            TraceEvent::EmptyGraphLookup { lookup, .. } if lookup == "SymbolName"
        )),
        "the empty lookup is still traced: {events:?}"
    );
    assert!(
        notices(&events)
            .iter()
            .any(|n| n.contains("declared files") && n.contains("src/budget.rs")),
        "the substitution is traced: {events:?}"
    );
    let c = report.counters;
    assert_eq!(c.sliced_dispatches, 1, "{c:?}");
    assert_eq!(c.slices_from_writes, 1, "{c:?}");
    assert!(c.empty_slice_lookups >= 1, "{c:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_contract_naming_no_slice_gets_its_declared_files_symbols() {
    let (report, _events, opening, _dir) = run(json!([])).await;
    assert!(opening.contains("fn estimate_tokens"), "{opening}");
    assert_eq!(report.counters.slices_from_writes, 1);
    assert_eq!(report.counters.empty_slice_lookups, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_slice_that_resolves_is_used_and_no_fallback_is_taken() {
    let (report, events, opening, _dir) = run(json!(["estimate_tokens"])).await;
    assert!(opening.contains("fn estimate_tokens"), "{opening}");
    assert_eq!(report.counters.sliced_dispatches, 1);
    assert_eq!(report.counters.slices_from_writes, 0);
    assert!(
        !notices(&events)
            .iter()
            .any(|n| n.contains("declared files")),
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_path_and_symbol_name_resolves_without_the_fallback() {
    let (report, events, opening, _dir) = run(json!(["src/budget.rs::estimate_tokens"])).await;
    assert!(opening.contains("fn estimate_tokens"), "{opening}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TraceEvent::EmptyGraphLookup { .. })),
        "{events:?}"
    );
    let c = report.counters;
    assert_eq!(c.empty_slice_lookups, 0, "{c:?}");
    assert_eq!(c.slices_from_writes, 0, "{c:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stem_shared_by_two_files_is_a_miss_that_names_both() {
    let (report, events, _opening, _dir) = run(json!(["budget"])).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            TraceEvent::EmptyGraphLookup { lookup, .. } if lookup == "budget"
        )),
        "{events:?}"
    );
    let names = notices(&events);
    assert!(
        names.iter().any(|n| n.contains("`budget` matches 2 nodes")
            && n.contains("src/budget.rs")
            && n.contains("other/budget.rs")),
        "the candidates are traced: {names:?}"
    );
    assert_eq!(report.counters.slices_from_writes, 1);
}
