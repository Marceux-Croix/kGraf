//! Real-data galaxy budgets: scene build, frame time and live-update cost on
//! scenes built from real indexes, never from the demo generator.
//!
//! Scenes:
//! - `repo`: this repository, indexed by the real indexer into a temporary database.
//! - `synth-10k`, `synth-100k`: generated Rust trees (20 functions per file, 25 files
//!   per directory, two calls per function, 10% of them across directories), written
//!   to disk and indexed by the real indexer, so the scene goes through the same
//!   store, layout cache, snapshot and model code as a user's project.
//! - `KGRAF_BENCH_PROJECT=<dir>` adds a project that already has an index (for
//!   example a large public repository indexed with `kgraf index`).
//!
//! Measured per scene: snapshot read (SQLite), model and scene build, full upload,
//! frame wall time (encode, submit, wait) on the GPU and CPU budgets at the home view,
//! and the cost of one live change batch (one file of about 21 nodes inserted, then
//! removed, applied to a rendered galaxy and uploaded in place).
//!
//! Every galaxy here is folded as the shell's project view folds it (decision 0068,
//! [`Chunks`] at the default budget): the frames, the change batch and the one-line
//! edit include the refold, and the fold itself is measured (a whole recompute, and
//! one user expansion and fold of the largest folded node, each uploaded). The drawn
//! count before (every node) and after the fold is reported. The **unfolded** frame
//! (every node expanded, the renderer's worst case) is measured and held to the same
//! frame budgets, so a renderer regression cannot hide behind the fold.
//!
//! **Method (changed 2026-09-27 with the fold):** every upload in a timed loop is
//! followed by a queue submit, as the next frame's render follows it in the app.
//! Without one, wgpu keeps every staged write pending and a loop measures the pile-up
//! (one batch's upload grew from 1.5 to 16 ms over 400 cycles). So change-batch numbers
//! from before this date (no submit, no refold) are not comparable one to one.
//!
//! Budgets are checked after criterion runs, from separate median timings, and a miss
//! panics, so `cargo bench` fails (a missed budget fails the run).
//!
//! Run: `cargo bench -p kgraf-galaxy --bench project`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use kgraf_galaxy::fold::Toggle;
use kgraf_galaxy::gpu::RenderTarget;
use kgraf_galaxy::{Chunks, Galaxy, ProjectModel, RenderBudget, Scene};
use kgraf_graph::snapshot::{
    GraphSnapshot, ProjectIndex, Resolved, SceneNode, open_existing, read_snapshot,
};
use kgraf_graph::{GraphStore, Indexer, NodeKind};

const W: u32 = 1920;
const H: u32 = 1080;

/// Fold recompute or one user toggle, uploaded: a click must feel immediate. Set as
/// the frame budget at 10,000 nodes (the 60 Hz frame at 100,000 would allow more):
/// a toggle is one frame's extra work.
const FOLD_MAX_MS: f64 = 8.0;

/// Budgets in milliseconds, set from the measurement on an Apple M4 Max (Metal,
/// 2026-09-26) with about 2 to 3 times headroom for a slower machine, except the
/// frame budgets, which are the interactivity bars themselves (8 ms at 10,000 nodes,
/// 60 Hz at 100,000). Measured then: 10,500 synthetic nodes read 10.7, build 5.7,
/// frame 1.20, batch 0.096; 100,275 nodes read 132.6, build 65.0, frame 4.54, batch
/// 0.281; 133,824 real nodes (30 public crates) read 195.0, build 89.9, frame 3.13,
/// batch 0.400. A budget applies to scenes of 0.9 to 1.5 times its node count.
struct Budget {
    nodes: usize,
    snapshot_read_ms: f64,
    scene_build_ms: f64,
    frame_gpu_ms: f64,
    /// The CPU budget's frame (added 2026-09-27): the folded frame had regressed from
    /// 0.37 to 0.86 to 2.05 ms at 100,000 nodes before the fold compacted its upload;
    /// measured 0.3 to 0.6 ms at base on this machine under load, so 1.5 ms catches a
    /// regression of that size with room for noise.
    frame_cpu_ms: f64,
    change_batch_ms: f64,
}

const BUDGETS: [Budget; 2] = [
    Budget {
        nodes: 10_000,
        snapshot_read_ms: 30.0,
        scene_build_ms: 15.0,
        frame_gpu_ms: 8.0,
        frame_cpu_ms: 1.5,
        change_batch_ms: 1.0,
    },
    Budget {
        nodes: 100_000,
        snapshot_read_ms: 450.0,
        scene_build_ms: 200.0,
        frame_gpu_ms: 16.7,
        frame_cpu_ms: 1.5,
        change_batch_ms: 2.0,
    },
];

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn gpu() -> Gpu {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .expect("adapter");
    let info = adapter.get_info();
    eprintln!(
        "adapter: {} ({:?}, {})",
        info.name, info.device_type, info.backend
    );
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("device");
    Gpu { device, queue }
}

fn frame(g: &mut Galaxy, gpu: &Gpu, target: &RenderTarget) {
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    g.render(&gpu.device, &gpu.queue, &mut encoder, target)
        .expect("render");
    gpu.queue.submit([encoder.finish()]);
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
}

/// Deterministic generator (splitmix64).
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) % n.max(1) as u64) as usize
    }
}

const FNS: usize = 20;
const FILES: usize = 25;

fn file_path(d: usize, f: usize) -> String {
    format!("pkg{}/mod{d}/src/f{f}.rs", d % 16)
}

fn file_body(d: usize, f: usize, dirs: usize, rng: &mut Rng) -> String {
    let mut s = String::new();
    for j in 0..FNS {
        let local = format!("f{d}_{f}_{}", (j + 1) % FNS);
        let (d2, f2) = if rng.below(10) == 0 {
            (rng.below(dirs), rng.below(FILES))
        } else {
            (d, rng.below(FILES))
        };
        let other = format!("f{d2}_{f2}_{}", rng.below(FNS));
        s.push_str(&format!(
            "pub fn f{d}_{f}_{j}() {{ {local}(); {other}(); }}\n"
        ));
    }
    s
}

/// Writes and indexes a tree of about `nodes` nodes (functions plus file nodes).
fn synth_project(nodes: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let per_dir = FILES * (FNS + 1);
    let dirs = nodes.div_ceil(per_dir);
    let mut rng = Rng(7);
    for d in 0..dirs {
        for f in 0..FILES {
            let path = dir.path().join(file_path(d, f));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, file_body(d, f, dirs, &mut rng)).expect("write");
        }
    }
    index_into(dir.path(), &dir.path().join(".kgraf"));
    dir
}

fn index_into(root: &Path, state: &Path) -> GraphStore {
    std::fs::create_dir_all(state).expect("state dir");
    let store = GraphStore::open(&state.join("graph.db"), root).expect("store");
    let t0 = Instant::now();
    let report = Indexer::new(&store).index().expect("index");
    eprintln!(
        "indexed {}: {} nodes, {} edges in {:.1} s",
        root.display(),
        report.nodes,
        report.edges,
        t0.elapsed().as_secs_f64()
    );
    store
}

fn median_ms(mut f: impl FnMut() -> Duration, n: usize) -> f64 {
    let mut v: Vec<f64> = (0..n).map(|_| f().as_secs_f64() * 1000.0).collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// One file's worth of new nodes (a file node plus 20 functions) in a fresh directory,
/// at the layout's seed positions, and the batch that removes them again.
fn change_batches(snap: &GraphSnapshot) -> (Resolved, Resolved) {
    let dir = "bench/live/src";
    let path = format!("{dir}/new.rs");
    let mut nodes = Vec::new();
    for j in 0..=FNS {
        let id = format!("bench-node-{j}");
        let p = kgraf_graph::layout::position_for(dir, &id);
        nodes.push(SceneNode {
            id,
            path: path.clone(),
            kind: if j == 0 {
                NodeKind::File
            } else {
                NodeKind::Function
            },
            qualified_name: format!("bench_{j}"),
            position: p,
        });
    }
    // Contains edges from the file node, plus one call into the existing graph each.
    let anchor = snap.nodes.first().map(|n| n.id.clone()).unwrap_or_default();
    let mut edges = Vec::new();
    for j in 1..=FNS {
        edges.push(kgraf_graph::snapshot::SceneEdge {
            id: format!("bench-contains-{j}"),
            src: "bench-node-0".into(),
            dst: format!("bench-node-{j}"),
            kind: kgraf_graph::EdgeKind::Contains,
        });
        edges.push(kgraf_graph::snapshot::SceneEdge {
            id: format!("bench-calls-{j}"),
            src: format!("bench-node-{j}"),
            dst: anchor.clone(),
            kind: kgraf_graph::EdgeKind::Calls,
        });
    }
    let insert = Resolved {
        nodes,
        edges,
        ..Resolved::default()
    };
    let remove = Resolved {
        removed_nodes: insert.nodes.iter().map(|n| n.id.clone()).collect(),
        removed_edges: insert.edges.iter().map(|e| e.id.clone()).collect(),
        ..Resolved::default()
    };
    (insert, remove)
}

struct Case {
    name: String,
    snapshot: GraphSnapshot,
    read_ms: f64,
    /// Kept alive so the tree and its index outlive the case.
    _dir: Option<tempfile::TempDir>,
    /// The tree in `_dir` is the bench's own and its index lives inside it, so the
    /// one-line-edit case may edit and reindex it.
    editable: bool,
}

/// A one-line edit must append change-log rows for its own file, not the tree:
/// before `resolve_calls` wrote only differences, this edit appended 400,083 rows
/// at 100,000 nodes (probe, 2026-09-26).
const EDIT_MAX_LOG_ROWS: usize = 500;
/// Applying that batch to a rendered galaxy and uploading it in place.
const EDIT_MAX_APPLY_MS: f64 = 2.0;

struct EditResult {
    reindex_ms: f64,
    log_rows: usize,
    feed_rows: usize,
    touched: usize,
    latency_ms: f64,
    apply_ms: f64,
}

/// Appends a comment line to one file, reindexes, and takes the feed's batch.
fn one_line_edit(root: &Path, gpu: &Gpu, target: &RenderTarget) -> EditResult {
    use kgraf_galaxy::feed::{Feed, FeedEvent, FeedOptions};
    let feed = Feed::spawn(
        root.to_path_buf(),
        FeedOptions {
            poll: Duration::from_millis(10),
            ..FeedOptions::default()
        },
        || {},
    )
    .expect("feed");
    let snapshot = loop {
        match feed
            .recv_timeout(Duration::from_secs(120))
            .expect("snapshot")
        {
            FeedEvent::Loaded { snapshot, .. } => break *snapshot,
            FeedEvent::Failed(e) => panic!("feed: {e}"),
            _ => {}
        }
    };
    let (mut model, scene) = ProjectModel::build(&snapshot).expect("build");
    let mut g = Galaxy::new(&gpu.device, scene, RenderBudget::Gpu);
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    frame(&mut g, gpu, target);

    let path = root.join(file_path(0, 0));
    let body = std::fs::read_to_string(&path).expect("read");
    std::fs::write(&path, format!("{body}// edited\n")).expect("write");
    let store = GraphStore::open(&root.join(".kgraf/graph.db"), root).expect("store");
    let t0 = Instant::now();
    let report = Indexer::new(&store).index().expect("reindex");
    let reindex_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let t1 = Instant::now();
    // Exactly one batch: the feed holds the window while the pass is marked running.
    let (batch, feed_rows) = match feed.recv_timeout(Duration::from_secs(60)).expect("batch") {
        FeedEvent::Changes { batch, rows, .. } => (batch, rows),
        FeedEvent::Failed(e) => panic!("feed: {e}"),
        FeedEvent::Loaded { .. } => panic!("a one-line edit reloaded the whole snapshot"),
        FeedEvent::Unindexed { reason } => panic!("feed: {reason}"),
    };
    let latency_ms = t1.elapsed().as_secs_f64() * 1000.0;
    let t2 = Instant::now();
    model.apply(&mut g, &batch).expect("apply");
    chunks.refresh(&model, &mut g, true);
    g.upload(&gpu.device, &gpu.queue).expect("upload");
    let apply_ms = t2.elapsed().as_secs_f64() * 1000.0;
    EditResult {
        reindex_ms,
        log_rows: report.change_log_rows as usize,
        feed_rows,
        touched: batch.nodes.len()
            + batch.edges.len()
            + batch.removed_nodes.len()
            + batch.removed_edges.len(),
        latency_ms,
        apply_ms,
    }
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    let mut add =
        |name: String, store: &GraphStore, dir: Option<tempfile::TempDir>, editable: bool| {
            let read_ms = median_ms(
                || {
                    let t0 = Instant::now();
                    std::hint::black_box(read_snapshot(store).expect("snapshot"));
                    t0.elapsed()
                },
                5,
            );
            let snapshot = read_snapshot(store).expect("snapshot");
            out.push(Case {
                name,
                snapshot,
                read_ms,
                _dir: dir,
                editable,
            });
        };

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repo_state = tempfile::tempdir().expect("tempdir");
    let store = index_into(&repo, repo_state.path());
    add("repo".into(), &store, Some(repo_state), false);

    for n in [10_000, 100_000] {
        let dir = synth_project(n);
        let ProjectIndex::Ready(store) = open_existing(dir.path()).expect("open") else {
            panic!("synthetic tree was indexed");
        };
        add(format!("synth-{}k", n / 1000), &store, Some(dir), true);
    }

    if let Some(p) = kgraf_core::legacy::env_var("KGRAF_BENCH_PROJECT") {
        match open_existing(Path::new(&p)).expect("open") {
            ProjectIndex::Ready(store) => add(format!("project:{p}"), &store, None, false),
            other => panic!("KGRAF_BENCH_PROJECT={p}: {other:?}"),
        }
    }
    out
}

fn benches(c: &mut Criterion) {
    let gpu = gpu();
    let target = RenderTarget::new(&gpu.device, W, H).expect("target");
    let mut failures = Vec::new();
    let mut report = Vec::new();

    for case in cases() {
        let n = case.snapshot.nodes.len();
        let build_ms = median_ms(
            || {
                let t0 = Instant::now();
                std::hint::black_box(ProjectModel::build(&case.snapshot).expect("build"));
                t0.elapsed()
            },
            7,
        );
        let (mut model, scene): (ProjectModel, Scene) =
            ProjectModel::build(&case.snapshot).expect("build");
        let edges = scene.live_edge_count();
        c.bench_with_input(BenchmarkId::new("scene_build", &case.name), &n, |b, _| {
            b.iter(|| std::hint::black_box(ProjectModel::build(&case.snapshot).expect("build")));
        });

        // Folded (as the view draws it) and unfolded (the renderer's worst case), on
        // the GPU and the CPU budget: [folded gpu, folded cpu, unfolded gpu, unfolded cpu].
        let mut frame_ms = [0.0f64; 4];
        let mut drawn = (0usize, 0usize);
        for (k, (budget, fold_budget)) in [
            (RenderBudget::Gpu, kgraf_galaxy::fold::VISIBLE_BUDGET),
            (RenderBudget::Cpu, kgraf_galaxy::fold::VISIBLE_BUDGET),
            (RenderBudget::Gpu, usize::MAX),
            (RenderBudget::Cpu, usize::MAX),
        ]
        .into_iter()
        .enumerate()
        {
            let mut g = Galaxy::new(&gpu.device, scene.clone(), budget);
            let mut chunks = Chunks::new(fold_budget);
            chunks.refresh(&model, &mut g, false);
            let folded = if k < 2 { "" } else { "_unfolded" };
            if k == 0 {
                drawn = (
                    g.scene().live_node_count(),
                    g.scene().live_node_count() - g.folded_count(),
                );
            }
            frame(&mut g, &gpu, &target);
            // Orbit a little every frame so each frame is a real redraw.
            let step = |g: &mut Galaxy| {
                g.camera_mut().rotate(2.0, 0.0, H as f32);
                g.update(1.0 / 60.0);
                frame(g, &gpu, &target);
            };
            c.bench_with_input(
                BenchmarkId::new(format!("frame_{budget:?}{folded}"), &case.name),
                &n,
                |b, _| b.iter(|| step(&mut g)),
            );
            frame_ms[k] = median_ms(
                || {
                    let t0 = Instant::now();
                    step(&mut g);
                    t0.elapsed()
                },
                60,
            );
        }

        // The fold: a whole recompute (a reload's cost), and one user expansion and
        // fold of the node hiding the most, each uploaded, on a rendered galaxy.
        let mut g = Galaxy::new(&gpu.device, scene.clone(), RenderBudget::Gpu);
        let mut chunks = Chunks::default();
        chunks.refresh(&model, &mut g, false);
        frame(&mut g, &gpu, &target);
        let refold_ms = median_ms(
            || {
                let t0 = Instant::now();
                chunks.forget_cut();
                chunks.refresh(&model, &mut g, false);
                g.upload(&gpu.device, &gpu.queue).expect("upload");
                gpu.queue.submit([]);
                t0.elapsed()
            },
            7,
        );
        let biggest = (0..g.scene().nodes().len() as u32)
            .max_by_key(|&s| (chunks.fold().hidden_below(s), std::cmp::Reverse(s)))
            .filter(|&s| chunks.fold().hidden_below(s) > 0);
        let toggle_ms = biggest.map_or(0.0, |s| {
            let revealed = chunks.fold().hidden_below(s);
            let ms = median_ms(
                || {
                    let t0 = Instant::now();
                    chunks.toggle(&model, &mut g, Toggle::Expand(s));
                    g.upload(&gpu.device, &gpu.queue).expect("upload");
                    gpu.queue.submit([]);
                    let t_open = t0.elapsed();
                    g.update(1.0);
                    g.upload(&gpu.device, &gpu.queue).expect("upload");
                    gpu.queue.submit([]);
                    let t1 = Instant::now();
                    chunks.toggle(&model, &mut g, Toggle::Collapse(s));
                    g.upload(&gpu.device, &gpu.queue).expect("upload");
                    gpu.queue.submit([]);
                    let t_close = t1.elapsed();
                    g.update(1.0);
                    g.upload(&gpu.device, &gpu.queue).expect("upload");
                    gpu.queue.submit([]);
                    t_open.max(t_close)
                },
                7,
            );
            eprintln!(
                "{}: toggling slot {s} ({revealed} hidden below it) {ms:.3} ms",
                case.name
            );
            ms
        });

        // One live batch in, one out, on a rendered GPU galaxy: apply, refold, upload
        // in place, and finish the tweens (which also upload in place).
        let (insert, remove) = change_batches(&case.snapshot);
        let mut g = Galaxy::new(&gpu.device, scene.clone(), RenderBudget::Gpu);
        let mut chunks = Chunks::default();
        chunks.refresh(&model, &mut g, false);
        frame(&mut g, &gpu, &target);
        let full_before = g.upload_stats().full;
        // Every upload is followed by a submit, as the next frame's render follows it in
        // the app. Without one, wgpu keeps every staged write pending: measured
        // 2026-09-27, a batch's upload grew from 1.5 to 16 ms over 400 cycles with no
        // submit and stayed flat with one, so an unsubmitted loop measures the pile-up.
        let upload = |g: &mut Galaxy| {
            g.upload(&gpu.device, &gpu.queue).expect("upload");
            gpu.queue.submit([]);
        };
        let mut cycle = |g: &mut Galaxy, model: &mut ProjectModel| {
            let t0 = Instant::now();
            model.apply(g, &insert).expect("insert");
            chunks.refresh(model, g, true);
            upload(g);
            let t_in = t0.elapsed();
            g.update(1.0);
            upload(g);
            let t1 = Instant::now();
            model.apply(g, &remove).expect("remove");
            chunks.refresh(model, g, true);
            upload(g);
            let t_out = t1.elapsed();
            g.update(1.0);
            upload(g);
            t_in.max(t_out)
        };
        c.bench_with_input(
            BenchmarkId::new("change_batch_in_out", &case.name),
            &n,
            |b, _| {
                b.iter(|| cycle(&mut g, &mut model));
            },
        );
        let batch_ms = median_ms(|| cycle(&mut g, &mut model), 31);
        let stats = g.upload_stats();
        // Where one insert's time goes (median of 15), for the report.
        let mut phases: Vec<[f64; 3]> = (0..15)
            .map(|_| {
                let t0 = Instant::now();
                model.apply(&mut g, &insert).expect("insert");
                let a = t0.elapsed().as_secs_f64() * 1000.0;
                let t1 = Instant::now();
                chunks.refresh(&model, &mut g, true);
                let r = t1.elapsed().as_secs_f64() * 1000.0;
                let t2 = Instant::now();
                upload(&mut g);
                let u = t2.elapsed().as_secs_f64() * 1000.0;
                g.update(1.0);
                upload(&mut g);
                model.apply(&mut g, &remove).expect("remove");
                chunks.refresh(&model, &mut g, true);
                upload(&mut g);
                g.update(1.0);
                upload(&mut g);
                [a, r, u]
            })
            .collect();
        phases.sort_by(|x, y| (x[0] + x[1] + x[2]).total_cmp(&(y[0] + y[1] + y[2])));
        let [pa, pr, pu] = phases[phases.len() / 2];
        eprintln!(
            "{}: one insert batch: apply {pa:.3} ms, refold {pr:.3} ms, upload and submit {pu:.3} ms",
            case.name
        );
        assert_eq!(
            stats.full, full_before,
            "{}: live batches must upload in place, not the whole scene",
            case.name
        );

        let line = format!(
            "{}: {n} nodes, {edges} edges | snapshot read {:.1} ms | scene build {build_ms:.1} ms | \
             drawn {} of {} scene nodes (fold budget {}) | refold {refold_ms:.2} ms, toggle {toggle_ms:.2} ms | \
             frame gpu {:.2} ms, cpu budget {:.2} ms (unfolded {:.2}, {:.2}) | change batch (21 nodes, 40 edges) {batch_ms:.3} ms, \
             last upload {} bytes",
            case.name,
            case.read_ms,
            drawn.1,
            drawn.0,
            kgraf_galaxy::fold::VISIBLE_BUDGET,
            frame_ms[0],
            frame_ms[1],
            frame_ms[2],
            frame_ms[3],
            stats.last_bytes
        );
        eprintln!("{line}");
        report.push(line);

        // A one-line edit through the real indexer and the real feed, on a tree the
        // bench owns (never this repository's files).
        if let Some(root) = case
            ._dir
            .as_ref()
            .map(|d| d.path())
            .filter(|_| case.editable)
        {
            let edit = one_line_edit(root, &gpu, &target);
            let line = format!(
                "{}: one-line edit | reindex {:.0} ms, {} change-log rows | feed delivers {} \
                 rows touching {} ids, {:.0} ms after the reindex | apply and upload {:.3} ms",
                case.name,
                edit.reindex_ms,
                edit.log_rows,
                edit.feed_rows,
                edit.touched,
                edit.latency_ms,
                edit.apply_ms
            );
            eprintln!("{line}");
            report.push(line);
            if edit.log_rows > EDIT_MAX_LOG_ROWS {
                failures.push(format!(
                    "{}: a one-line edit appended {} change-log rows (budget {EDIT_MAX_LOG_ROWS})",
                    case.name, edit.log_rows
                ));
            }
            if edit.apply_ms > EDIT_MAX_APPLY_MS {
                failures.push(format!(
                    "{}: applying a one-line edit took {:.2} ms (budget {EDIT_MAX_APPLY_MS})",
                    case.name, edit.apply_ms
                ));
            }
        }

        for (what, got) in [("refold", refold_ms), ("fold toggle", toggle_ms)] {
            if got > FOLD_MAX_MS {
                failures.push(format!(
                    "{}: {what} {got:.2} ms over the {FOLD_MAX_MS} ms budget",
                    case.name
                ));
            }
        }
        if let Some(b) = BUDGETS
            .iter()
            .find(|b| n >= b.nodes * 9 / 10 && n <= b.nodes * 3 / 2)
        {
            for (what, got, max) in [
                ("snapshot read", case.read_ms, b.snapshot_read_ms),
                ("scene build", build_ms, b.scene_build_ms),
                ("frame (gpu budget)", frame_ms[0], b.frame_gpu_ms),
                ("frame (cpu budget)", frame_ms[1], b.frame_cpu_ms),
                ("unfolded frame (gpu budget)", frame_ms[2], b.frame_gpu_ms),
                ("unfolded frame (cpu budget)", frame_ms[3], b.frame_cpu_ms),
                ("change batch", batch_ms, b.change_batch_ms),
            ] {
                if got > max {
                    failures.push(format!(
                        "{}: {what} {got:.2} ms over the {max} ms budget",
                        case.name
                    ));
                }
            }
        }
    }
    eprintln!("\nsummary:");
    for line in &report {
        eprintln!("  {line}");
    }
    assert!(
        failures.is_empty(),
        "budgets missed:\n{}",
        failures.join("\n")
    );
}

criterion_group! {
    name = project_benches;
    config = Criterion::default().sample_size(10).measurement_time(Duration::from_secs(3));
    targets = benches
}
criterion_main!(project_benches);
