//! Regression test for an on-disk growth defect: the per-grant
//! `MAX(generation) WHERE path = ?` lookup in `lease_history` was indexed on `path`
//! alone, so SQLite scanned every historical row for a path to find the maximum
//! instead of seeking to the end of a `(path, generation)` ordering. Measured before
//! this fix, on disk, acquiring and releasing the same path repeatedly: 111us/op at
//! 2,000 rows of accumulated history, 377us/op at 10,000 -- growing with history size
//! rather than staying flat, which is the signature of a missing index, not of the
//! mechanism itself being slow.
//!
//! `lease_history_path_generation` and `node_lease_history_node_generation`
//! (`crates/kgraf-graph/src/store.rs`) are the fix. This test reproduces the
//! measurement on disk and asserts the growth is gone, i.e. the cost of the tenth
//! batch is not meaningfully larger than the cost of the first.
//!
//! Ignored by default: it does real disk I/O across 10,000 acquire/release cycles,
//! which is unnecessary weight for the default `cargo test` loop. Run explicitly:
//! `cargo test --release -p kgraf-graph --test lease_disk_growth -- --ignored --nocapture`.

use kgraf_graph::leases::{Acquisition, LeaseRegistry};
use kgraf_graph::store::GraphStore;

#[test]
#[ignore = "does real disk I/O across 10,000 acquire/release cycles; run explicitly"]
fn acquiring_the_same_path_repeatedly_does_not_slow_down_as_history_grows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = GraphStore::open(&dir.path().join("g.db"), dir.path()).expect("store");
    let registry = LeaseRegistry::new(&store);
    let path = vec!["a.rs".to_owned()];

    let mut clock = 0i64;
    let mut per_batch_us = Vec::new();
    for batch in 0..5 {
        let start = std::time::Instant::now();
        for _ in 0..2_000 {
            clock += 1;
            match registry
                .acquire_all("agent-a", "t", &path, &[], clock)
                .expect("acquire")
            {
                Acquisition::Granted(mut l) => registry.release(l.remove(0)).expect("release"),
                Acquisition::Blocked(c) => panic!("uncontended acquire must not block: {c:?}"),
            }
        }
        let us_per_op = start.elapsed().as_micros() as f64 / 2_000.0;
        println!(
            "lease on-disk acquire+release, batch {batch} (history {} rows): {us_per_op:.1} us/op",
            (batch + 1) * 2_000
        );
        per_batch_us.push(us_per_op);
    }

    let first = per_batch_us[0];
    let last = *per_batch_us.last().expect("five batches ran");
    println!(
        "lease on-disk: first batch {first:.1} us/op, last batch (10,000 rows of history) \
         {last:.1} us/op, ratio {:.2}x",
        last / first
    );
    // Before the index existed, this ratio was well over 3x (111us -> 377us across a
    // smaller span than this test even covers). A flat mechanism should not need
    // meaningfully more time per operation once there is more history behind it.
    assert!(
        last < first * 2.0,
        "acquisition cost grew with history size: {first:.1}us/op at 2,000 rows vs \
         {last:.1}us/op at 10,000 -- the (path, generation) index may be missing"
    );
}
