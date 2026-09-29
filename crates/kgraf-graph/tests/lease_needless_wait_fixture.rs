//! Decision 0049's over-serialization measurement, reproduced and then compared
//! against what the real registry mechanism computes (test the mechanism, not the setting).
//!
//! **The first version of this file was not comparable to
//! 0049's 47.9% at all**, for three independent reasons, each corrected here:
//!
//! 1. `merged_range` joined every hunk in a file into one `(min, max)` span before
//!    comparing. That is coarser than 0049's per-hunk comparison, so it overlaps more
//!    readily and understates the needless-wait rate for reasons that have nothing to
//!    do with the registry being better -- it is a different, coarser question.
//! 2. The old test's denominator was **per shared-file handoff** (whatever
//!    `needless_wait_fraction` counts), while 0049's 47.9% is **per commit pair**: a
//!    pair with three shared files and one genuine overlap is one "collided" pair to
//!    0049, not two collided handoffs and one needless one.
//! 3. Every pair was replayed into the *same* registry without resetting it, so
//!    `needless_wait_fraction`'s `LAG` (ordered by insertion across the whole table)
//!    could compare a grant from one pair against a grant from a completely different,
//!    unrelated pair that happened to touch the same path later in fixture order.
//!
//! This file now reports three numbers, none of them presented as an "improvement":
//!
//! - [`reproduces_0049s_own_method`]: 0049's exact method (hunk-pair overlap within 3
//!   lines of git context, "over-serialized" is per pair) -- expected to land at
//!   0049's recorded **0.4790**.
//! - The same method with no context fuzzing (`ctx = 0`) -- the "raw" figure, exact
//!   hunk-range overlap only, expected at **0.5437**. Higher than 0049's own figure,
//!   because removing the 3-line tolerance makes *fewer* pairs count as overlapping.
//! - [`the_registrys_own_declared_range_reproduces_a_third_number`]: what the real
//!   `LeaseRegistry` computes when driven with its actual key, one merged range per
//!   file per grant (there is no per-hunk-list field in the schema; `declared_range`
//!   is `Option<(u32, u32)>`, singular). This is a *different, coarser* question
//!   again, run pair-isolated (a fresh store per pair) so `LAG` cannot cross pairs.
//!
//! Run these tests explicitly (`cargo test -p kgraf-graph --test
//! lease_needless_wait_fixture -- --ignored`) with `KGRAF_HUNKS_FIXTURE` set to the
//! fixture file (JSON lines, one commit per line with its hunks). A separate
//! implementation reading the same fixture computes the first two figures.

use kgraf_graph::leases::{Acquisition, LeaseRegistry};
use kgraf_graph::store::GraphStore;
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
struct FixtureCommit {
    repo: String,
    #[allow(dead_code)]
    commit: String,
    hunks: BTreeMap<String, Vec<[u32; 2]>>,
}

fn load_eligible() -> Vec<FixtureCommit> {
    let path = std::env::var("KGRAF_HUNKS_FIXTURE")
        .expect("set KGRAF_HUNKS_FIXTURE to the 0049 hunk fixture file");
    let text = std::fs::read_to_string(&path)
        .expect("the 0049 fixture must be readable at KGRAF_HUNKS_FIXTURE");
    let commits: Vec<FixtureCommit> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each fixture line is valid JSON"))
        .collect();
    // Same eligibility cap as the fixture's builder: 1 to 12 files.
    commits
        .into_iter()
        .filter(|c| (1..=12).contains(&c.hunks.len()))
        .collect()
}

/// Every pair, in the same repository, at most 5 positions apart in fixture order,
/// sharing at least one file. Returns the shared file names for each qualifying pair.
fn near_concurrent_sharing_pairs(eligible: &[FixtureCommit]) -> Vec<(usize, usize, Vec<String>)> {
    let mut pairs = Vec::new();
    for i in 0..eligible.len() {
        let window_end = (i + 5).min(eligible.len().saturating_sub(1));
        for j in (i + 1)..=window_end {
            let (a, b) = (&eligible[i], &eligible[j]);
            if a.repo != b.repo {
                continue;
            }
            let shared: Vec<String> = a
                .hunks
                .keys()
                .filter(|p| b.hunks.contains_key(*p))
                .cloned()
                .collect();
            if !shared.is_empty() {
                pairs.push((i, j, shared));
            }
        }
    }
    pairs
}

fn merged_range(hunks: &[[u32; 2]]) -> (u32, u32) {
    let start = hunks.iter().map(|h| h[0]).min().expect("at least one hunk");
    let end = hunks.iter().map(|h| h[1]).max().expect("at least one hunk");
    (start, end)
}

fn overlaps(a: (u32, u32), b: (u32, u32), context: u32) -> bool {
    !(a.0 > b.1 + context || b.0 > a.1 + context)
}

/// 0049's own method: two hunks collide if they lie within `context` lines of each
/// other; a pair is "over-serialized" (needless) if *no* shared file has any
/// colliding hunk pair. Matches the separate implementation exactly.
fn per_pair_needless_raw_hunks(
    eligible: &[FixtureCommit],
    pairs: &[(usize, usize, Vec<String>)],
    context: u32,
) -> f64 {
    let mut needless = 0usize;
    for (i, j, shared) in pairs {
        let (a, b) = (&eligible[*i], &eligible[*j]);
        let any_overlap = shared.iter().any(|p| {
            a.hunks[p].iter().any(|x| {
                b.hunks[p]
                    .iter()
                    .any(|y| overlaps((x[0], x[1]), (y[0], y[1]), context))
            })
        });
        if !any_overlap {
            needless += 1;
        }
    }
    needless as f64 / pairs.len() as f64
}

#[test]
#[ignore = "reads the 0049 fixture; run explicitly, see the module doc"]
fn reproduces_0049s_own_method_and_the_raw_no_context_figure() {
    let eligible = load_eligible();
    assert!(eligible.len() > 100, "the fixture shrank unexpectedly");
    let pairs = near_concurrent_sharing_pairs(&eligible);
    assert_eq!(
        pairs.len(),
        618,
        "the near-concurrent file-sharing population must match 0049's recorded count"
    );

    let ctx3 = per_pair_needless_raw_hunks(&eligible, &pairs, 3);
    let ctx0 = per_pair_needless_raw_hunks(&eligible, &pairs, 0);

    println!(
        "lease (0049 method reproduced, ctx=3 lines): {ctx3:.4} over {} pairs \
         (0049's recorded figure: 0.4790)",
        pairs.len()
    );
    println!(
        "lease (raw, no context fuzzing, ctx=0): {ctx0:.4} over {} pairs",
        pairs.len()
    );

    assert!(
        (ctx3 - 0.4790).abs() < 0.001,
        "0049's method must reproduce its own recorded figure: got {ctx3:.4}"
    );
    assert!(
        (ctx0 - 0.5437).abs() < 0.001,
        "the raw, no-context figure must match the independently-verified value: got {ctx0:.4}"
    );
    // ctx=0 is strictly harder to satisfy than ctx=3 (fewer pairs count as
    // overlapping), so removing the context tolerance can only raise the
    // over-serialized share, never lower it.
    assert!(ctx0 >= ctx3);
}

/// What the real `LeaseRegistry` reports, driven with its actual key shape: one
/// merged `(u32, u32)` range per file per grant, which is coarser than 0049's
/// per-hunk comparison. Each pair gets its own fresh in-memory store, so
/// `needless_wait_fraction`'s `LAG` never sees a grant from a different pair.
///
/// **This number answers a different, coarser question than the two above and must
/// not be read as comparable to 0049's 47.9%, let alone as an improvement on it.**
/// It is reported because it is the only one of the three actually produced by
/// shipped code, which is the point of testing the mechanism (test the mechanism, not the setting)
/// rather than a side computation.
#[test]
#[ignore = "reads the 0049 fixture; run explicitly, see the module doc"]
fn the_registrys_own_declared_range_reproduces_a_third_number() {
    let eligible = load_eligible();
    let pairs = near_concurrent_sharing_pairs(&eligible);

    let mut pair_needless = 0usize;
    let mut pairs_with_a_handoff = 0usize;
    for (i, j, shared) in &pairs {
        let (a, b) = (&eligible[*i], &eligible[*j]);
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let registry = LeaseRegistry::new(&store);

        let a_ranges: Vec<Option<(u32, u32)>> = shared
            .iter()
            .map(|p| Some(merged_range(&a.hunks[p])))
            .collect();
        match registry
            .acquire_all("a", "ta", shared, &a_ranges, 10)
            .expect("acquire a")
        {
            Acquisition::Granted(_) => {}
            Acquisition::Blocked(c) => panic!("a fresh path set must not block: {c:?}"),
        }
        registry.release_all("a").expect("release a");

        let b_ranges: Vec<Option<(u32, u32)>> = shared
            .iter()
            .map(|p| Some(merged_range(&b.hunks[p])))
            .collect();
        registry
            .acquire_all("b", "tb", shared, &b_ranges, 20)
            .expect("acquire b");

        if let Some(fraction) = registry.needless_wait_fraction().expect("query") {
            pairs_with_a_handoff += 1;
            // 0049's per-pair definition: over-serialized only if *no* shared file
            // overlapped, i.e. every handoff in this (isolated) pair was needless.
            if fraction >= 1.0 {
                pair_needless += 1;
            }
        }
    }

    assert_eq!(
        pairs_with_a_handoff,
        pairs.len(),
        "every qualifying pair shares a file, so every pair must produce a handoff"
    );
    let registry_figure = pair_needless as f64 / pairs.len() as f64;
    println!(
        "lease (this registry's own declared_range, merged-per-file, pair-isolated): \
         {registry_figure:.4} over {} pairs -- not comparable to 0049's per-hunk figures above",
        pairs.len()
    );
    assert!((0.0..=1.0).contains(&registry_figure));
}
