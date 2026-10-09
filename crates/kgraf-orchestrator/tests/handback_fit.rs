//! A hand-back's child starts with room to work.
//!
//! Window 10,000: the stop point is 8,000 tokens and the plan target (what a
//! contract's brief is sized to fit under) 4,000. An `ALWAYSBIG` worker
//! reports 9,000 tokens on its first round, so every child of the chain hands
//! back again until the delegation depth cap, where the worker compacts and
//! finishes. What this file measures is the first request of each child:
//!
//! - it is under the plan target, with the pinned slices trimmed to what the
//!   fixed part of the brief leaves (the parent's whole slice set is not
//!   inherited);
//! - it does not grow with the depth of the chain (a remainder quotes the
//!   chain's original instruction once, not each ancestor's brief inside the
//!   next);
//! - a contract whose child could not fit at all is spawned to compact instead
//!   of handing back.
//!
//! Measured with `estimate_tokens` of the opening user message, which holds
//! the instruction and the rendered slices, plus the route's prefix estimate:
//! the figures the dispatch estimate uses.

mod support;

use kgraf_core::{RunId, estimate_tokens};
use kgraf_orchestrator::{Orchestrator, RunStatus, RunStore, TraceEvent, TraceLog};
use std::sync::Arc;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 10_000;
/// Half the stop point: what a brief is planned to fit under.
const TARGET: u64 = 4_000;
/// Eight functions: each slice shows its own function once (a brief's slices
/// show each line once), so the parent is over the target on its own text, not
/// on the same file repeated in every slice.
const SCAFFOLD_SYMBOLS: usize = 8;

/// Eight functions of about 470 tokens each: together about 3,800, which the plan-time
/// write-file slicing gives a worker whole.
fn scaffold() -> String {
    scaffold_of(SCAFFOLD_SYMBOLS)
}

/// `symbols` functions of about 470 tokens each.
fn scaffold_of(symbols: usize) -> String {
    (0..symbols)
        .map(|i| {
            let body = format!("    // {}\n", "scaffold line ".repeat(100));
            format!("pub fn f{i}() -> u32 {{\n{body}    {i}\n}}\n\n")
        })
        .collect()
}

fn plan(instruction: &str) -> String {
    serde_json::json!({"contracts":[
        {"id":"big","instruction":instruction,"acceptance":"src/big.rs exists",
         "writes":[{"path":"src/big.rs"}],"tools":["write_file"]}
    ]})
    .to_string()
}

struct Measured {
    /// The first message of every attempt, parent first.
    openings: Vec<String>,
    prefix_tokens: u64,
    trace: Vec<TraceEvent>,
    record: kgraf_orchestrator::RunStateRecord,
    status: RunStatus,
}

async fn run(instruction: &str) -> Measured {
    run_in(instruction, WINDOW, 1_000).await
}

/// `run` with the window and the most tokens one slice renders at.
async fn run_in(instruction: &str, window: u64, slice_budget: u64) -> Measured {
    run_with(instruction, window, slice_budget, false, SCAFFOLD_SYMBOLS).await
}

/// The scaffold with every function's signature changed: each symbol gets a
/// new node id once it is indexed again, so a hand-back child's recorded
/// slice ids name nothing.
fn rewritten_scaffold() -> String {
    scaffold().replace("() -> u32", "(x: u32) -> u32")
}

/// `run_in`; with `reindex_during_first_child`, the scaffold is rewritten and
/// indexed again while the first child is in flight (what a resume after an
/// edit, or a predecessor's merge, does to the graph), so the next child's
/// slices are resolved again at its dispatch.
async fn run_with(
    instruction: &str,
    window: u64,
    slice_budget: u64,
    reindex_during_first_child: bool,
    symbols: usize,
) -> Measured {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/big.rs"), scaffold_of(symbols)).expect("write");
    {
        let g =
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
        kgraf_graph::Indexer::new(&g)
            .index_paths(&["src/big.rs".to_owned()])
            .expect("index");
    }
    let plan = plan(instruction);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        // A child stays in flight long enough for the graph to change under it.
        Duration::from_millis(if reindex_during_first_child { 400 } else { 5 }),
    );
    let mut services = p.services(window);
    // Workers render their slices from the run's graph, as the host wires it.
    for r in &mut services.routes {
        let env = Arc::get_mut(&mut r.env).expect("a fresh route");
        env.slices = Arc::new(kgraf_runtime::GraphSlices::new(
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph"),
            slice_budget,
        ));
    }
    let prefix_tokens = services.routes[0].env.prefix.estimated_tokens();
    let orchestrator = Orchestrator::new(config(), services).expect("orchestrator");
    let root = dir.path().to_path_buf();
    let stats = p.stats.clone();
    let editor = async move {
        if !reindex_during_first_child {
            return;
        }
        // Openings: the parent's, then the first child's.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while stats.openings.lock().expect("lock").len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "no child was dispatched"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        std::fs::write(root.join("src/big.rs"), rewritten_scaffold()).expect("rewrite");
        let g = kgraf_graph::GraphStore::open(&root.join("graph.db"), &root).expect("graph");
        kgraf_graph::Indexer::new(&g)
            .index_paths(&["src/big.rs".to_owned()])
            .expect("index again");
    };
    let (report, ()) = tokio::join!(
        orchestrator.start(RunId::new("r1"), "a big part".into()),
        editor
    );
    let report = report.expect("run");
    let trace = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    let openings = p.stats.openings.lock().expect("lock").clone();
    Measured {
        openings,
        prefix_tokens,
        trace,
        record,
        status: report.status,
    }
}

fn start_tokens(m: &Measured, opening: &str) -> u64 {
    m.prefix_tokens + estimate_tokens(opening)
}

/// An instruction of about 1,200 bytes (a bound of 600 tokens).
fn long_instruction() -> String {
    format!(
        "ALWAYSBIG part. {} WRITE src/big.rs",
        "Keep the sections apart. ".repeat(46)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hand_backs_child_starts_under_the_plan_target_with_trimmed_slices() {
    let m = run(&long_instruction()).await;
    assert_eq!(m.status, RunStatus::Completed, "{:?}", m.status);
    let parent = &m.openings[0];
    let children: Vec<&String> = m
        .openings
        .iter()
        .filter(|o| {
            o.contains(kgraf_runtime::REMAINDER_HEAD)
                && !o.contains(kgraf_runtime::COMPACTION_NOTICE_HEADER)
        })
        .collect();
    assert!(
        children.len() >= 2,
        "a chain of hand-backs ran: {} openings",
        m.openings.len()
    );
    // The parent carries every slice: it is the case the child used to inherit.
    assert_eq!(
        parent.matches("// slice ").count(),
        SCAFFOLD_SYMBOLS,
        "the parent has all {SCAFFOLD_SYMBOLS} slices"
    );
    assert!(
        start_tokens(&m, parent) > TARGET,
        "the parent alone is over the target: {}",
        start_tokens(&m, parent)
    );
    for child in &children {
        let tokens = start_tokens(&m, child);
        assert!(
            tokens <= TARGET,
            "a child starts at {tokens} tokens, over the {TARGET}-token plan target"
        );
        let slices = child.matches("// slice ").count();
        assert!(
            (1..SCAFFOLD_SYMBOLS).contains(&slices),
            "the child keeps some slices and not all: {slices}"
        );
    }
    // The trim is said with the numbers.
    let said = m
        .trace
        .iter()
        .filter(|e| {
            matches!(e, TraceEvent::Notice { detail, .. }
            if detail.contains("the hand-back's child pins")
                && detail.contains(&format!("of {SCAFFOLD_SYMBOLS} slices")))
        })
        .count();
    // Said where it happens: the first child drops slices, the later ones inherit the trimmed set.
    assert_eq!(said, 1, "{said}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_of_hand_backs_does_not_grow_a_childs_brief() {
    let m = run(&long_instruction()).await;
    let children: Vec<&String> = m
        .openings
        .iter()
        .filter(|o| {
            o.contains(kgraf_runtime::REMAINDER_HEAD)
                && !o.contains(kgraf_runtime::COMPACTION_NOTICE_HEADER)
        })
        .collect();
    assert!(children.len() >= 3, "{} children", children.len());
    // One wrapper in every child, whatever its depth.
    for c in &children {
        assert_eq!(c.matches(kgraf_runtime::REMAINDER_HEAD).count(), 1, "{c}");
    }
    // The same text but for the token counts the remainder names, so the same
    // length (every count here has the same number of digits).
    let first = children[0].len();
    for c in &children[1..] {
        assert_eq!(c.len(), first, "a deeper child's brief is the same size");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contract_whose_child_could_not_fit_compacts_instead_of_handing_back() {
    // About 7,400 bytes, under the guards' 8,000-character task bound but with a
    // bound of 3,700 tokens: with the hand-back wrapper, the brief lines and the
    // prefix the child's fixed part is over the 3,200-token target of an 8,000-token
    // window (stop point 6,400). No child can fit, so the first stop point compacts.
    // Four functions: with eight, the parent itself would reach the stop point.
    let long = format!(
        "ALWAYSBIG part. {} WRITE src/big.rs",
        "Keep the sections apart. ".repeat(296)
    );
    let m = run_with(&long, 8_000, 500, false, 4).await;
    assert_eq!(m.status, RunStatus::Completed, "{:?}", m.status);
    let handbacks = m
        .trace
        .iter()
        .filter(
            |e| matches!(e, TraceEvent::WorkerFinished { outcome, .. } if outcome == "handed_back"),
        )
        .count();
    assert_eq!(
        handbacks, 0,
        "no hand-back: the worker compacted at its stop point"
    );
    assert_eq!(m.record.compactions.len(), 1, "{:?}", m.record.compactions);
    assert_eq!(m.record.compactions[0].contract, "big");
    assert!(
        !m.openings
            .iter()
            .any(|o| o.contains(kgraf_runtime::REMAINDER_HEAD)),
        "no child was made"
    );
}

/// The cap holds at dispatch, not only when the child is made. Once the graph
/// changes under a hand-back's chain, a later child's pinned slice ids name
/// nothing, and dispatch falls back to the symbols of the declared files:
/// all of them, the set the parent had. The contract's cap keeps the child
/// under the plan target. With `Orchestrator::cap_slices` disabled this test
/// fails on the slice count and the size.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_childs_slices_resolved_again_at_dispatch_still_fit_its_cap() {
    let m = run_with(&long_instruction(), WINDOW, 1_000, true, SCAFFOLD_SYMBOLS).await;
    assert_eq!(m.status, RunStatus::Completed, "{:?}", m.status);
    let children: Vec<&String> = m
        .openings
        .iter()
        .filter(|o| {
            o.contains(kgraf_runtime::REMAINDER_HEAD)
                && !o.contains(kgraf_runtime::COMPACTION_NOTICE_HEADER)
        })
        .collect();
    assert!(children.len() >= 2, "{} children", children.len());
    // The first child was dispatched before the graph changed; a later one
    // reads the new signatures, so its slices were resolved again.
    let again: Vec<&&String> = children
        .iter()
        .filter(|c| c.contains("(x: u32) -> u32"))
        .collect();
    assert!(
        !again.is_empty(),
        "a child's slices were resolved again from the changed graph"
    );
    for child in again {
        let tokens = start_tokens(&m, child);
        assert!(
            tokens <= TARGET,
            "a child starts at {tokens} tokens, over the {TARGET}-token plan target"
        );
        let slices = child.matches("// slice ").count();
        assert!(
            (1..SCAFFOLD_SYMBOLS).contains(&slices),
            "the capped set, not all {SCAFFOLD_SYMBOLS} symbols: {slices}"
        );
    }
}
