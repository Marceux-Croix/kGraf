//! Lease acquire/release time, uncontended and under contention (decision 0055's
//! budget). No prior budget exists for this mechanism -- this run is
//! the baseline a future change is measured against, since a benchmark
//! budget needs a recorded baseline: the budget from here forward
//! is this benchmark's own recorded numbers, read from `target/criterion/`.
//!
//! Four shapes, matching what the registry actually does on each path:
//!
//! - **File-level, uncontended**: acquire then release one path nobody else wants.
//!   This is the whole cost of decision 0022's mechanism on the happy path.
//! - **File-level, contended**: a second agent already holds the path, so every call
//!   discovers a conflict and grants nothing. This is the cost `Acquisition::Blocked`
//! - **Node-level, uncontended**: acquire then release one leaf of a five-deep chain.
//!   The extra cost over the file-level case is the recursive ancestor/descendant
//!   query (decisions 0049, 0050), which the file-level path never runs.
//! - **Node-level, contended**: an ancestor three levels up already holds a lease, so
//!   every call must walk the chain and still find the conflict.

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_graph::NodeKind;
use kgraf_graph::leases::LeaseRegistry;
use kgraf_graph::store::{FileRecord, GraphStore, NodeRecord};
use std::hint::black_box;

fn store() -> (tempfile::TempDir, GraphStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    (dir, store)
}

/// A chain of `depth` nodes, each the previous one's parent, in one file, through the
/// same public API the indexer uses. Returns the leaf id (deepest) and the root id.
fn insert_chain(store: &GraphStore, depth: usize) -> (String, String) {
    let file = FileRecord {
        id: "f".to_owned(),
        path: "f.rs".to_owned(),
        content_hash: "h".to_owned(),
        size_bytes: 0,
        mtime_ns: 0,
        language: None,
        line_count: 1,
    };
    store.upsert_file(&file).expect("file");
    let mut parent: Option<String> = None;
    let mut nodes = Vec::with_capacity(depth);
    for i in 0..depth {
        let id = format!("n{i}");
        nodes.push(NodeRecord {
            id: id.clone(),
            file_id: file.id.clone(),
            path: file.path.clone(),
            kind: NodeKind::Function,
            name: id.clone(),
            qualified_name: id.clone(),
            parent_id: parent.clone(),
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 0,
            signature: String::new(),
            identity_stable: true,
        });
        parent = Some(id);
    }
    store
        .replace_file_nodes(&file.id, &nodes, &[], &[])
        .expect("nodes");
    (
        nodes.last().unwrap().id.clone(),
        nodes.first().unwrap().id.clone(),
    )
}

fn bench_file_uncontended(c: &mut Criterion) {
    let (_dir, store) = store();
    let registry = LeaseRegistry::new(&store);
    let path = vec!["src/lib.rs".to_owned()];
    let mut clock = 0i64;
    c.bench_function("path_acquire_release_uncontended", |b| {
        b.iter(|| {
            clock += 1;
            let lease = match registry
                .acquire_all("agent-a", "t1", black_box(&path), &[], clock)
                .expect("acquire")
            {
                kgraf_graph::leases::Acquisition::Granted(mut l) => l.remove(0),
                kgraf_graph::leases::Acquisition::Blocked(c) => {
                    panic!("uncontended acquire must not block: {c:?}")
                }
            };
            registry.release(lease).expect("release");
        });
    });
}

fn bench_file_contended(c: &mut Criterion) {
    let (_dir, store) = store();
    let registry = LeaseRegistry::new(&store).with_ttl_ms(i64::MAX / 2);
    let path = vec!["src/lib.rs".to_owned()];
    registry
        .acquire_all("holder", "t0", &path, &[], 0)
        .expect("seed holder");
    c.bench_function("path_acquire_blocked_under_contention", |b| {
        b.iter(|| {
            let outcome = registry
                .acquire_all("agent-a", "t1", black_box(&path), &[], 1)
                .expect("acquire");
            assert!(matches!(
                outcome,
                kgraf_graph::leases::Acquisition::Blocked(_)
            ));
        });
    });
}

fn bench_node_uncontended(c: &mut Criterion) {
    let (_dir, store) = store();
    let (leaf, _root) = insert_chain(&store, 5);
    let registry = LeaseRegistry::new(&store);
    let request = vec![leaf];
    let mut clock = 0i64;
    c.bench_function("node_acquire_release_uncontended_depth5", |b| {
        b.iter(|| {
            clock += 1;
            let lease = match registry
                .acquire_node_set("agent-a", "t1", black_box(&request), clock)
                .expect("acquire")
            {
                kgraf_graph::leases::NodeAcquisition::Granted(mut l) => l.remove(0),
                other => panic!("uncontended acquire must not block: {other:?}"),
            };
            registry.release_node(lease).expect("release");
        });
    });
}

fn bench_node_contended(c: &mut Criterion) {
    let (_dir, store) = store();
    let (leaf, root) = insert_chain(&store, 5);
    let registry = LeaseRegistry::new(&store).with_ttl_ms(i64::MAX / 2);
    registry
        .acquire_node_set("holder", "t0", &[root], 0)
        .expect("seed holder");
    let request = vec![leaf];
    c.bench_function("node_acquire_blocked_under_contention_depth5", |b| {
        b.iter(|| {
            let outcome = registry
                .acquire_node_set("agent-a", "t1", black_box(&request), 1)
                .expect("acquire");
            assert!(matches!(
                outcome,
                kgraf_graph::leases::NodeAcquisition::Blocked(_)
            ));
        });
    });
}

criterion_group!(
    benches,
    bench_file_uncontended,
    bench_file_contended,
    bench_node_uncontended,
    bench_node_contended
);
criterion_main!(benches);
