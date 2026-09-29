//! Partition acceptance, measured separately.
//!
//! Against a task set whose true write sets are known, a predicted partition is
//! scored on four things the roadmap names, each reported, none thresholded here:
//!
//! - **Write-set precision and recall**: per predicted file, is it written
//!   (precision); per written file, was it predicted (recall). Micro-averaged over
//!   files and macro-averaged over tasks.
//! - **Pair safety** (the axes of the earlier probe, so the numbers compare): of all task
//!   pairs, how many the prediction calls disjoint (yield), how many truly collide
//!   (base rate), the share of colliding pairs called disjoint (false disjoint
//!   rate), the share of disjoint calls that collide (contamination), and their
//!   ratio to the base rate (lift; 1.0 means the verdict carries no information).
//! - **Collisions traceable to low-confidence call edges**: for each colliding
//!   pair called disjoint, why the shared file was missed: no call path within four
//!   hops, a path only through ambiguous (weight under 100) call edges, or a
//!   confident path the predictor did not walk that far.
//! - **Serialization**: admitting tasks in plan order the way the lease registry
//!   does (a task waits while a running task's predicted set overlaps its own), the
//!   share that waited at least once (the `RunCounters::serialized` definition),
//!   and the truly colliding pairs admitted side by side anyway.
//!
//! [`GraphPredictor`] is the graph-derived partition: seeds are identifiers in a
//! task's text that resolve to graph nodes; the prediction is the seeds' files
//! plus files within `hops` call edges of weight at least `min_weight`. It never
//! sees the true write sets. The agents' own guesses (the decomposer's declared
//! writes) need a live model and are not measured here.
//!
//! Written new: the scoring follows the definitions of an earlier probe, whose
//! Python harness is not in this repository.

use kgraf_graph::{EdgeKind, GraphError, GraphStore};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// One task with its known write set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTruth {
    pub id: String,
    pub text: String,
    pub writes: BTreeSet<String>,
}

/// Write-set scores.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WriteSetScore {
    pub true_positive: usize,
    pub false_positive: usize,
    pub false_negative: usize,
    pub precision: f64,
    pub recall: f64,
    pub macro_precision: f64,
    pub macro_recall: f64,
    /// Tasks whose prediction was empty (nothing resolved).
    pub empty_predictions: usize,
}

/// Pair scores.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PairScore {
    pub pairs: usize,
    pub colliding: usize,
    pub predicted_disjoint: usize,
    /// Colliding pairs the prediction called disjoint.
    pub false_disjoint: usize,
    pub base_rate: f64,
    pub yield_: f64,
    pub false_disjoint_rate: f64,
    pub contamination: f64,
    pub lift: f64,
}

/// Why a truly written file was missed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissCause {
    /// No call path from any seed within four hops (or no seed at all).
    Unreachable,
    /// Reachable within four hops only through an ambiguous call edge.
    LowConfidence,
    /// Reachable through confident edges, beyond the predictor's depth.
    BeyondDepth,
}

/// The collision-cause share.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CauseScore {
    /// Colliding pairs called disjoint.
    pub false_disjoint: usize,
    /// Of those, pairs where a shared file was missed through a low-confidence
    /// edge.
    pub low_confidence: usize,
    pub share: f64,
    /// Every missed file, by cause.
    pub missed_files: BTreeMap<String, usize>,
}

/// Serialization scores.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SerializationScore {
    pub tasks: usize,
    /// Tasks that waited at least once on an overlapping predicted set.
    pub serialized: usize,
    pub fraction: f64,
    /// Rounds needed to admit everything.
    pub rounds: usize,
    /// Truly colliding pairs admitted in the same round.
    pub collisions_admitted: usize,
}

/// Everything measured for one predictor.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PartitionReport {
    pub predictor: String,
    pub write_sets: WriteSetScore,
    pub pairs: PairScore,
    pub causes: CauseScore,
    pub serialization: SerializationScore,
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

/// Precision and recall of predicted write sets against the truth.
pub fn score_write_sets(truth: &[TaskTruth], predicted: &[BTreeSet<String>]) -> WriteSetScore {
    let mut s = WriteSetScore::default();
    let (mut mp, mut mr) = (0.0, 0.0);
    for (t, p) in truth.iter().zip(predicted) {
        let tp = p.intersection(&t.writes).count();
        s.true_positive += tp;
        s.false_positive += p.len() - tp;
        s.false_negative += t.writes.len() - tp;
        if p.is_empty() {
            s.empty_predictions += 1;
        }
        mp += ratio(tp, p.len());
        mr += ratio(tp, t.writes.len());
    }
    s.precision = ratio(s.true_positive, s.true_positive + s.false_positive);
    s.recall = ratio(s.true_positive, s.true_positive + s.false_negative);
    s.macro_precision = if truth.is_empty() {
        0.0
    } else {
        mp / truth.len() as f64
    };
    s.macro_recall = if truth.is_empty() {
        0.0
    } else {
        mr / truth.len() as f64
    };
    s
}

fn overlaps(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    a.intersection(b).next().is_some()
}

/// The pair axes of the earlier probe.
pub fn score_pairs(truth: &[TaskTruth], predicted: &[BTreeSet<String>]) -> PairScore {
    let mut s = PairScore::default();
    for i in 0..truth.len() {
        for j in i + 1..truth.len() {
            s.pairs += 1;
            let collide = overlaps(&truth[i].writes, &truth[j].writes);
            let disjoint = !overlaps(&predicted[i], &predicted[j]);
            s.colliding += usize::from(collide);
            s.predicted_disjoint += usize::from(disjoint);
            s.false_disjoint += usize::from(collide && disjoint);
        }
    }
    s.base_rate = ratio(s.colliding, s.pairs);
    s.yield_ = ratio(s.predicted_disjoint, s.pairs);
    s.false_disjoint_rate = ratio(s.false_disjoint, s.colliding);
    s.contamination = ratio(s.false_disjoint, s.predicted_disjoint);
    s.lift = if s.base_rate == 0.0 {
        0.0
    } else {
        s.contamination / s.base_rate
    };
    s
}

/// Admit tasks in order, round by round, as the lease registry would: a task
/// waits while an admitted task's predicted set overlaps its own.
pub fn score_serialization(
    truth: &[TaskTruth],
    predicted: &[BTreeSet<String>],
) -> SerializationScore {
    let mut s = SerializationScore {
        tasks: truth.len(),
        ..SerializationScore::default()
    };
    let mut pending: Vec<usize> = (0..truth.len()).collect();
    let mut waited: BTreeSet<usize> = BTreeSet::new();
    while !pending.is_empty() {
        s.rounds += 1;
        let mut admitted: Vec<usize> = Vec::new();
        let mut rest = Vec::new();
        for i in pending {
            if admitted
                .iter()
                .any(|a| overlaps(&predicted[*a], &predicted[i]))
            {
                waited.insert(i);
                rest.push(i);
            } else {
                admitted.push(i);
            }
        }
        for (k, a) in admitted.iter().enumerate() {
            for b in &admitted[k + 1..] {
                s.collisions_admitted +=
                    usize::from(overlaps(&truth[*a].writes, &truth[*b].writes));
            }
        }
        pending = rest;
    }
    s.serialized = waited.len();
    s.fraction = ratio(s.serialized, s.tasks);
    s
}

/// Causes of the false-disjoint collisions. `cause(task, file)` classifies a file
/// task `task` wrote and did not predict.
pub fn score_causes(
    truth: &[TaskTruth],
    predicted: &[BTreeSet<String>],
    cause: &mut dyn FnMut(usize, &str) -> MissCause,
) -> CauseScore {
    let mut s = CauseScore::default();
    let mut memo: BTreeMap<(usize, String), MissCause> = BTreeMap::new();
    let mut classify = |t: usize, f: &str| -> MissCause {
        *memo.entry((t, f.to_owned())).or_insert_with(|| cause(t, f))
    };
    for (t, (tr, p)) in truth.iter().zip(predicted).enumerate() {
        for f in tr.writes.difference(p) {
            let c = classify(t, f);
            *s.missed_files.entry(format!("{c:?}")).or_default() += 1;
        }
    }
    for i in 0..truth.len() {
        for j in i + 1..truth.len() {
            let shared: Vec<&String> = truth[i].writes.intersection(&truth[j].writes).collect();
            if shared.is_empty() || overlaps(&predicted[i], &predicted[j]) {
                continue;
            }
            s.false_disjoint += 1;
            let low = shared.iter().any(|f| {
                (!predicted[i].contains(*f) && classify(i, f) == MissCause::LowConfidence)
                    || (!predicted[j].contains(*f) && classify(j, f) == MissCause::LowConfidence)
            });
            s.low_confidence += usize::from(low);
        }
    }
    s.share = ratio(s.low_confidence, s.false_disjoint);
    s
}

/// All four scores for one predictor's output.
pub fn measure(
    predictor: &str,
    truth: &[TaskTruth],
    predicted: &[BTreeSet<String>],
    cause: &mut dyn FnMut(usize, &str) -> MissCause,
) -> PartitionReport {
    PartitionReport {
        predictor: predictor.to_owned(),
        write_sets: score_write_sets(truth, predicted),
        pairs: score_pairs(truth, predicted),
        causes: score_causes(truth, predicted, cause),
        serialization: score_serialization(truth, predicted),
    }
}

/// The graph-derived partition.
pub struct GraphPredictor<'g> {
    pub graph: &'g GraphStore,
    /// Call-edge hops walked from each seed, in both directions.
    pub hops: u32,
    /// Edges below this weight are not walked (100 is an unambiguous resolution).
    pub min_weight: i64,
}

/// The weight of an unambiguous call resolution (`kgraf-graph` `index.rs`).
pub const CONFIDENT: i64 = 100;

impl GraphPredictor<'_> {
    /// Node ids of identifiers in `text` that resolve in the graph.
    pub fn seeds(&self, text: &str) -> Result<Vec<String>, GraphError> {
        let mut out = BTreeSet::new();
        for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if word.len() < 3 || word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            match self.graph.resolve_symbol(word) {
                Ok(nodes) => out.extend(nodes.into_iter().map(|n| n.id)),
                Err(GraphError::UnknownSymbol { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Files within `hops` call edges of weight at least `min_weight` of `seeds`,
    /// with the smallest edge weight on the path that reached each.
    fn reach(
        &self,
        seeds: &[String],
        hops: u32,
        min_weight: i64,
    ) -> Result<BTreeMap<String, i64>, GraphError> {
        let mut files: BTreeMap<String, i64> = BTreeMap::new();
        let mut best: BTreeMap<String, i64> = BTreeMap::new();
        let mut queue: VecDeque<(String, u32, i64)> = VecDeque::new();
        for s in seeds {
            if let Some(n) = self.graph.node(s)? {
                files.insert(n.path, CONFIDENT);
            }
            best.insert(s.clone(), CONFIDENT);
            queue.push_back((s.clone(), 0, CONFIDENT));
        }
        while let Some((id, depth, weakest)) = queue.pop_front() {
            if depth == hops {
                continue;
            }
            for (n, kind, weight, _) in self.graph.neighbors(&id)? {
                if kind != EdgeKind::Calls || weight < min_weight {
                    continue;
                }
                let w = weakest.min(weight);
                if best.get(&n.id).is_some_and(|b| *b >= w) {
                    continue;
                }
                best.insert(n.id.clone(), w);
                let f = files.entry(n.path.clone()).or_insert(w);
                *f = (*f).max(w);
                queue.push_back((n.id, depth + 1, w));
            }
        }
        Ok(files)
    }

    /// The predicted write set of a task.
    pub fn predict(&self, text: &str) -> Result<BTreeSet<String>, GraphError> {
        let seeds = self.seeds(text)?;
        Ok(self
            .reach(&seeds, self.hops, self.min_weight)?
            .into_keys()
            .collect())
    }

    /// Why `file` is not in the prediction for `text`.
    pub fn classify_miss(&self, text: &str, file: &str) -> Result<MissCause, GraphError> {
        let seeds = self.seeds(text)?;
        if self.reach(&seeds, 4, CONFIDENT)?.contains_key(file) {
            return Ok(MissCause::BeyondDepth);
        }
        if self.reach(&seeds, 4, 0)?.contains_key(file) {
            return Ok(MissCause::LowConfidence);
        }
        Ok(MissCause::Unreachable)
    }
}

/// A size-matched random prediction (the probe's control): for each task, as
/// many files as `sizes` says, drawn from `files` with a fixed-seed generator so
/// the number is reproducible.
pub fn random_predictions(files: &[String], sizes: &[usize], seed: u64) -> Vec<BTreeSet<String>> {
    let mut state = seed.max(1);
    let mut next = || {
        // xorshift64: reproducible, no dependency, enough for a control draw.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    sizes
        .iter()
        .map(|&k| {
            let mut out = BTreeSet::new();
            let k = k.min(files.len());
            while out.len() < k {
                out.insert(files[(next() % files.len() as u64) as usize].clone());
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(s: &[&str]) -> BTreeSet<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    fn task(id: &str, w: &[&str]) -> TaskTruth {
        TaskTruth {
            id: id.into(),
            text: String::new(),
            writes: set(w),
        }
    }

    #[test]
    fn scores_match_a_hand_count() {
        let truth = vec![task("a", &["x", "y"]), task("b", &["y"]), task("c", &["z"])];
        let pred = vec![set(&["x", "q"]), set(&["y"]), set(&["z", "x"])];
        let w = score_write_sets(&truth, &pred);
        // tp: x, y, z = 3; fp: q, x(c) = 2; fn: y(a) = 1.
        assert_eq!(
            (w.true_positive, w.false_positive, w.false_negative),
            (3, 2, 1)
        );
        assert!((w.precision - 0.6).abs() < 1e-9);
        assert!((w.recall - 0.75).abs() < 1e-9);
        let p = score_pairs(&truth, &pred);
        // Pairs: ab collide (y), predicted disjoint -> false disjoint.
        //        ac disjoint, predicted overlapping (x). bc disjoint, disjoint.
        assert_eq!(
            (p.pairs, p.colliding, p.predicted_disjoint, p.false_disjoint),
            (3, 1, 2, 1)
        );
        assert!((p.contamination - 0.5).abs() < 1e-9);
        assert!((p.lift - 1.5).abs() < 1e-9);
        let s = score_serialization(&truth, &pred);
        // Round 1 admits a and b (disjoint predictions: a truly colliding pair)
        // and holds c (x); round 2 admits c.
        assert_eq!((s.serialized, s.rounds, s.collisions_admitted), (1, 2, 1));
        let c = score_causes(&truth, &pred, &mut |_, _| MissCause::LowConfidence);
        assert_eq!((c.false_disjoint, c.low_confidence), (1, 1));
    }

    #[test]
    fn the_degenerate_anchors_bound_the_space() {
        let truth = vec![task("a", &["x"]), task("b", &["x"]), task("c", &["y"])];
        // Predict everything: perfectly safe, perfectly serial.
        let all = vec![set(&["x", "y"]); 3];
        let p = score_pairs(&truth, &all);
        assert_eq!((p.yield_, p.false_disjoint_rate), (0.0, 0.0));
        assert_eq!(score_serialization(&truth, &all).serialized, 2);
        // Predict nothing: every pair called disjoint, every collision admitted.
        let none = vec![BTreeSet::new(); 3];
        let p = score_pairs(&truth, &none);
        assert_eq!((p.yield_, p.false_disjoint_rate), (1.0, 1.0));
        assert_eq!(score_serialization(&truth, &none).collisions_admitted, 1);
    }

    #[test]
    fn a_random_control_is_reproducible_and_size_matched() {
        let files: Vec<String> = (0..20).map(|i| format!("f{i}")).collect();
        let a = random_predictions(&files, &[3, 0, 5], 7);
        assert_eq!(a, random_predictions(&files, &[3, 0, 5], 7));
        assert_eq!(
            a.iter().map(BTreeSet::len).collect::<Vec<_>>(),
            vec![3, 0, 5]
        );
    }
}
