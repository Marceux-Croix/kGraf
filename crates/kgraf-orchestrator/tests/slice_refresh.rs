//! Slices are resolved at dispatch, not only at planning. A node id hashes
//! its symbol's signature, so a predecessor that renames a parameter (the
//! scaffold's `_s` becoming `s`) gives the symbol a new id once its merge is
//! indexed. The dependent contract still gets the symbol's slice, read by name
//! against the graph as it is at dispatch; a name that no longer resolves is a
//! traced empty lookup, never a failed spawn.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus, TraceEvent};
use serde_json::json;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;
const SCAFFOLD: &str = "pub fn rate(_s: &str) -> u32 {\n    0\n}\n";

/// c1 rewrites `src/pricing.rs` (a stem other than the slice name, which would resolve as a file); c2 depends on c1 and asks for the slice `rate`.
fn plan() -> String {
    json!({"contracts": [
        {
            "id": "c1",
            "instruction": "implement rate WRITE src/pricing.rs",
            "acceptance": "src/pricing.rs exists",
            "writes": [{"path": "src/pricing.rs"}],
            "depends_on": [],
            "tools": ["write_file"],
            "attempts": 1,
        },
        {
            "id": "c2",
            "instruction": "use rate WRITE src/use_rate.rs",
            "acceptance": "src/use_rate.rs exists",
            "writes": [{"path": "src/use_rate.rs"}],
            "depends_on": ["c1"],
            "slices": ["rate"],
            "tools": ["write_file"],
            "attempts": 1,
        },
    ]})
    .to_string()
}

/// Index the scaffold at the store root, run the plan with every worker
/// writing `written`, and return the parts, the trace and c2's opening.
async fn run(written: &str) -> (Parts, Vec<TraceEvent>, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/pricing.rs"), SCAFFOLD).expect("write");
    {
        let g =
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
        kgraf_graph::Indexer::new(&g)
            .index_paths(&["src/pricing.rs".to_owned()])
            .expect("index");
    }
    let plan = plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let planned = p
        .graph
        .lock()
        .expect("lock")
        .resolve_symbol("rate")
        .expect("the scaffold's rate is indexed")[0]
        .id
        .clone();
    p.alpha.set_content(written);
    p.beta.set_content(written);
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    let mut services = p.services(WINDOW);
    services.workspaces = integration;
    // Workers render their slices from the run's graph, as the host wires it.
    for r in &mut services.routes {
        let env = std::sync::Arc::get_mut(&mut r.env).expect("a fresh route");
        env.slices = std::sync::Arc::new(kgraf_runtime::GraphSlices::new(
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph"),
            2_000,
        ));
    }
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "build rate".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["c1".to_owned(), "c2".to_owned()]);
    assert!(
        p.graph
            .lock()
            .expect("lock")
            .node(&planned)
            .expect("lookup")
            .is_none(),
        "the planned node id is gone once c1's merge is indexed"
    );
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
        .find(|o| o.contains("use rate"))
        .cloned()
        .expect("c2 was dispatched");
    (p, events, opening, dir)
}

fn empty_lookups<'a>(events: &'a [TraceEvent], contract: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::EmptyGraphLookup {
                contract: c,
                lookup,
                ..
            } if c == contract => Some(lookup.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dependent_gets_the_slice_of_a_symbol_its_predecessor_renamed_a_parameter_of() {
    let (_p, events, opening, _dir) =
        run("pub fn rate(s: &str) -> u32 {\n    s.len() as u32\n}\n").await;
    assert!(
        opening.contains("s.len() as u32"),
        "c2's brief carries the merged `rate`: {opening}"
    );
    assert!(empty_lookups(&events, "c2").is_empty(), "{events:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slice_whose_symbol_is_gone_is_an_empty_lookup_not_a_failed_spawn() {
    let (_p, events, opening, _dir) = run("pub fn other() {}\n").await;
    assert!(
        !opening.contains("// slice"),
        "no slice is given: {opening}"
    );
    assert_eq!(empty_lookups(&events, "c2"), vec!["rate"], "{events:?}");
}
