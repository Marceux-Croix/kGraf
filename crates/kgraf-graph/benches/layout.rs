//! Layout time for synthetic graphs of 10,000 and 100,000 nodes.
//!
//! The graph imitates a code index: files of about 20 definitions, 40
//! definitions per directory cluster, one `contains` edge per definition and
//! two name-resolved `calls` edges per definition, 80% inside the directory.
//! `layout_graph` is the whole derivation the renderer needs: positions from
//! identity and call depth, size from degree and kind.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use kgraf_graph::NodeKind;
use kgraf_graph::layout::{self, LayoutEdge, LayoutNode};
use kgraf_graph::store::EdgeKind;
use std::hint::black_box;

struct Synthetic {
    ids: Vec<String>,
    dirs: Vec<String>,
    kinds: Vec<NodeKind>,
    edges: Vec<LayoutEdge>,
}

fn synthetic(n: usize) -> Synthetic {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as usize
    };
    let per_dir = 40;
    let per_file = 20;
    let ids = (0..n).map(|i| format!("{i:016x}")).collect();
    let dirs = (0..n)
        .map(|i| format!("crates/c{}/src/m{}", i / (per_dir * 4), (i / per_dir) % 4))
        .collect();
    let kinds = (0..n)
        .map(|i| {
            if i % per_file == 0 {
                NodeKind::File
            } else {
                NodeKind::Function
            }
        })
        .collect();
    let mut edges = Vec::with_capacity(n * 3);
    for i in 0..n {
        let file = i - i % per_file;
        if i != file {
            edges.push(LayoutEdge {
                src: file as u32,
                dst: i as u32,
                kind: EdgeKind::Contains,
            });
        }
        let dir_start = i - i % per_dir;
        for _ in 0..2 {
            let target = if next() % 10 < 8 {
                (dir_start + next() % per_dir).min(n - 1)
            } else {
                next() % n
            };
            edges.push(LayoutEdge {
                src: i as u32,
                dst: target as u32,
                kind: EdgeKind::Calls,
            });
        }
    }
    Synthetic {
        ids,
        dirs,
        kinds,
        edges,
    }
}

fn bench_layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("layout_graph");
    group.sample_size(20);
    for n in [10_000usize, 100_000] {
        let graph = synthetic(n);
        let nodes: Vec<LayoutNode<'_>> = (0..n)
            .map(|i| LayoutNode {
                id: &graph.ids[i],
                directory: &graph.dirs[i],
                kind: graph.kinds[i],
            })
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| layout::layout_graph(black_box(&nodes), black_box(&graph.edges)))
        });
    }
    group.finish();

    let mut group = c.benchmark_group("call_depths");
    group.sample_size(20);
    for n in [10_000usize, 100_000] {
        let graph = synthetic(n);
        let calls: Vec<(u32, u32)> = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .map(|e| (e.src, e.dst))
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| layout::call_depths(black_box(n), black_box(&calls)))
        });
    }
    group.finish();
}

/// A store of `n` functions whose call graph is a layered DAG with the depth
/// distribution measured on this repository (84% at depth 0, 11% at 1, 4% at
/// 2, 1% deeper; max depth 4), not one giant strongly connected component.
/// Files hold 20 functions of one layer; one directory in seven holds the
/// largest cluster, the rest spread over 120 directories. Files are written
/// deepest layer first, so every call edge's callee already exists (the store
/// enforces foreign keys). Every node is left at its seed, so the first
/// `refresh_layout` rewrites almost every row.
fn seeded_store(n: usize, root: &std::path::Path) -> kgraf_graph::GraphStore {
    use kgraf_graph::{FileRecord, GraphStore, NodeRecord};
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as usize
    };
    let share = [0.84, 0.11, 0.04, 0.008, 0.002];
    let mut layers: Vec<Vec<usize>> = Vec::new();
    let mut start = 0usize;
    for (level, s) in share.iter().enumerate() {
        let count = if level == share.len() - 1 {
            n - start
        } else {
            (n as f64 * s) as usize
        };
        layers.push((start..start + count).collect());
        start += count;
    }
    let id = |i: usize| format!("{i:016x}");
    let store = GraphStore::open_in_memory(root).expect("store");
    let mut file_no = 0usize;
    for level in (0..layers.len()).rev() {
        for chunk in layers[level].chunks(20) {
            let dir = if file_no.is_multiple_of(7) {
                "pkg0/src".to_string()
            } else {
                format!("pkg{}/src", 1 + file_no % 120)
            };
            let path = format!("{dir}/f{file_no}.rs");
            let file_id = format!("file{file_no}");
            let file = FileRecord {
                id: file_id.clone(),
                path: path.clone(),
                content_hash: file_id.clone(),
                size_bytes: 0,
                mtime_ns: 0,
                language: None,
                line_count: 20,
            };
            let nodes: Vec<NodeRecord> = chunk
                .iter()
                .map(|&i| NodeRecord {
                    id: id(i),
                    file_id: file_id.clone(),
                    path: path.clone(),
                    kind: NodeKind::Function,
                    name: format!("f{i}"),
                    qualified_name: format!("f{i}"),
                    parent_id: None,
                    start_line: 1,
                    end_line: 1,
                    start_byte: 0,
                    end_byte: 0,
                    signature: String::new(),
                    identity_stable: true,
                })
                .collect();
            // Each node calls one or two nodes of the next layer down, so its
            // callees sit at its depth plus one.
            let mut edges = Vec::new();
            if let Some(below) = layers.get(level + 1).filter(|l| !l.is_empty()) {
                for &i in chunk {
                    for _ in 0..(1 + next() % 2) {
                        let callee = below[next() % below.len()];
                        edges.push((id(i), id(callee), EdgeKind::Calls, 100));
                    }
                }
            }
            store
                .replace_file_nodes(&file, &nodes, &edges, &[])
                .expect("rows");
            file_no += 1;
        }
    }
    store
}

fn bench_refresh(c: &mut Criterion) {
    let n = 100_000;
    let root = std::env::temp_dir();
    let mut group = c.benchmark_group("refresh_layout");
    group.sample_size(10);

    let steady = seeded_store(n, &root);
    let first = layout::cache::refresh_layout(&steady).expect("refresh");
    eprintln!("refresh_layout/{n}: first refresh {first:?}");
    group.bench_function(BenchmarkId::new("steady_state", n), |b| {
        b.iter(|| {
            let r = layout::cache::refresh_layout(black_box(&steady)).expect("refresh");
            assert_eq!(r.moved, 0);
        })
    });

    group.bench_function(BenchmarkId::new("every_row_moves", n), |b| {
        b.iter_batched(
            || seeded_store(n, &root),
            |store| {
                let r = layout::cache::refresh_layout(&store).expect("refresh");
                assert!(r.moved > n / 2);
                store
            },
            criterion::BatchSize::PerIteration,
        )
    });
    group.finish();
}

criterion_group!(benches, bench_layout, bench_refresh);
criterion_main!(benches);
