//! Instrument check for partition acceptance: the measuring
//! code in `partition.rs` run end to end on a small constructed fixture.
//!
//! **This is not evidence about the partition.** The fixture and the predictor
//! were written by the same author, the task set is ten tasks, and nothing here
//! is recorded as evidence. The assertions check the
//! instrument (every predictor is scored against the same truth; a deeper walk
//! predicts a superset; the counts are consistent), never a threshold. The
//! printed numbers describe this fixture only.
//!
//! Fixture: `tests/fixtures/partition/`, a ten-file Rust project and ten change
//! requests whose write sets were fixed before any predictor ran (see its
//! `tasks.json` note for what that does and does not show).

use kgraf_graph::{GraphStore, Indexer};
use kgraf_orchestrator::{
    GraphPredictor, MissCause, PartitionReport, TaskTruth, measure, random_predictions,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/partition")
}

fn tasks() -> Vec<TaskTruth> {
    let text = std::fs::read_to_string(fixture().join("tasks.json")).expect("tasks");
    let v: serde_json::Value = serde_json::from_str(&text).expect("json");
    serde_json::from_value(v["tasks"].clone()).expect("tasks")
}

fn run(
    name: &str,
    graph: &GraphStore,
    truth: &[TaskTruth],
    hops: u32,
    min_weight: i64,
) -> (PartitionReport, Vec<BTreeSet<String>>) {
    let p = GraphPredictor {
        graph,
        hops,
        min_weight,
    };
    let predicted: Vec<BTreeSet<String>> = truth
        .iter()
        .map(|t| p.predict(&t.text).expect("predict"))
        .collect();
    let mut cause = |t: usize, f: &str| {
        p.classify_miss(&truth[t].text, f)
            .unwrap_or(MissCause::Unreachable)
    };
    (measure(name, truth, &predicted, &mut cause), predicted)
}

#[test]
fn instrument_check_scores_the_graph_partition_against_known_write_sets() {
    let root = fixture().join("repo");
    let graph = GraphStore::open_in_memory(&root).expect("graph");
    let indexed = Indexer::new(&graph).index().expect("index");
    assert!(indexed.files_seen >= 19, "{indexed:?}");
    let truth = tasks();
    assert_eq!(truth.len(), 10);
    let files = graph.file_paths().expect("files");
    // Every true write is a file in the project: the truth is checkable.
    for t in &truth {
        for w in &t.writes {
            assert!(files.contains(w), "{} writes unknown {w}", t.id);
        }
    }
    let total_true: usize = truth.iter().map(|t| t.writes.len()).sum();

    let configs = [
        ("seed files only", 0, 100),
        ("1 hop, confident calls", 1, 100),
        ("1 hop, all calls", 1, 0),
        ("2 hops, all calls", 2, 0),
    ];
    let mut reports = Vec::new();
    let mut previous: Option<Vec<BTreeSet<String>>> = None;
    for (name, hops, min_weight) in configs {
        let (r, predicted) = run(name, &graph, &truth, hops, min_weight);
        let w = &r.write_sets;
        assert_eq!(w.true_positive + w.false_negative, total_true, "{name}");
        assert_eq!(r.pairs.pairs, 45);
        // Each configuration walks at least what the one before it walked, so it
        // predicts a superset: recall cannot fall and yield cannot rise.
        if let Some(prev) = &previous {
            for (a, b) in prev.iter().zip(&predicted) {
                assert!(a.is_subset(b), "{name}: {a:?} not within {b:?}");
            }
        }
        previous = Some(predicted.clone());
        reports.push((r, predicted));
    }
    for pair in reports.windows(2) {
        assert!(pair[1].0.write_sets.recall >= pair[0].0.write_sets.recall);
        assert!(pair[1].0.pairs.yield_ <= pair[0].0.pairs.yield_);
    }

    // The probe's control: random files, size-matched to the 1-hop prediction.
    let sizes: Vec<usize> = reports[2].1.iter().map(BTreeSet::len).collect();
    let random = random_predictions(&files, &sizes, 42);
    let control = measure(
        "random, size-matched to 1 hop",
        &truth,
        &random,
        &mut |_, _| MissCause::Unreachable,
    );

    println!(
        "instrument check (not evidence), fixture tests/fixtures/partition, 10 tasks, 45 pairs"
    );
    for r in reports
        .iter()
        .map(|(r, _)| r)
        .chain(std::iter::once(&control))
    {
        println!(
            "{:32} precision {:.2} recall {:.2} (macro {:.2}/{:.2}, empty {}) | \
             yield {:.2} base {:.2} false-disjoint {:.2} contamination {:.2} lift {:.2} | \
             low-conf share {}/{} missed {:?} | serialized {}/{} ({:.2}) rounds {} \
             collisions admitted {}",
            r.predictor,
            r.write_sets.precision,
            r.write_sets.recall,
            r.write_sets.macro_precision,
            r.write_sets.macro_recall,
            r.write_sets.empty_predictions,
            r.pairs.yield_,
            r.pairs.base_rate,
            r.pairs.false_disjoint_rate,
            r.pairs.contamination,
            r.pairs.lift,
            r.causes.low_confidence,
            r.causes.false_disjoint,
            r.causes.missed_files,
            r.serialization.serialized,
            r.serialization.tasks,
            r.serialization.fraction,
            r.serialization.rounds,
            r.serialization.collisions_admitted,
        );
    }
}
