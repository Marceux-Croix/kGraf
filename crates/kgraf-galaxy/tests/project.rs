//! The galaxy fed from a real project (the mechanism, not the setting). Each
//! test indexes a temporary tree with the real indexer, reads it through the real
//! snapshot queries, tails the real change log on the real feed thread, and renders
//! on a real adapter.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use glam::Vec3;
use kgraf_galaxy::Palette;
use kgraf_galaxy::feed::{Feed, FeedEvent, FeedOptions};
use kgraf_galaxy::galaxy::TWEEN_SECONDS;
use kgraf_galaxy::gpu::{BACKGROUND_SRGB, RenderTarget};
use kgraf_galaxy::project::LIVE_EDGE;
use kgraf_galaxy::selection::OVERLAY_EDGE;
use kgraf_galaxy::{Galaxy, NodeInput, Population, ProjectModel, RenderBudget, Scene, SceneInput};
use kgraf_graph::layout::density::{CROSS_CLUSTER_EDGE, SAME_CLUSTER_EDGE};
use kgraf_graph::layout::stellar::node_size;
use kgraf_graph::snapshot::{
    GraphSnapshot, ProjectIndex, Resolved, STATE_DIR, graph_db_path, now_ms, open_existing,
    read_snapshot,
};
use kgraf_graph::{GraphStore, Indexer, LeaseRegistry, NodeKind};

const W: u32 = 256;
const H: u32 = 256;
const WAIT: Duration = Duration::from_secs(20);

fn write(root: &Path, rel: &str, body: &str) {
    let full = root.join(rel);
    fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    fs::write(full, body).expect("write");
}

/// A small tree in three clusters: an app with a call chain, a library calling into
/// the app across clusters, and a lone tool.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(
        root,
        "crates/app/src/main.rs",
        "pub fn entry_point() { helper_one(); runner(); }\npub fn runner() { helper_one(); }\n",
    );
    write(
        root,
        "crates/app/src/util.rs",
        "pub fn helper_one() { deep_leaf(); }\npub fn deep_leaf() {}\n",
    );
    write(
        root,
        "crates/lib/src/lib.rs",
        "pub fn api_call() { helper_one(); }\n",
    );
    write(root, "tools/lone/x.rs", "pub fn lone_tool() {}\n");
    dir
}

fn index(root: &Path) -> GraphStore {
    fs::create_dir_all(root.join(STATE_DIR)).expect("state dir");
    let store = GraphStore::open(&graph_db_path(root), root).expect("store");
    Indexer::new(&store).index().expect("index");
    store
}

fn snapshot(root: &Path) -> GraphSnapshot {
    let ProjectIndex::Ready(store) = open_existing(root).expect("open") else {
        panic!("the tree was indexed");
    };
    read_snapshot(&store).expect("snapshot")
}

fn gpu() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
    });
    let mut adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    assert!(!adapters.is_empty(), "no graphics adapter found");
    // A software rasterizer first where one exists, as in tests/headless.rs.
    adapters.sort_by_key(|a| match a.get_info().device_type {
        wgpu::DeviceType::Cpu => 0,
        wgpu::DeviceType::DiscreteGpu => 1,
        _ => 2,
    });
    let adapter = adapters.remove(0);
    let required_limits = wgpu::Limits::default().using_resolution(adapter.limits());
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits,
        ..Default::default()
    }))
    .expect("device")
}

fn render(g: &mut Galaxy, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<u8> {
    let target = RenderTarget::new(device, W, H).expect("target");
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    g.render(device, queue, &mut encoder, &target)
        .expect("render");
    queue.submit([encoder.finish()]);
    target.read_pixels(device, queue).expect("readback")
}

fn brightness_near(img: &[u8], p: [f32; 2]) -> u32 {
    let (cx, cy) = (p[0] as i64, p[1] as i64);
    let mut best = 0;
    for y in cy - 1..=cy + 1 {
        for x in cx - 1..=cx + 1 {
            if x < 0 || y < 0 || x >= i64::from(W) || y >= i64::from(H) {
                continue;
            }
            let i = ((y as u32 * W + x as u32) * 4) as usize;
            best = best.max(u32::from(img[i]) + u32::from(img[i + 1]) + u32::from(img[i + 2]));
        }
    }
    best
}

fn px(g: &Galaxy, world: [f32; 3]) -> [f32; 2] {
    g.camera()
        .frame(W as f32, H as f32)
        .project(Vec3::from(world))
        .expect("in front of the camera")
        .0
}

fn fast_feed(root: &Path) -> Feed {
    Feed::spawn(
        root.to_path_buf(),
        FeedOptions {
            poll: Duration::from_millis(20),
            recheck: Duration::from_millis(20),
            ..FeedOptions::default()
        },
        || {},
    )
    .expect("feed")
}

fn next_loaded(feed: &Feed) -> GraphSnapshot {
    loop {
        match feed.recv_timeout(WAIT).expect("a feed event in time") {
            FeedEvent::Loaded { snapshot, .. } => return *snapshot,
            FeedEvent::Failed(e) => panic!("feed failed: {e}"),
            _ => {}
        }
    }
}

/// The next change batch satisfying `want`, merging nothing: batches are resolved to
/// current state, so the latest one that matches is the one to apply. Earlier batches
/// are returned too, so the caller applies every one in order.
fn batches_until(feed: &Feed, want: impl Fn(&Resolved) -> bool) -> Vec<Resolved> {
    let mut out = Vec::new();
    loop {
        match feed.recv_timeout(WAIT).expect("a change batch in time") {
            FeedEvent::Changes { batch, .. } => {
                let done = want(&batch);
                out.push(*batch);
                if done {
                    return out;
                }
            }
            FeedEvent::Failed(e) => panic!("feed failed: {e}"),
            other => panic!("unexpected feed event: {other:?}"),
        }
    }
}

#[test]
fn the_scene_is_the_indexed_graph_with_cached_positions_and_stellar_looks() {
    let dir = repo();
    let store = index(dir.path());
    let snap = snapshot(dir.path());
    let (model, scene) = ProjectModel::build(&snap).expect("build");

    assert_eq!(
        model.code_node_count() as u64,
        store.node_count().expect("n")
    );
    // Plus one proxy per directory (decision 0068), single-child chains merged:
    // crates, crates/app/src, crates/lib/src and tools/lone.
    assert_eq!(model.proxy_count(), 4);
    assert_eq!(
        scene.live_node_count(),
        model.code_node_count() + model.proxy_count()
    );
    assert_eq!(
        scene.live_edge_count() as u64,
        store.edge_count().expect("e")
    );

    // Degree counted independently from the snapshot's edges.
    let mut degree: HashMap<&str, u32> = HashMap::new();
    for e in &snap.edges {
        *degree.entry(e.src.as_str()).or_default() += 1;
        *degree.entry(e.dst.as_str()).or_default() += 1;
    }
    for n in &snap.nodes {
        let slot = model.slot_of(&n.id).expect("every node has a slot");
        let drawn = scene.nodes()[slot as usize];
        let cached = store.position(&n.id).expect("pos").expect("cached");
        assert_eq!(drawn.position, [cached.x, cached.y, cached.z], "{}", n.id);
        let d = degree.get(n.id.as_str()).copied().unwrap_or(0);
        // Gray dots (STG-DRAW-2): only the size follows the degree.
        assert_eq!(drawn.color, Palette::default().code_dot, "{}", n.id);
        assert_eq!(drawn.size, node_size(n.kind, d), "{}", n.id);
        assert_eq!(scene.population(slot), Population::Code);
    }

    // The library's call into the app crosses clusters; the app's own call does not.
    let id = |name: &str| store.resolve_symbol(name).expect("resolve")[0].id.clone();
    let intensity = |a: &str, b: &str| {
        let (sa, sb) = (
            model.slot_of(&id(a)).expect("a"),
            model.slot_of(&id(b)).expect("b"),
        );
        scene
            .edges()
            .iter()
            .find(|e| (e.source, e.target) == (sa, sb))
            .map(|e| e.intensity)
            .expect("edge drawn")
    };
    assert_eq!(intensity("api_call", "helper_one"), CROSS_CLUSTER_EDGE);
    assert_eq!(intensity("helper_one", "deep_leaf"), SAME_CLUSTER_EDGE);
}

#[test]
fn an_unindexed_directory_is_reported_and_loads_once_indexed() {
    let dir = repo();
    let feed = fast_feed(dir.path());
    match feed.recv_timeout(WAIT).expect("event") {
        FeedEvent::Unindexed { reason } => {
            assert!(reason.contains("not indexed"), "{reason}");
        }
        other => panic!("expected Unindexed, got {other:?}"),
    }
    assert!(
        !dir.path().join(STATE_DIR).exists(),
        "opening a directory must not create an index"
    );
    let store = index(dir.path());
    // The feed may read its snapshot while the indexer is still committing files
    // (one transaction per file); the tail then carries the rest.
    let (mut model, mut scene) = ProjectModel::build(&next_loaded(&feed)).expect("build");
    let nodes = store.node_count().expect("n") as usize;
    let edges = store.edge_count().expect("e") as usize;
    while model.code_node_count() != nodes || scene.live_edge_count() != edges {
        match feed.recv_timeout(WAIT).expect("the tail catches up") {
            FeedEvent::Changes { batch, .. } => {
                model.apply(&mut scene, &batch).expect("apply");
            }
            FeedEvent::Loaded { snapshot, .. } => {
                (model, scene) = ProjectModel::build(&snapshot).expect("build");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn an_insert_through_the_change_log_is_applied_in_place_and_animated() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    let snap = next_loaded(&feed);
    let (mut model, scene) = ProjectModel::build(&snap).expect("build");
    let (device, queue) = gpu();
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    let before = render(&mut g, &device, &queue);
    let full_before = g.upload_stats().full;
    let instances_before: Vec<[f32; 3]> = (0..g.scene().nodes().len() as u32)
        .map(|i| g.shown_position(i).expect("slot"))
        .collect();
    let edges_before = g.scene().live_edge_count();

    // A new file in a new directory, calling nothing: nothing else may move (0015).
    write(
        dir.path(),
        "extra/fresh/new.rs",
        "pub fn brand_new_fn() {}\n",
    );
    Indexer::new(&store).index().expect("reindex");
    let fresh = store.resolve_symbol("brand_new_fn").expect("resolve")[0]
        .id
        .clone();
    let batches = batches_until(&feed, |b| b.nodes.iter().any(|n| n.id == fresh));
    let mut inserted = 0;
    for b in &batches {
        inserted += model.apply(&mut g, b).expect("apply").inserted;
    }
    assert_eq!(inserted, 2, "the file node and its function");
    let slot = model.slot_of(&fresh).expect("the new node has a slot");
    assert_eq!(g.scene().population(slot), Population::Code);
    assert!(
        g.scene().live_edge_count() > edges_before,
        "its contains edge"
    );

    // In place: the change rewrote only its own slots.
    render(&mut g, &device, &queue);
    let stats = g.upload_stats();
    assert_eq!(stats.full, full_before, "no whole-scene upload: {stats:?}");
    assert!(stats.partial >= 1, "{stats:?}");
    let full_bytes = (g.scene().nodes().len() * 32 + g.scene().edges().len() * 56) as u64;
    assert!(
        stats.last_bytes < full_bytes,
        "{} bytes written of {full_bytes}",
        stats.last_bytes
    );
    // Nodes elsewhere did not move, bit for bit.
    for (i, p) in instances_before.iter().enumerate() {
        assert_eq!(g.shown_position(i as u32), Some(*p), "slot {i} moved");
    }

    // Animated: it appears from nothing, so a frame is needed until the tween ends,
    // then none.
    assert!(g.is_animating());
    let target_pos = g.scene().nodes()[slot as usize].position;
    let mut frames = 0;
    while g.update(1.0 / 60.0) {
        render(&mut g, &device, &queue);
        frames += 1;
        assert!(frames < 600, "the animation never ended");
    }
    let expected = (TWEEN_SECONDS * 60.0) as i32;
    assert!(
        (frames - expected).abs() <= 2,
        "{frames} frames for a {TWEEN_SECONDS}s tween"
    );
    let after = render(&mut g, &device, &queue);
    assert!(
        !g.update(1.0 / 60.0) && !g.needs_render(W, H),
        "settled: nothing to draw"
    );
    let p = px(&g, target_pos);
    assert!(
        brightness_near(&after, p) > brightness_near(&before, p),
        "the new node is drawn at its cached position {target_pos:?}"
    );
}

#[test]
fn a_call_depth_change_moves_nodes_with_an_animation() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    let (mut model, scene) = ProjectModel::build(&next_loaded(&feed)).expect("build");
    let (device, queue) = gpu();
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    render(&mut g, &device, &queue);
    let entry = store.resolve_symbol("entry_point").expect("r")[0]
        .id
        .clone();
    let slot = model.slot_of(&entry).expect("slot");
    let old = g.scene().nodes()[slot as usize].position;

    // A new caller of the entry point sinks it and its tree one layer (0058, case 2).
    write(
        dir.path(),
        "tests/smoke.rs",
        "pub fn smoke_test() { entry_point(); }\n",
    );
    Indexer::new(&store).index().expect("reindex");
    let new = store.position(&entry).expect("pos").expect("cached");
    let new = [new.x, new.y, new.z];
    assert_ne!(old, new, "the layout moved the entry point");
    let batches = batches_until(&feed, |b| b.nodes.iter().any(|n| n.id == entry));
    let mut moved = 0;
    for b in &batches {
        moved += model.apply(&mut g, b).expect("apply").moved;
    }
    // entry_point and runner sink; helper_one keeps depth 1 through api_call, and
    // deep_leaf depth 2, so neither moves (local perturbation, 0015).
    assert_eq!(moved, 2, "entry_point and runner sink one layer");
    let helper = store.resolve_symbol("helper_one").expect("r")[0].id.clone();
    let helper_slot = model.slot_of(&helper).expect("slot");
    assert_eq!(
        g.shown_position(helper_slot),
        Some(g.scene().nodes()[helper_slot as usize].position),
        "helper_one is not animating"
    );
    assert_eq!(g.scene().nodes()[slot as usize].position, new, "target set");
    assert_eq!(g.shown_position(slot), Some(old), "drawn where it was");
    g.update(TWEEN_SECONDS / 2.0);
    let mid = g.shown_position(slot).expect("slot");
    assert!(mid[2] < old[2] && mid[2] > new[2], "halfway: {mid:?}");
    g.update(TWEEN_SECONDS);
    assert_eq!(g.shown_position(slot), Some(new));
    render(&mut g, &device, &queue);
    assert!(!g.is_animating() && !g.needs_render(W, H));
}

#[test]
fn an_idle_feed_sends_nothing_and_a_reload_request_sends_a_snapshot() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    let first = next_loaded(&feed);
    // Idle: many polls, no messages (so no wake-ups and no frames).
    assert!(feed.recv_timeout(Duration::from_millis(300)).is_none());
    feed.request_reload();
    let again = next_loaded(&feed);
    assert_eq!(again.nodes.len(), first.nodes.len());
    assert_eq!(again.watermark, store.change_watermark().expect("w"));
}

#[test]
fn a_removed_file_leaves_the_scene() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    let (mut model, mut scene) = ProjectModel::build(&next_loaded(&feed)).expect("build");
    let lone = store.resolve_symbol("lone_tool").expect("r")[0].id.clone();
    let slot = model.slot_of(&lone).expect("slot");
    fs::remove_file(dir.path().join("tools/lone/x.rs")).expect("rm");
    Indexer::new(&store).index().expect("reindex");
    for b in batches_until(&feed, |b| b.removed_nodes.contains(&lone)) {
        model.apply(&mut scene, &b).expect("apply");
    }
    assert!(model.slot_of(&lone).is_none());
    assert!(!scene.is_alive(slot));
    assert_eq!(
        model.code_node_count() as u64,
        store.node_count().expect("n")
    );
    // The lone tool's directory emptied, so its proxy left with it.
    assert_eq!(model.proxy_count(), 3);
    assert_eq!(
        scene.live_node_count(),
        model.code_node_count() + model.proxy_count()
    );
    assert_eq!(
        scene.live_edge_count() as u64,
        store.edge_count().expect("e")
    );
}

#[test]
fn an_agent_is_its_own_population_and_selecting_it_highlights_its_live_edges() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    let (mut model, scene) = ProjectModel::build(&next_loaded(&feed)).expect("build");
    let (device, queue) = gpu();
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);

    // A worker leases a function and a whole file, through the real registry.
    let helper = store.resolve_symbol("helper_one").expect("r")[0].id.clone();
    let reg = LeaseRegistry::new(&store);
    let got = reg
        .acquire_node_set(
            "worker-7",
            "task-42",
            std::slice::from_ref(&helper),
            now_ms(),
        )
        .expect("node lease");
    assert!(matches!(got, kgraf_graph::NodeAcquisition::Granted(_)));
    let got = reg
        .acquire_all(
            "worker-7",
            "task-42",
            &["crates/lib/src/lib.rs".to_owned()],
            &[None],
            now_ms(),
        )
        .expect("file lease");
    assert!(matches!(got, kgraf_graph::Acquisition::Granted(_)));
    let lib_file = store
        .nodes_in_file("crates/lib/src/lib.rs")
        .expect("nodes")
        .into_iter()
        .find(|n| n.kind == NodeKind::File)
        .expect("file node")
        .id;

    // Wait for both leases, not for the first activity: the feed polls every 20 ms
    // while the two leases are taken one after the other, so a poll between them
    // delivers an activity set with only the node lease. That is correct (the next
    // batch brings the file lease), but a test that stopped there asserted edges that
    // did not exist yet; it failed once in a loaded workspace run (2026-09-27), and
    // every time with a 300 ms pause between the two acquires.
    let both = |a: &[kgraf_graph::snapshot::Activity]| {
        let holds = |pred: &dyn Fn(&kgraf_graph::snapshot::Activity) -> bool| {
            a.iter()
                .any(|r| r.kind == kgraf_graph::snapshot::ActivityKind::Holds && pred(r))
        };
        holds(&|r| r.node_id.as_deref() == Some(helper.as_str()))
            && holds(&|r| r.node_id.as_deref() == Some(lib_file.as_str()))
    };
    for b in batches_until(&feed, |b| b.activity.as_deref().is_some_and(both)) {
        model.apply(&mut g, &b).expect("apply");
    }
    assert_eq!(model.overlay_counts(), (1, 1), "one agent, one work item");
    let agent = model.agent_slot("worker-7").expect("agent slot");
    let task = model.task_slot("task-42").expect("task slot");
    assert_eq!(g.scene().population(agent), Population::Agent);
    assert_eq!(g.scene().population(task), Population::Work);
    // Its own channel: the actor disk, not the code dots' gray (STG-DRAW-7).
    let p = Palette::default();
    let a = g.scene().nodes()[agent as usize];
    assert_eq!(a.color, p.agent);
    assert_eq!(g.scene().nodes()[task as usize].color, p.state_neutral);
    assert_ne!(
        a.color, p.code_dot,
        "agents must not reuse the code encoding"
    );
    // A code node's degree ignores agent edges.
    let helper_slot = model.slot_of(&helper).expect("slot");
    let code_degree = g
        .scene()
        .neighbors(helper_slot)
        .filter(|&n| g.scene().population(n) == Population::Code)
        .count() as u32;
    assert_eq!(
        g.scene().nodes()[helper_slot as usize].size,
        node_size(NodeKind::Function, code_degree),
        "its size counts code edges only"
    );

    // Its live edges: the leased node, the leased file's node, and its task.
    let lib_slot = model.slot_of(&lib_file).expect("lib file slot");
    let mut live: Vec<u32> = g.scene().neighbors(agent).collect();
    live.sort_unstable();
    let mut want = vec![helper_slot, lib_slot, task];
    want.sort_unstable();
    assert_eq!(live, want);
    let held = g
        .scene()
        .incident_edges(agent)
        .iter()
        .map(|&e| g.scene().edges()[e as usize])
        .find(|e| e.target == helper_slot || e.source == helper_slot)
        .expect("held edge");
    assert_eq!(held.intensity, LIVE_EDGE);

    // Selecting the agent highlights exactly those, dims the rest, and draws its
    // edges at 0.85 (select an agent, see its live edges).
    for _ in 0..200 {
        g.update(1.0 / 60.0);
    }
    render(&mut g, &device, &queue);
    let lone = model
        .slot_of(&store.resolve_symbol("lone_tool").expect("r")[0].id)
        .expect("lone");
    g.select(Some(agent));
    for _ in 0..600 {
        if !g.update(1.0 / 60.0) {
            break;
        }
    }
    let sel = g.selection().expect("selected").clone();
    let mut members = sel.members().to_vec();
    members.sort_unstable();
    let mut expect_members = vec![agent, helper_slot, lib_slot, task];
    expect_members.sort_unstable();
    assert_eq!(members, expect_members);
    let img = render(&mut g, &device, &queue);
    // The midpoint of the agent's edge to the held function is lit by that edge.
    let a_pos = Vec3::from(g.scene().nodes()[agent as usize].position);
    let h_pos = Vec3::from(g.scene().nodes()[helper_slot as usize].position);
    let mid = px(&g, ((a_pos + h_pos) * 0.5).to_array());
    let bg: u32 = BACKGROUND_SRGB.iter().map(|&c| u32::from(c)).sum();
    assert!(
        brightness_near(&img, mid) > bg + 60,
        "the agent's live edge is drawn at {OVERLAY_EDGE}: {} vs background {bg}",
        brightness_near(&img, mid)
    );
    // An unrelated node is outside the highlight, so it draws dimmed (color 0.15).
    assert!(!sel.is_highlighted(lone));

    // Releasing everything removes the agent and its task (animated out).
    reg.release_node_all("worker-7").expect("release");
    reg.release_all("worker-7").expect("release");
    for b in batches_until(&feed, |b| b.activity.as_ref().is_some_and(|a| a.is_empty())) {
        model.apply(&mut g, &b).expect("apply");
    }
    assert_eq!(model.overlay_counts(), (0, 0));
    assert!(g.is_vanishing(agent) || !g.scene().is_alive(agent));
    assert!(g.selection().is_none() || !g.scene().is_alive(agent) || g.is_vanishing(agent));
    g.update(TWEEN_SECONDS * 2.0);
    render(&mut g, &device, &queue);
    assert!(
        !g.scene().is_alive(agent),
        "the slot is freed after the fade"
    );
    assert!(
        g.selection().is_none(),
        "a removed agent is no longer selected"
    );
}

#[test]
fn actors_draw_as_bordered_disks_work_as_cards_and_code_as_discs() {
    // One of each population, same size. The actor's border is lighter than its
    // center (STG-DRAW-7); a card is wider than tall (STG-DRAW-5), so a point at 0.9r
    // to the side is on it and one at 0.9r above is not.
    let (device, queue) = gpu();
    let node = |x: f32, color: [f32; 3]| NodeInput {
        position: [x, 0.0, 0.0],
        color,
        size: 120.0,
    };
    let scene = Scene::with_populations(
        SceneInput {
            nodes: vec![
                node(-300.0, [1.0; 3]),
                node(0.0, [0.3; 3]),
                node(300.0, [1.0; 3]),
            ],
            edges: vec![],
        },
        vec![Population::Code, Population::Agent, Population::Work],
    )
    .expect("scene");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Cpu);
    let img = render(&mut g, &device, &queue);
    let frame = g.camera().frame(W as f32, H as f32);
    let bg: u32 = BACKGROUND_SRGB.iter().map(|&c| u32::from(c)).sum();
    let at = |x: f32| {
        let (c, depth) = frame.project(Vec3::new(x, 0.0, 0.0)).expect("visible");
        (c, 60.0 / (depth * frame.pixel_world_size_per_depth))
    };
    let (c, r) = at(-300.0);
    assert!(
        brightness_near(&img, [c[0] + 0.6 * r, c[1] - 0.6 * r]) > bg + 100,
        "a disc"
    );
    let (c, r) = at(0.0);
    let center = brightness_near(&img, c);
    let border = brightness_near(&img, [c[0] + 0.93 * r, c[1]]);
    assert!(center > bg, "the actor's fill is drawn");
    assert!(
        border > center + 60,
        "the border is lighter: {border} vs {center}"
    );
    let (c, r) = at(300.0);
    assert!(
        brightness_near(&img, [c[0] + 0.9 * r, c[1]]) > bg + 100,
        "on the card's side"
    );
    assert_eq!(
        brightness_near(&img, [c[0], c[1] - 0.9 * r]),
        bg,
        "above the card"
    );
}

#[test]
fn the_real_scene_renders_headlessly() {
    let dir = repo();
    let _store = index(dir.path());
    let (_model, scene) = ProjectModel::build(&snapshot(dir.path())).expect("build");
    let (device, queue) = gpu();
    for budget in [RenderBudget::Gpu, RenderBudget::Cpu] {
        let mut g = Galaxy::new(&device, scene.clone(), budget);
        let img = render(&mut g, &device, &queue);
        let lit = img
            .chunks_exact(4)
            .filter(|p| p[..3] != BACKGROUND_SRGB)
            .count();
        assert!(
            lit >= scene.live_node_count(),
            "{budget:?}: the project is drawn ({lit} lit pixels)"
        );
        // Every live node projects to a lit pixel.
        for (i, n) in scene.nodes().iter().enumerate() {
            let p = px(&g, n.position);
            if p[0] < 1.0 || p[1] < 1.0 || p[0] > (W - 2) as f32 || p[1] > (H - 2) as f32 {
                continue;
            }
            let bg: u32 = BACKGROUND_SRGB.iter().map(|&c| u32::from(c)).sum();
            assert!(
                brightness_near(&img, p) > bg,
                "{budget:?}: node {i} at {p:?} is not drawn"
            );
        }
    }
}

/// Receives until `want` accepts an event, failing on a feed error.
fn event_until(feed: &Feed, want: impl Fn(&FeedEvent) -> bool) -> FeedEvent {
    loop {
        let e = feed.recv_timeout(WAIT).expect("a feed event in time");
        if let FeedEvent::Failed(msg) = &e {
            panic!("feed failed: {msg}");
        }
        if want(&e) {
            return e;
        }
    }
}

fn is_data(e: &FeedEvent) -> bool {
    matches!(e, FeedEvent::Loaded { .. } | FeedEvent::Changes { .. })
}

#[test]
fn a_replaced_index_is_reopened_not_read_stale() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    let first = next_loaded(&feed);
    assert!(
        first
            .nodes
            .iter()
            .all(|n| !n.qualified_name.ends_with("replacement_fn"))
    );
    drop(store);

    // Remove the index and build a new one at the same path. The old connection
    // would keep reading the deleted file without an error.
    fs::remove_dir_all(dir.path().join(STATE_DIR)).expect("rm index");
    write(dir.path(), "extra/r.rs", "pub fn replacement_fn() {}\n");
    let store = index(dir.path());
    let reloaded = match event_until(&feed, |e| matches!(e, FeedEvent::Loaded { .. })) {
        FeedEvent::Loaded { snapshot, .. } => *snapshot,
        _ => unreachable!("filtered"),
    };
    assert!(
        reloaded
            .nodes
            .iter()
            .any(|n| n.qualified_name.ends_with("replacement_fn")),
        "the feed reads the new index"
    );
    assert_eq!(reloaded.nodes.len() as u64, store.node_count().expect("n"));
}

#[test]
fn a_one_line_edit_is_a_small_batch() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    next_loaded(&feed);
    let path = dir.path().join("crates/app/src/util.rs");
    let body = fs::read_to_string(&path).expect("read");
    fs::write(&path, format!("{body}// edited\n")).expect("write");
    let report = Indexer::new(&store).index().expect("reindex");
    assert_eq!(report.files_changed, 1);
    let (rows, touched) = match event_until(&feed, |e| matches!(e, FeedEvent::Changes { .. })) {
        FeedEvent::Changes { batch, rows, .. } => (rows, batch.nodes.len() + batch.edges.len()),
        _ => unreachable!("filtered"),
    };
    // The file's own nodes and edges, not every call edge in the tree.
    assert!(
        (rows as u64) <= report.change_log_rows && touched < 20,
        "{rows} rows, {touched} ids touched"
    );
}

#[test]
fn changes_are_held_while_an_index_pass_is_marked_running() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    next_loaded(&feed);
    store
        .set_meta(kgraf_graph::index::INDEX_IN_PROGRESS, &now_ms().to_string())
        .expect("marker");
    let helper = store.resolve_symbol("helper_one").expect("r")[0].id.clone();
    let got = LeaseRegistry::new(&store)
        .acquire_node_set("w", "t", std::slice::from_ref(&helper), now_ms())
        .expect("lease");
    assert!(matches!(got, kgraf_graph::NodeAcquisition::Granted(_)));
    assert!(
        feed.recv_timeout(Duration::from_millis(400)).is_none(),
        "nothing is sent mid-pass"
    );
    store
        .clear_meta(kgraf_graph::index::INDEX_IN_PROGRESS)
        .expect("clear");
    match event_until(&feed, |e| matches!(e, FeedEvent::Changes { .. })) {
        FeedEvent::Changes { batch, .. } => {
            assert_eq!(batch.activity.as_ref().map(Vec::len), Some(1));
        }
        _ => unreachable!("filtered"),
    }
}

#[test]
fn an_undrained_feed_holds_one_data_message_and_coalesces_the_rest() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    // Leave the first snapshot unreceived (an occluded window), and make changes.
    std::thread::sleep(Duration::from_millis(100));
    for i in 0..5 {
        write(
            dir.path(),
            &format!("more/m{i}/x.rs"),
            &format!("pub fn more_{i}() {{}}\n"),
        );
        Indexer::new(&store).index().expect("reindex");
        std::thread::sleep(Duration::from_millis(60));
    }
    // Only the snapshot is waiting: one data message, however many reindexes ran.
    // Checked before taking it. Taking it wakes the feed, which may send the coalesced
    // batch at once; the earlier check after the take raced that send (it failed about
    // 1 run in 30 beside the other tests under load, seeing the 20-row batch).
    let deadline = std::time::Instant::now() + WAIT;
    while feed.data_in_flight() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the snapshot never came"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Several polls later it is still the only one: the feed coalesces behind it.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(feed.data_in_flight(), 1, "one data message in flight");
    let first = feed.try_recv().expect("the snapshot");
    assert!(matches!(first, FeedEvent::Loaded { .. }), "{first:?}");
    // Once taken, the held changes arrive, coalesced.
    let (mut model, mut scene) = match first {
        FeedEvent::Loaded { snapshot, .. } => ProjectModel::build(&snapshot).expect("build"),
        _ => unreachable!("matched"),
    };
    let nodes = store.node_count().expect("n") as usize;
    let mut batches = 0;
    while model.code_node_count() != nodes {
        match event_until(&feed, is_data) {
            FeedEvent::Changes { batch, .. } => {
                model.apply(&mut scene, &batch).expect("apply");
                batches += 1;
            }
            FeedEvent::Loaded { snapshot, .. } => {
                (model, scene) = ProjectModel::build(&snapshot).expect("build");
            }
            _ => unreachable!("filtered"),
        }
    }
    assert!(
        batches <= 2,
        "five reindexes coalesced into {batches} batch(es)"
    );
}

#[test]
fn an_expired_lease_leaves_the_view_without_a_write() {
    let dir = repo();
    let store = index(dir.path());
    let feed = fast_feed(dir.path());
    let (mut model, mut scene) = ProjectModel::build(&next_loaded(&feed)).expect("build");
    let helper = store.resolve_symbol("helper_one").expect("r")[0].id.clone();
    let got = LeaseRegistry::new(&store)
        .with_ttl_ms(500)
        .acquire_node_set("brief", "t", std::slice::from_ref(&helper), now_ms())
        .expect("lease");
    assert!(matches!(got, kgraf_graph::NodeAcquisition::Granted(_)));
    for b in batches_until(&feed, |b| {
        b.activity.as_ref().is_some_and(|a| !a.is_empty())
    }) {
        model.apply(&mut scene, &b).expect("apply");
    }
    assert_eq!(model.overlay_counts(), (1, 1));
    let watermark = store.change_watermark().expect("w");
    // No write happens; the lease just lapses.
    for b in batches_until(&feed, |b| b.activity.as_ref().is_some_and(Vec::is_empty)) {
        model.apply(&mut scene, &b).expect("apply");
    }
    assert_eq!(model.overlay_counts(), (0, 0));
    assert_eq!(
        store.change_watermark().expect("w"),
        watermark,
        "expiry wrote nothing"
    );
}

#[test]
fn a_move_under_a_selection_is_written_in_place() {
    let dir = repo();
    let store = index(dir.path());
    let (model, scene) = ProjectModel::build(&snapshot(dir.path())).expect("build");
    let (device, queue) = gpu();
    let mut g = Galaxy::new(&device, scene.clone(), RenderBudget::Gpu);
    let helper = model
        .slot_of(&store.resolve_symbol("helper_one").expect("r")[0].id)
        .expect("slot");
    g.select(Some(helper));
    while g.update(1.0 / 60.0) {}
    render(&mut g, &device, &queue);
    let full = g.upload_stats().full;

    // Move a highlighted neighbor and animate it to its end.
    let neighbor = g.scene().neighbors(helper).next().expect("a neighbor");
    let mut to = g.scene().nodes()[neighbor as usize].position;
    to[2] -= 50.0;
    g.move_node(neighbor, to).expect("move");
    let mut frames = 0u64;
    while g.update(1.0 / 60.0) {
        render(&mut g, &device, &queue);
        frames += 1;
        assert!(frames < 600);
    }
    let moved = render(&mut g, &device, &queue);
    let stats = g.upload_stats();
    assert_eq!(
        stats.full, full,
        "no whole-scene upload while animating under a selection: {stats:?}"
    );
    assert!(stats.partial >= frames, "{stats:?}");
    let degree = g.scene().degree(neighbor) as u64;
    assert!(
        stats.last_bytes <= 32 + degree * 56,
        "one sprite and its edges: {stats:?}"
    );

    // The in-place result is exactly what a fresh upload of the same state draws.
    let mut fresh_scene = scene;
    let mut n = fresh_scene.nodes()[neighbor as usize];
    n.position = to;
    fresh_scene.set_node(neighbor, n).expect("set");
    let mut fresh = Galaxy::new(&device, fresh_scene, RenderBudget::Gpu);
    fresh.select(Some(helper));
    *fresh.camera_mut() = g.camera().clone();
    let expected = render(&mut fresh, &device, &queue);
    assert!(moved == expected, "partial writes match a full upload");
}

/// Every color the model gives the scene comes from its palette, and a new palette
/// recolors everything in place (the shell's run-time palette goes through here).
#[test]
fn a_new_palette_recolors_every_node_and_edge_in_place() {
    let dir = repo();
    let _store = index(dir.path());
    let snap = snapshot(dir.path());
    let (mut model, mut scene) = ProjectModel::build(&snap).expect("build");
    let p = Palette::default();
    let pop = |scene: &Scene, s: u32| scene.population(s);
    for s in 0..scene.nodes().len() as u32 {
        if scene.is_alive(s) && pop(&scene, s) == Population::Code {
            assert_eq!(scene.nodes()[s as usize].color, p.code_dot);
        }
    }
    let custom = Palette {
        code_dot: [0.1, 0.2, 0.3],
        edge: [0.4, 0.5, 0.6],
        proxy: [0.7, 0.8, 0.9],
        ..Palette::default()
    };
    let sizes: Vec<f32> = scene.nodes().iter().map(|n| n.size).collect();
    model.set_palette(&mut scene, custom).expect("recolor");
    let mut seen = (0, 0, 0);
    for s in 0..scene.nodes().len() as u32 {
        if !scene.is_alive(s) {
            continue;
        }
        let n = scene.nodes()[s as usize];
        assert_eq!(n.size, sizes[s as usize], "sizes do not change");
        match pop(&scene, s) {
            Population::Code => {
                assert_eq!(n.color, custom.code_dot);
                seen.0 += 1;
            }
            Population::Cluster => {
                assert_eq!(n.color, custom.proxy);
                seen.1 += 1;
            }
            _ => {}
        }
    }
    for e in 0..scene.edges().len() as u32 {
        if scene.is_edge_alive(e) {
            assert_eq!(scene.edges()[e as usize].color, custom.edge);
            seen.2 += 1;
        }
    }
    assert!(seen.0 > 0 && seen.1 > 0 && seen.2 > 0, "{seen:?}");
    assert_eq!(model.palette(), &custom);
}
