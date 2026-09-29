//! Layered write-set scheduling for lease admission.
//!
//! Ported from `haystack/components/agents/tool_calling.py`, `_schedule_tool_calls`
//! (deepset-ai/haystack, Apache-2.0, commit `8a5406eea71a0fc19e94c4b9a5cd96df2158a45a`,
//! verified 2026-09-26). Full provenance: `third_party/haystack/UPSTREAM`. Decision
//! 0055 names this "the best idea in the repo for kGraf": a layered topological
//! sort over each pending call's read and write sets, so independent work runs
//! together and dependent work waits exactly as long as it must.
//!
//! **What changed in the port.** Haystack schedules writes into a *mergeable* `State`
//! dict, so only read-after-write forces an order and "write-write creates no
//! dependency" is correct there. A kGraf write lease is mutual exclusion, not a
//! merge target, so [`WorkFootprint::conflicts_with`] treats every overlap, write vs
//! read, write vs write, in either direction, as a reason two items must not share a
//! layer. Unlike upstream, that reason carries no direction of its own (there is no
//! data flowing from one item to the other to observe), so [`schedule_layers`] orders
//! a conflicting pair by declaration index instead: this keeps the dependency graph a
//! DAG by construction rather than relying on upstream's cycle tie-break to resolve
//! it at run time. See `third_party/haystack/UPSTREAM` for the full comparison.

use std::collections::BTreeSet;

/// One pending unit of work's declared footprint: the lease keys (file paths and
/// hierarchical node ids, decision 0050) it will read and write.
///
/// A key is an opaque string on purpose. This module does not know whether a key
/// names a path or a node id, and does not need to: [`crate::leases::LeaseRegistry`]
/// is what interprets a key when a layer is actually admitted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkFootprint {
    reads: BTreeSet<String>,
    writes: BTreeSet<String>,
}

impl WorkFootprint {
    /// A footprint that touches nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a key this item reads.
    pub fn read(mut self, key: impl Into<String>) -> Self {
        self.reads.insert(key.into());
        self
    }

    /// Declare a key this item writes.
    pub fn write(mut self, key: impl Into<String>) -> Self {
        self.writes.insert(key.into());
        self
    }

    /// Whether this item must not run at the same time as `other`.
    ///
    /// Mirrors `_keys_intersect` generalized to a symmetric exclusion relation: any
    /// overlap between one side's writes and the other's reads-or-writes conflicts,
    /// checked in both directions. Two reads never conflict.
    pub fn conflicts_with(&self, other: &WorkFootprint) -> bool {
        overlaps(&self.writes, &other.reads)
            || overlaps(&self.writes, &other.writes)
            || overlaps(&self.reads, &other.writes)
    }
}

fn overlaps(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    // `BTreeSet::is_disjoint` walks both sets once; good enough for a scheduling pass
    // over one step's worth of pending work, which is small by construction (an LLM
    // turn's tool calls, or one dispatch round's write sets).
    !a.is_disjoint(b)
}

/// Group items into ordered batches ("layers") from their declared footprints.
///
/// Direct port of `_schedule_tool_calls`'s loop, with one adaptation to the
/// dependency direction (see the module doc and `third_party/haystack/UPSTREAM`):
/// upstream derives direction from which call reads what another writes, because its
/// writes are dataflow into a shared `State`. kGraf's items have no such flow,
/// two conflicting items simply must not share a layer, and *which* of them goes
/// first is not a correctness question, only a determinism one. So direction here is
/// declaration order: a conflicting pair `(i, j)` with `i < j` always produces the
/// edge `i -> j`, never the reverse. Every edge therefore points from a lower index
/// to a higher one, which makes the dependency graph acyclic **by construction**,
/// stronger than upstream's guarantee, which only prevents cycles for well-formed
/// read/write declarations and falls back to a tie-break for the pathological case.
/// That tie-break is kept below anyway: it is dead code under this module's own
/// invariant, cheap to keep, and it is exactly what upstream reaches for if a
/// non-monotonic conflict predicate is ever swapped in.
///
/// Items in one layer are mutually independent (no declared conflict with each
/// other) and may be admitted in any order or concurrently; layers run in the order
/// returned.
///
/// **The layer count is minimal only for the order `footprints` was given in, not
/// across every possible reordering of the same conflicts.** This is a greedy
/// first-fit over declaration order (identical in spirit to upstream, which makes
/// the same trade for the same reason: reordering to minimize layers is graph
/// coloring, NP-hard in general, for a scheduling pass that runs on every dispatch
/// round). A concrete case where it is not minimal: four items forming a 4-cycle of
/// pairwise conflicts in declaration order 0-1, 1-2, 2-3, 3-0 (so 0 and 2 never
/// conflict, nor do 1 and 3) schedule into four layers here, one item at a time,
/// even though `{0, 2}` and `{1, 3}` is a valid 2-layer grouping, the two-coloring
/// is invisible to a scan that only ever looks at *already-scheduled* items, not at
/// what the *unscheduled* remainder could still be grouped with. Callers that
/// dispatch large, densely-conflicting batches and care about layer count, not just
/// correctness, should presort by an ordering more likely to cluster non-conflicting
/// items together (by declared path or node id, say) before calling this.
///
/// Returns a list of layers, each a list of indices into `footprints`.
pub fn schedule_layers(footprints: &[WorkFootprint]) -> Vec<Vec<usize>> {
    let n = footprints.len();

    // deps[j] = indices before j, in declaration order, that conflict with j.
    let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for j in 0..n {
        for i in 0..j {
            if footprints[i].conflicts_with(&footprints[j]) {
                deps[j].insert(i);
            }
        }
    }

    let mut scheduled = vec![false; n];
    let mut done: BTreeSet<usize> = BTreeSet::new();
    let mut layers: Vec<Vec<usize>> = Vec::new();

    while done.len() < n {
        // Always non-empty: `deps[j] ⊆ 0..j` above means the smallest unscheduled
        // index has every dependency already scheduled (each dependency has a
        // strictly smaller index, and everything smaller is done by induction), so
        // it always passes the filter. No fallback branch is needed, and so no
        // `.expect()` is either -- an earlier version's
        // defensive-but-dead branch was exactly the kind of panic-shaped code this
        // project's "no unwrap outside tests" convention exists to catch, even when,
        // as here, it provably never fires.
        let ready: Vec<usize> = (0..n)
            .filter(|&k| !scheduled[k] && deps[k].is_subset(&done))
            .collect();
        for &k in &ready {
            scheduled[k] = true;
        }
        done.extend(&ready);
        layers.push(ready);
    }
    layers
}

/// Admit a scheduled set of work layer by layer.
///
/// This is the seam between the scheduler above (which only orders indices) and
/// [`crate::leases::LeaseRegistry`] (which knows how to admit one). `admit_one` is
/// called for every index, in layer order; within a layer the order is index order
/// but nothing here or in the schedule requires it, every item in one layer is,
/// by construction, free of declared conflict with every other item in that layer,
/// so a caller with a thread pool may fan them out concurrently instead.
///
/// Returns each layer's outcomes, alongside the original index, so a caller can tell
/// a scheduling decision (which layer an item landed in) from an admission decision
/// (whether `admit_one` accepted it).
pub fn admit_layers<F>(layers: &[Vec<usize>], mut admit_one: F) -> Vec<Vec<(usize, bool)>>
where
    F: FnMut(usize) -> bool,
{
    layers
        .iter()
        .map(|layer| layer.iter().map(|&i| (i, admit_one(i))).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(reads: &[&str], writes: &[&str]) -> WorkFootprint {
        let mut f = WorkFootprint::new();
        for r in reads {
            f = f.read(*r);
        }
        for w in writes {
            f = f.write(*w);
        }
        f
    }

    #[test]
    fn disjoint_items_land_in_one_layer() {
        let items = vec![wf(&[], &["a"]), wf(&[], &["b"]), wf(&[], &["c"])];
        let layers = schedule_layers(&items);
        assert_eq!(layers, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn a_write_write_conflict_serializes_into_separate_layers() {
        // Upstream would call this "no dependency" because their writes merge. A lease
        // is exclusion, so two writers of the same key must not share a layer.
        let items = vec![wf(&[], &["a"]), wf(&[], &["a"])];
        let layers = schedule_layers(&items);
        assert_eq!(layers, vec![vec![0], vec![1]], "{layers:?}");
    }

    #[test]
    fn read_after_write_lands_in_a_later_layer() {
        let items = vec![wf(&[], &["a"]), wf(&["a"], &[])];
        let layers = schedule_layers(&items);
        assert_eq!(layers, vec![vec![0], vec![1]]);
    }

    #[test]
    fn a_mutual_read_write_pair_still_serializes_in_declaration_order() {
        // Item 0 writes `a` and reads `b`; item 1 writes `b` and reads `a`. Under
        // upstream's read-after-write-only rule this would be a genuine dependency
        // cycle needing the tie-break. Here it is not a cycle at all: the edge is
        // `0 -> 1` because 0 was declared first, full stop, regardless of which one
        // "really" depends on the other.
        let items = vec![wf(&["b"], &["a"]), wf(&["a"], &["b"])];
        let layers = schedule_layers(&items);
        assert_eq!(layers, vec![vec![0], vec![1]]);
    }

    #[test]
    fn two_reads_of_the_same_key_still_batch_together() {
        let items = vec![wf(&["a"], &[]), wf(&["a"], &[])];
        let layers = schedule_layers(&items);
        assert_eq!(layers, vec![vec![0, 1]]);
    }

    #[test]
    fn a_mixed_batch_produces_independent_and_dependent_layers() {
        // 0 writes `a`; 1 reads `a` (depends on 0); 2 is disjoint from both.
        let items = vec![wf(&[], &["a"]), wf(&["a"], &[]), wf(&[], &["z"])];
        let layers = schedule_layers(&items);
        assert_eq!(layers, vec![vec![0, 2], vec![1]], "{layers:?}");
    }

    #[test]
    fn empty_input_produces_no_layers() {
        assert!(schedule_layers(&[]).is_empty());
    }

    #[test]
    fn admit_layers_calls_every_index_exactly_once_in_layer_order() {
        let items = vec![wf(&[], &["a"]), wf(&["a"], &[]), wf(&[], &["z"])];
        let layers = schedule_layers(&items);
        let mut seen = Vec::new();
        let outcomes = admit_layers(&layers, |i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![0, 2, 1], "must visit layer 0 before layer 1");
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().flatten().all(|&(_, ok)| ok));
    }

    #[test]
    fn admit_layers_reports_a_refusal_without_stopping_the_batch() {
        let items = vec![wf(&[], &["a"]), wf(&[], &["b"])];
        let layers = schedule_layers(&items);
        let outcomes = admit_layers(&layers, |i| i == 0);
        assert_eq!(outcomes, vec![vec![(0, true), (1, false)]]);
    }

    /// A 4-cycle of conflicts (0-1, 1-2, 2-3, 3-0; 0-2 and 1-3 independent) schedules
    /// into 4 layers here even though a 2-layer grouping exists. This is the concrete
    /// case the function doc names for "minimal only for the declared order", a
    /// regression test for the *documented* behavior, not a bug: it must keep failing
    /// this way unless the algorithm changes on purpose, in which case the doc above
    /// changes with it.
    #[test]
    fn layering_is_not_globally_minimal_across_reorderings() {
        let items = vec![
            wf(&[], &["a", "d"]),
            wf(&[], &["a", "b"]),
            wf(&[], &["b", "c"]),
            wf(&[], &["c", "d"]),
        ];
        let layers = schedule_layers(&items);
        assert_eq!(layers, vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    /// A scale test (decision 0055's build plan calls for one): 1,000 items, each
    /// conflicting only with its immediate declaration-order neighbor, so every
    /// dependency lookup is real work rather than a vacuous empty set. This is
    /// O(n^2) in `deps` construction by design (every pair is compared once); the
    /// property under test is that it finishes and produces a correct, exhaustive,
    /// non-overlapping layering, not a particular wall-clock budget -- `benches/leases.rs`
    /// owns the timing budget for the registry this feeds into.
    #[test]
    fn a_thousand_items_schedule_correctly_and_without_blowing_up() {
        const N: usize = 1_000;
        // Item k writes key `k` and key `k+1`, so consecutive items conflict and
        // items two or more apart never do -- a long chain, not one giant clique.
        let items: Vec<WorkFootprint> = (0..N)
            .map(|k| wf(&[], &[&k.to_string(), &(k + 1).to_string()]))
            .collect();
        let layers = schedule_layers(&items);

        let scheduled: BTreeSet<usize> = layers.iter().flatten().copied().collect();
        assert_eq!(
            scheduled.len(),
            N,
            "every item must be scheduled exactly once"
        );
        assert_eq!(
            scheduled,
            (0..N).collect::<BTreeSet<usize>>(),
            "every index 0..N must appear"
        );

        // No two conflicting items share a layer.
        for layer in &layers {
            for a in 0..layer.len() {
                for b in (a + 1)..layer.len() {
                    assert!(
                        !items[layer[a]].conflicts_with(&items[layer[b]]),
                        "layer {layer:?} put conflicting items {} and {} together",
                        layer[a],
                        layer[b]
                    );
                }
            }
        }

        // This chain is 2-colorable (alternate items are mutually independent), but
        // each item's one conflict is with its immediate declaration-order
        // predecessor, so the greedy schedule -- which is explicitly not claimed to
        // be globally minimal, see the function doc -- serializes it into one item
        // per layer rather than finding the 2-coloring. That is the point of this
        // assertion: it is the same non-minimality the 4-cycle test shows, at scale,
        // not a different, worse bug.
        assert_eq!(
            layers.len(),
            N,
            "a chain of adjacent-only conflicts fully serializes under this greedy \
             schedule; a smarter ordering would find 2 layers, which is exactly what \
             the function doc's non-minimality note is warning a caller about"
        );
    }
}
