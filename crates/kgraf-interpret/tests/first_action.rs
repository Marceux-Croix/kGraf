//! Time to first useful action on this repository (an acceptance
//! concern), with the model call faked so the measurement is of kGraf's own
//! part: indexing, ingest, ranking, the brief, parsing and reconciliation. The live
//! model call adds its own latency on top and is measured by the CLI's live run.
//!
//! The test asserts the work done: files indexed, one model call, the second pass
//! served from the cache without indexing again. The debug-build wall-clock
//! budgets below are enforced only when `KGRAF_TIME_BUDGET` is set, since elapsed
//! time depends on machine load. The release budget, measured with criterion, is in
//! `benches/first_action.rs`.

mod support;

use kgraf_interpret::{CommandProvenance, NoTestRuns, prepare};
use serde_json::json;
use std::path::Path;
use std::time::Instant;
use support::*;

/// Cold first action on this repository in a debug build, in milliseconds. Measured
/// at 10.2 s to 20.0 s on the development machine (Apple silicon) with
/// other builds holding the load average near 24; the budget is 3x the worst. It
/// catches a pathological regression (an accidental quadratic), not a few percent;
/// the release budgets in `benches/first_action.rs` are the tight ones.
const DEBUG_COLD_BUDGET_MS: u128 = 60_000;
/// Warm (index current, answer cached): what a restart or a re-read costs.
/// Measured at 2.4 s at a load average near 24, and at 10.1 s at a load average
/// of 40; the budget is 3x the worst, for the same reason as above.
const DEBUG_WARM_BUDGET_MS: u128 = 30_000;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

/// Directories the snapshot leaves out: build output, VCS and agent state.
const SKIP: &[&str] = &["target", ".git", ".claude", ".kgraf", "node_modules"];

/// Copy the working tree, so a file edited while the suite runs cannot change the
/// corpus between the cold and the warm pass.
fn snapshot(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        let kind = entry.file_type().expect("type");
        let target = to.join(&name);
        if kind.is_dir() {
            if SKIP.iter().any(|s| name == *s) {
                continue;
            }
            std::fs::create_dir_all(&target).expect("mkdir");
            snapshot(&entry.path(), &target);
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn time_to_first_useful_action_on_this_repository() {
    let db = tempfile::tempdir().expect("db");
    let tree = tempfile::tempdir().expect("tree");
    snapshot(repo_root(), tree.path());
    let store = store(tree.path(), db.path());
    let model = FakeModel::new(|_| {
        answer(
            vec![item(
                "interpret",
                &[],
                json!({"symbols": ["rank_repository"], "tests": ["matches_networkx_reference"]}),
                "partial",
                json!([{"text": "the workspace tests pass", "check": {"kind": "command", "command": "cargo test --workspace"}}]),
            )],
            json!([{"command": "cargo test --workspace", "source": "CLAUDE.md"}]),
            &[],
        )
    });

    let started = Instant::now();
    let prepared = prepare(&store, request()).expect("prepare");
    let i = prepared
        .ask(&model)
        .await
        .expect("ask")
        .finish(&store, &NoTestRuns)
        .expect("finish");
    let cold = started.elapsed().as_millis();
    eprintln!(
        "cold first action {cold} ms: {:?}; {} files indexed, {} documents, brief {} tokens, \
         {} of {} sections, {} of {} definitions",
        i.timings(),
        i.corpus().files_indexed,
        i.corpus().documents,
        i.brief().tokens,
        i.brief().sections_included,
        i.brief().sections_total,
        i.brief().definitions_included,
        i.brief().definitions_ranked
    );
    assert_eq!(i.first_action(), Some("interpret"));
    assert!(i.corpus().files_indexed > 0, "nothing was indexed");
    assert_eq!(
        i.corpus().files_reindexed,
        i.corpus().files_indexed,
        "a fresh graph indexes every file it sees"
    );
    assert_eq!(model.calls(), 1, "the cold pass makes one model call");
    if time_budget_enforced() {
        assert!(
            cold <= DEBUG_COLD_BUDGET_MS,
            "{cold} ms > {DEBUG_COLD_BUDGET_MS} ms"
        );
    }

    // Self-test of the reading's inputs: this repository's own planning documents
    // lead the brief, and its declared validation command is discovered, not
    // proposed.
    let brief = model.last_brief();
    for must in ["ROADMAP.md#", "CLAUDE.md"] {
        assert!(brief.contains(must), "the brief lacks {must}");
    }
    assert!(brief.contains("# Code map:") && i.brief().definitions_included > 0);
    assert!(i.brief().tokens <= i.brief().budget_tokens);
    assert!(matches!(
        &i.commands()[0].provenance,
        CommandProvenance::Discovered { path, .. } if path.ends_with(".md")
    ));
    // Symbol present, test present but not run: partial, and the documents' claim
    // agrees, so no discrepancy.
    assert_eq!(i.states()[0].doneness, kgraf_interpret::Doneness::Partial);

    // Warm: the index is current and the answer cached; no model call.
    let started = Instant::now();
    let again = prepare(&store, request())
        .expect("prepare")
        .ask(&model)
        .await
        .expect("ask")
        .finish(&store, &NoTestRuns)
        .expect("finish");
    let warm = started.elapsed().as_millis();
    eprintln!("warm first action {warm} ms: {:?}", again.timings());
    assert!(again.is_cached());
    assert_eq!(model.calls(), 1);
    assert_eq!(
        again.corpus().files_reindexed,
        0,
        "the warm pass indexed a file again"
    );
    if time_budget_enforced() {
        assert!(
            warm <= DEBUG_WARM_BUDGET_MS,
            "{warm} ms > {DEBUG_WARM_BUDGET_MS} ms"
        );
    }
}

/// The same measurement on any tree, read in place and never written: the graph
/// database lives in a temp directory. Run with `KGRAF_FIRST_ACTION_TREE=<dir>`
/// and `--ignored`; prints the phase timings for profiling a large tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs KGRAF_FIRST_ACTION_TREE"]
async fn first_action_on_a_tree_named_in_the_environment() {
    let Some(tree) = std::env::var_os("KGRAF_FIRST_ACTION_TREE") else {
        panic!("set KGRAF_FIRST_ACTION_TREE to the tree to read");
    };
    // The graph database goes to `KGRAF_FIRST_ACTION_DB` when set (kept between runs,
    // so a later run is warm), otherwise to a temp directory.
    let temp = tempfile::tempdir().expect("db");
    let db = std::env::var_os("KGRAF_FIRST_ACTION_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temp.path().to_path_buf());
    std::fs::create_dir_all(&db).expect("db dir");
    let store = store(Path::new(&tree), &db);
    let model = FakeModel::new(|_| answer(vec![], json!([]), &["what is this tree?"]));
    for pass in ["cold", "warm"] {
        let started = Instant::now();
        let prepared = prepare(&store, request()).expect("prepare");
        let done = prepared
            .ask(&model)
            .await
            .expect("ask")
            .finish(&store, &NoTestRuns);
        let ms = started.elapsed().as_millis();
        match done {
            Ok(i) => eprintln!(
                "{pass} first action {ms} ms: {:?}; {} files indexed, {} documents, brief {} tokens, \
                 {} of {} sections, {} of {} definitions",
                i.timings(),
                i.corpus().files_indexed,
                i.corpus().documents,
                i.brief().tokens,
                i.brief().sections_included,
                i.brief().sections_total,
                i.brief().definitions_included,
                i.brief().definitions_ranked
            ),
            Err(e) => eprintln!("{pass} first action {ms} ms (the fake answer was refused: {e})"),
        }
    }
}
