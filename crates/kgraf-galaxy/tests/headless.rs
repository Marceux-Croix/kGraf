//! Headless GPU tests: render real frames into a texture, read the pixels back,
//! and check the effect each mechanism claims, not its settings.

use glam::Vec3;
use kgraf_galaxy::budget::CPU_NODE_CAP;
use kgraf_galaxy::gpu::{BACKGROUND_SRGB, RenderTarget};
use kgraf_galaxy::{
    EdgeInput, Galaxy, GalaxyError, NodeInput, RenderBudget, Scene, SceneInput, demo,
};

const W: u32 = 256;
const H: u32 = 256;

/// Picks an adapter for testing, preferring a software rasterizer when one exists so CI
/// machines without a GPU run the same test. Adapted from rerun re_renderer
/// `select_testing_adapter` (crates/viewer_support/re_renderer/src/device_caps.rs,
/// commit f54fa0212aa1ff8595ea60fe841fd385821c1644, MIT OR Apache-2.0, Copyright (c)
/// 2022 Rerun Technologies AB). Fails loudly when there is no adapter: a
/// skipped GPU test would read as a passing one.
fn gpu() -> (wgpu::Device, wgpu::Queue, wgpu::AdapterInfo) {
    gpu_with(|_| {})
}

/// [`gpu`] with the device's limits adjusted by `limits` (to force a limit low).
fn gpu_with(
    limits: impl FnOnce(&mut wgpu::Limits),
) -> (wgpu::Device, wgpu::Queue, wgpu::AdapterInfo) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
    });
    let mut adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    assert!(!adapters.is_empty(), "no graphics adapter found");
    adapters.sort_by_key(|a| match a.get_info().device_type {
        wgpu::DeviceType::Cpu => 0,
        wgpu::DeviceType::DiscreteGpu => 1,
        wgpu::DeviceType::Other
        | wgpu::DeviceType::IntegratedGpu
        | wgpu::DeviceType::VirtualGpu => 2,
    });
    let adapter = adapters.remove(0);
    let info = adapter.get_info();
    let mut required_limits = wgpu::Limits::default().using_resolution(adapter.limits());
    limits(&mut required_limits);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits,
        ..Default::default()
    }))
    .expect("device");
    (device, queue, info)
}

fn scene() -> Scene {
    let node = |x: f32, y: f32| NodeInput {
        position: [x, y, 0.0],
        color: [1.0, 1.0, 1.0],
        // Drawn core radius 12 (size times the 0.5 node scale).
        size: 24.0,
    };
    Scene::new(SceneInput {
        // 0 and 1 joined by an edge through the origin; 2 alone above.
        nodes: vec![node(-100.0, 0.0), node(100.0, 0.0), node(0.0, 100.0)],
        edges: vec![EdgeInput {
            source: 0,
            target: 1,
            color: [1.0, 1.0, 1.0],
            intensity: 0.25,
        }],
    })
    .expect("valid scene")
}

fn render(g: &mut Galaxy, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<u8> {
    let target = RenderTarget::new(device, W, H).expect("target");
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    g.render(device, queue, &mut encoder, &target)
        .expect("render");
    queue.submit([encoder.finish()]);
    target.read_pixels(device, queue).expect("readback")
}

/// Pixels that differ from the background.
fn lit_pixels(img: &[u8]) -> usize {
    img.chunks_exact(4)
        .filter(|p| p[..3] != BACKGROUND_SRGB)
        .count()
}

fn rgb_at(img: &[u8], p: [f32; 2]) -> [u8; 3] {
    let i = ((p[1] as u32 * W + p[0] as u32) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]]
}

fn node(position: [f32; 3], color: [f32; 3], size: f32) -> NodeInput {
    NodeInput {
        position,
        color,
        size,
    }
}

/// Brightest channel sum in the 3x3 block around a pixel.
fn brightness_near(img: &[u8], p: [f32; 2]) -> u32 {
    let (cx, cy) = (p[0] as i64, p[1] as i64);
    let mut best = 0;
    for y in cy - 1..=cy + 1 {
        for x in cx - 1..=cx + 1 {
            if x < 0 || y < 0 || x >= i64::from(W) || y >= i64::from(H) {
                continue;
            }
            let i = ((y as u32 * W + x as u32) * 4) as usize;
            let sum = u32::from(img[i]) + u32::from(img[i + 1]) + u32::from(img[i + 2]);
            best = best.max(sum);
        }
    }
    best
}

/// The color of the brightest pixel near `p`.
fn color_near(img: &[u8], p: [f32; 2]) -> [u8; 3] {
    let (cx, cy) = (p[0] as i64, p[1] as i64);
    let mut best = ([0u8; 3], 0u32);
    for y in cy - 1..=cy + 1 {
        for x in cx - 1..=cx + 1 {
            if x < 0 || y < 0 || x >= i64::from(W) || y >= i64::from(H) {
                continue;
            }
            let i = ((y as u32 * W + x as u32) * 4) as usize;
            let c = [img[i], img[i + 1], img[i + 2]];
            let sum = c.iter().map(|&v| u32::from(v)).sum();
            if sum > best.1 {
                best = (c, sum);
            }
        }
    }
    best.0
}

fn background() -> u32 {
    BACKGROUND_SRGB.iter().map(|&c| u32::from(c)).sum()
}

fn px(g: &Galaxy, world: [f32; 3]) -> [f32; 2] {
    g.camera()
        .frame(W as f32, H as f32)
        .project(Vec3::from(world))
        .expect("in front of the camera")
        .0
}

#[test]
fn nodes_are_drawn_where_they_project_and_nowhere_else() {
    let (device, queue, info) = gpu();
    eprintln!("adapter: {} ({:?})", info.name, info.device_type);
    let mut g = Galaxy::new(&device, scene(), RenderBudget::Gpu);
    let img = render(&mut g, &device, &queue);
    let bg = background();
    for world in [[-100.0, 0.0, 0.0], [100.0, 0.0, 0.0], [0.0, 100.0, 0.0]] {
        let b = brightness_near(&img, px(&g, world));
        assert!(b > 600, "node at {world:?} should be near white, got {b}");
    }
    // Corners are empty space: exactly the background.
    for corner in [[2.0, 2.0], [253.0, 2.0], [2.0, 253.0], [253.0, 253.0]] {
        assert_eq!(brightness_near(&img, corner), bg, "corner {corner:?}");
    }
}

#[test]
fn edges_follow_the_budget_policy() {
    let (device, queue, _) = gpu();
    let bg = background();
    let mid = |g: &Galaxy| px(g, [0.0, 0.0, 0.0]);

    // GPU budget: edges drawn with nothing selected.
    let mut g = Galaxy::new(&device, scene(), RenderBudget::Gpu);
    let img = render(&mut g, &device, &queue);
    let lit = brightness_near(&img, mid(&g));
    assert!(
        lit > bg + 150,
        "GPU: edge midpoint should be lit, got {lit}"
    );

    // CPU budget: no edges without a selection.
    let mut g = Galaxy::new(&device, scene(), RenderBudget::Cpu);
    let img = render(&mut g, &device, &queue);
    assert_eq!(
        brightness_near(&img, mid(&g)),
        bg,
        "CPU: no edge unselected"
    );
    assert_eq!(g.drawn_edge_vertices(), 0);

    // CPU budget with an endpoint selected: the edge is one of the selection's
    // relations, drawn in the selection's blue at 55% (STG-DRAW-4). Rendered before the
    // camera flies, so the midpoint is still where it was.
    g.select(Some(0));
    let img = render(&mut g, &device, &queue);
    let sel = brightness_near(&img, mid(&g));
    assert!(sel > bg + 100, "CPU: the selected edge is drawn ({sel})");
    let [r, _, b] = color_near(&img, mid(&g));
    assert!(
        b > r + 40,
        "CPU: the relation is blue: {:?}",
        color_near(&img, mid(&g))
    );
}

#[test]
fn selection_dims_nodes_outside_the_neighborhood() {
    let (device, queue, _) = gpu();
    let mut g = Galaxy::new(&device, scene(), RenderBudget::Gpu);
    let before = render(&mut g, &device, &queue);
    let p_lone = px(&g, [0.0, 100.0, 0.0]);
    let p_sel = px(&g, [-100.0, 0.0, 0.0]);
    g.select(Some(0));
    let after = render(&mut g, &device, &queue);
    let (lone_before, lone_after) = (
        brightness_near(&before, p_lone),
        brightness_near(&after, p_lone),
    );
    assert!(
        lone_after * 2 < lone_before,
        "node 2 is outside node 0's neighborhood: {lone_before} -> {lone_after}"
    );
    assert_eq!(
        brightness_near(&after, p_sel),
        brightness_near(&before, p_sel),
        "the selected node keeps its brightness"
    );
}

/// A search result (0032) lights the matched set in place and dims the rest, on
/// both budgets, and the camera flies to the set: nodes 0 and 2 match, 1 does not,
/// though it is 0's neighbor.
#[test]
fn a_set_selection_lights_the_matches_and_dims_even_their_neighbors() {
    let (device, queue, _) = gpu();
    for budget in [RenderBudget::Gpu, RenderBudget::Cpu] {
        let mut g = Galaxy::new(&device, scene(), budget);
        let before = render(&mut g, &device, &queue);
        let (p0, p1, p2) = (
            px(&g, [-100.0, 0.0, 0.0]),
            px(&g, [100.0, 0.0, 0.0]),
            px(&g, [0.0, 100.0, 0.0]),
        );
        let target_before = g.camera().target();
        g.select_set(&[2, 0]);
        let sel = g.selection().expect("selected");
        assert_eq!(sel.selected(), 2, "the first match is primary");
        assert_eq!(sel.members(), &[2, 0]);
        // Render before the camera moves, to compare the same pixels.
        let after = render(&mut g, &device, &queue);
        assert!(
            brightness_near(&after, p1) * 2 < brightness_near(&before, p1),
            "{budget:?}: the neighbor that did not match recedes"
        );
        // A match turns the selection's blue (STG-DRAW-2, COL-4); before, it was not.
        for p in [p0, p2] {
            let [r0, _, b0] = color_near(&before, p).map(u32::from);
            let [r1, _, b1] = color_near(&after, p).map(u32::from);
            assert!(
                b1 > r1 + 40,
                "{budget:?}: a match is blue: {:?}",
                color_near(&after, p)
            );
            assert!(b0 <= r0 + 10, "{budget:?}: not blue before");
        }
        let mut steps = 0;
        while g.update(1.0 / 60.0) {
            steps += 1;
            assert!(steps < 2_000, "camera never settled");
        }
        let centroid = Vec3::new(-50.0, 50.0, 0.0);
        assert!(
            g.camera().target().distance(centroid) < target_before.distance(centroid),
            "{budget:?}: the camera flew toward the matches"
        );
        g.select_set(&[]);
        assert!(
            g.selection().is_none(),
            "an empty result clears the selection"
        );
    }
}

/// Labels (ported from the reference's `NodeLabels`): work nodes and agents first,
/// then the largest code nodes, capped; with a selection, only what it lights.
#[test]
fn labels_go_to_overlays_first_then_the_largest_stars_within_the_cap() {
    use kgraf_galaxy::labels::{MAX_LABELS, choose};
    let (device, _queue, _) = gpu();
    let nodes: Vec<NodeInput> = (0..200)
        .map(|i| node([i as f32 * 5.0, 0.0, 0.0], [1.0; 3], 1.0 + i as f32 * 0.1))
        .collect();
    let scene = Scene::new(SceneInput {
        nodes,
        edges: vec![],
    })
    .expect("scene");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    let overlay = [3u32, 7];
    let mut by_size: Vec<u32> = (0..200).collect();
    by_size.reverse();
    let (over, code) = choose(&g, &overlay, by_size.iter().copied(), MAX_LABELS);
    assert_eq!(over, [7, 3], "overlays first, larger first");
    assert_eq!(over.len() + code.len(), MAX_LABELS, "capped at 80 in all");
    assert_eq!(code[0], 199, "then the largest code node");
    // With a selection, only highlighted nodes carry labels.
    g.select_set(&[5, 3]);
    let (over, code) = choose(&g, &overlay, by_size.iter().copied(), MAX_LABELS);
    assert_eq!(over, [3]);
    assert_eq!(code, [5, 3]);
    assert_eq!(RenderBudget::Cpu.label_cap(), MAX_LABELS / 2);
}

#[test]
fn idle_galaxy_renders_no_frames_until_something_changes() {
    let (device, queue, _) = gpu();
    let mut g = Galaxy::new(&device, scene(), RenderBudget::Gpu);
    assert!(g.needs_render(W, H), "first frame");
    render(&mut g, &device, &queue);
    for _ in 0..100 {
        assert!(!g.update(1.0 / 60.0), "no motion without input");
        assert!(!g.needs_render(W, H), "nothing changed, nothing to draw");
    }
    assert_eq!(g.frames_rendered(), 1);

    g.camera_mut().rotate(30.0, 0.0, H as f32);
    assert!(g.update(1.0 / 60.0));
    assert!(g.needs_render(W, H), "camera moved");
    render(&mut g, &device, &queue);

    // Selecting flies the camera; frames are needed until it settles, then none.
    g.select(Some(2));
    let mut steps = 0;
    while g.update(1.0 / 60.0) {
        steps += 1;
        assert!(steps < 2_000, "camera never settled");
    }
    assert!(g.needs_render(W, H));
    render(&mut g, &device, &queue);
    assert!(!g.update(1.0 / 60.0) && !g.needs_render(W, H));
    assert!(g.needs_render(W + 1, H), "a resize is a change");
}

#[test]
fn clicking_a_rendered_node_selects_it() {
    let (device, queue, _) = gpu();
    let mut g = Galaxy::new(&device, scene(), RenderBudget::Gpu);
    let img = render(&mut g, &device, &queue);
    let p = px(&g, [100.0, 0.0, 0.0]);
    assert!(brightness_near(&img, p) > 600, "node 1 is drawn at {p:?}");
    assert_eq!(g.pick(p, W, H), Some(1));
    assert_eq!(g.pick([2.0, 2.0], W, H), None);
}

#[test]
fn a_near_node_covers_a_far_one_whatever_the_index_order() {
    let (device, queue, _) = gpu();
    // Index 0 is near (z = 200, red), index 1 far (z = -200, blue), both on the view
    // axis. Without a depth buffer the far node, drawn second, covered the near one.
    let scene = Scene::new(SceneInput {
        nodes: vec![
            node([0.0, 0.0, 200.0], [1.0, 0.0, 0.0], 30.0),
            node([0.0, 0.0, -200.0], [0.0, 0.0, 1.0], 60.0),
        ],
        edges: vec![],
    })
    .expect("scene");
    for budget in [RenderBudget::Gpu, RenderBudget::Cpu] {
        let mut g = Galaxy::new(&device, scene.clone(), budget);
        let img = render(&mut g, &device, &queue);
        let center = px(&g, [0.0, 0.0, 0.0]);
        let [r, _, b] = rgb_at(&img, center);
        assert!(
            r > 100 && b < 30,
            "{budget:?}: near red node must win, got r {r} b {b}"
        );
    }
}

#[test]
fn edges_pass_behind_nearer_cores_and_in_front_of_farther_ones() {
    let (device, queue, _) = gpu();
    // A dark node core at the origin and one edge through the view axis, either
    // behind the core (z = -300) or in front of it (z = 300).
    let run = |z: f32| {
        let white = [1.0, 1.0, 1.0];
        let scene = Scene::new(SceneInput {
            nodes: vec![
                node([0.0, 0.0, 0.0], [0.05, 0.05, 0.05], 80.0),
                node([-300.0, 0.0, z], white, 4.0),
                node([300.0, 0.0, z], white, 4.0),
            ],
            edges: vec![EdgeInput {
                source: 1,
                target: 2,
                color: white,
                intensity: 1.0,
            }],
        })
        .expect("scene");
        let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
        let img = render(&mut g, &device, &queue);
        brightness_near(&img, px(&g, [0.0, 0.0, 0.0]))
    };
    let behind = run(-300.0);
    let in_front = run(300.0);
    assert!(behind < 100, "edge behind the core is hidden: {behind}");
    // Without depth testing the core, drawn after the edges, covered this one too.
    assert!(
        in_front > 400,
        "edge in front of the core is drawn: {in_front}"
    );
}

#[test]
fn a_scene_beyond_max_buffer_size_is_a_typed_error_not_a_panic() {
    // Above the 256x256 readback (256 KiB), below a 20,000-node instance buffer.
    const MAX: u64 = 512 * 1024;
    let (device, queue, _) = gpu_with(|l| {
        l.max_buffer_size = MAX;
        l.max_uniform_buffer_binding_size = l.max_uniform_buffer_binding_size.min(MAX);
        l.max_storage_buffer_binding_size = l.max_storage_buffer_binding_size.min(MAX);
    });
    assert_eq!(device.limits().max_buffer_size, MAX, "limit applied");

    // 20,000 instances are 640,000 bytes: refused with a typed error.
    let big = Scene::new(demo::synthetic(20_000, 7)).expect("scene");
    let mut g = Galaxy::new(&device, big, RenderBudget::Gpu);
    let target = RenderTarget::new(&device, W, H).expect("target");
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    match g.render(&device, &queue, &mut encoder, &target) {
        Err(GalaxyError::SceneTooLarge { bytes, max, .. }) => {
            assert!(bytes > max && max == MAX, "{bytes} {max}");
        }
        other => panic!("expected SceneTooLarge, got {other:?}"),
    }

    // 12,000 instances are 384,000 bytes: under the limit, but 1.5x headroom is not.
    // The headroom must be clamped, and the frame must draw.
    let nodes = (0..12_000)
        .map(|i| {
            let (x, y) = ((i % 120) as f32, (i / 120) as f32);
            node([x * 8.0 - 480.0, y * 8.0 - 400.0, 0.0], [1.0; 3], 4.0)
        })
        .collect();
    let fits = Scene::new(SceneInput {
        nodes,
        edges: vec![],
    })
    .expect("scene");
    let mut g = Galaxy::new(&device, fits, RenderBudget::Gpu);
    let img = render(&mut g, &device, &queue);
    assert!(lit_pixels(&img) > 1_000);
    let (inst, _) = g.buffer_capacity();
    assert!((384_000..=MAX).contains(&inst), "instance buffer {inst}");
}

#[test]
fn buffers_shrink_when_the_scene_becomes_much_smaller() {
    let (device, queue, _) = gpu();
    let mut g = Galaxy::new(
        &device,
        Scene::new(demo::synthetic(20_000, 7)).expect("scene"),
        RenderBudget::Gpu,
    );
    render(&mut g, &device, &queue);
    let (big_inst, big_edges) = g.buffer_capacity();
    g.set_scene(scene());
    render(&mut g, &device, &queue);
    let (inst, edges) = g.buffer_capacity();
    assert!(
        inst < big_inst / 4 && edges < big_edges / 4,
        "{big_inst}->{inst}, {big_edges}->{edges}"
    );
}

#[test]
fn cpu_budget_caps_individual_sprites_and_groups_the_rest() {
    let (device, queue, _) = gpu();
    let scene = Scene::new(demo::synthetic(30_000, 7)).expect("scene");
    let mut g = Galaxy::new(&device, scene.clone(), RenderBudget::Cpu);
    let img = render(&mut g, &device, &queue);
    let groups = g.visibility().groups().len();
    assert_eq!(g.visibility().shown_count(), CPU_NODE_CAP);
    assert!(groups > 0 && groups <= 4_096, "{groups}");
    assert_eq!(g.drawn_sprites() as usize, CPU_NODE_CAP + groups);
    assert!(lit_pixels(&img) > 1_000, "the capped galaxy still draws");

    // A hidden node that gets selected is drawn individually with its neighbors.
    let hidden = (0..30_000u32)
        .find(|&i| !g.visibility().is_shown(i))
        .expect("some hidden");
    g.select(Some(hidden));
    render(&mut g, &device, &queue);
    let members = g.selection().expect("selected").members().len();
    let extra = g
        .selection()
        .expect("selected")
        .members()
        .iter()
        .filter(|&&m| !g.visibility().is_shown(m))
        .count();
    assert!(extra >= 1 && extra <= members);
    assert_eq!(g.drawn_sprites() as usize, CPU_NODE_CAP + groups + extra);

    // The GPU budget draws every node.
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    render(&mut g, &device, &queue);
    assert_eq!(g.drawn_sprites(), 30_000);
}

#[test]
fn a_close_zoom_cannot_fill_the_screen_with_one_sprite() {
    let (device, queue, _) = gpu();
    // One huge node right in front of the camera: unclamped it covers the target.
    let scene = Scene::new(SceneInput {
        nodes: vec![node([0.0, 0.0, 0.0], [1.0; 3], 5_000.0)],
        edges: vec![],
    })
    .expect("scene");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Cpu);
    let img = render(&mut g, &device, &queue);
    // The documented CPU clamp, stated here rather than read back from the code under
    // test, so a changed or removed clamp fails this test.
    let max_px = 48.0_f32;
    assert_eq!(RenderBudget::Cpu.max_core_px(), max_px);
    // Disc of radius max_px plus a feather pixel.
    let bound = (std::f32::consts::PI * (max_px + 2.0) * (max_px + 2.0)) as usize;
    let lit = lit_pixels(&img);
    assert!(lit > 0 && lit <= bound, "{lit} lit pixels, bound {bound}");
}

/// The palette's stage background reaches the frame (it is a render setting, not a
/// constant): the default is the style guide's `#07080F`, and a new one repaints.
#[test]
fn the_stage_background_comes_from_the_palette() {
    let (device, queue, _) = gpu();
    let mut g = Galaxy::new(&device, scene(), RenderBudget::Gpu);
    let corner = |img: &[u8]| [img[0], img[1], img[2]];
    let img = render(&mut g, &device, &queue);
    assert_eq!(corner(&img), BACKGROUND_SRGB);
    assert_eq!(BACKGROUND_SRGB, [0x07, 0x08, 0x0f]);
    let p = kgraf_galaxy::Palette::default();
    g.set_stage([1.0, 0.0, 0.0], p.card);
    let img = render(&mut g, &device, &queue);
    assert_eq!(corner(&img), [255, 0, 0]);
}
