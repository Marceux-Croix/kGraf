//! Chunking (decision 0068) on large graphs, through the real model, the real galaxy
//! and the real picker (the mechanism, not the setting):
//! - the automatic cut draws at most the budget on 10,000- and 100,000-node
//!   hierarchies and uses it;
//! - with hysteresis, inserting or removing a function, a file or a directory moves
//!   the view by about that change, and a reload that renumbers every slot keeps it;
//! - a picked click expands a folded node, a click on a child picks the child (never
//!   its selected parent in front of it), and a proxy's selection lights its children;
//! - over the budget the fold says so; on the CPU budget a fold compacts the upload;
//! - notices count once on their work node, and a hidden node's carrier is its drawn
//!   ancestor or its CPU group.

use std::collections::{HashMap, HashSet};

use glam::Vec3;
use kgraf_galaxy::fold::{self, Carrier, MAX_FANOUT, NoticeKind, Toggle, VISIBLE_BUDGET};
use kgraf_galaxy::gpu::{BACKGROUND_SRGB, RenderTarget};
use kgraf_galaxy::view::Click;
use kgraf_galaxy::{
    BlockKind, Chunks, Galaxy, PlanNode, ProjectModel, RenderBudget, Scene, WorkState, demo,
};
use kgraf_graph::layout::Position;
use kgraf_graph::snapshot::{GraphSnapshot, Resolved, SceneEdge, SceneNode};
use kgraf_graph::{EdgeKind, NodeKind};

const W: u32 = 800;
const H: u32 = 600;

fn gpu() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
    });
    let mut adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    assert!(!adapters.is_empty(), "no graphics adapter found");
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

/// Members the fold covers: code nodes plus proxies.
fn members(model: &ProjectModel) -> usize {
    model.code_node_count() + model.proxy_count()
}

/// The automatic cut with no history, and its fold, with no user overrides.
fn cut(model: &ProjectModel, scene: &Scene, budget: usize) -> fold::Fold {
    let h = model.hierarchy();
    let alive = |s: u32| scene.is_alive(s);
    let auto = fold::auto_expand(h, alive, budget);
    fold::resolve(h, alive, &auto, &HashMap::new())
}

/// The drawn members, by fold key (slots are not stable across builds).
fn drawn_keys(model: &ProjectModel, f: &fold::Fold, scene: &Scene) -> HashSet<String> {
    (0..scene.nodes().len() as u32)
        .filter(|&s| scene.is_alive(s) && model.hierarchy().is_member(s) && f.is_visible(s))
        .filter_map(|s| model.fold_key(s))
        .collect()
}

#[test]
fn the_automatic_cut_fits_the_budget_on_large_graphs_and_uses_it() {
    for n in [10_500, 100_000] {
        let snap = demo::synthetic_project(n, 7);
        let (model, scene) = ProjectModel::build(&snap).expect("build");
        let f = cut(&model, &scene, VISIBLE_BUDGET);
        let total = members(&model);
        eprintln!(
            "synthetic {n}: {} code nodes, {} proxies; before: {total} drawn; after: {} drawn, {} folded",
            model.code_node_count(),
            model.proxy_count(),
            f.visible_count(),
            f.hidden_count()
        );
        assert!(n <= model.code_node_count() && model.code_node_count() < n + 64);
        assert!(
            f.visible_count() <= VISIBLE_BUDGET,
            "{n}: {}",
            f.visible_count()
        );
        assert_eq!(
            f.visible_count() + f.hidden_count(),
            total,
            "every member placed"
        );
        let h = model.hierarchy();
        for s in 0..h.len() as u32 {
            assert!(h.children(s).len() <= MAX_FANOUT, "{n}: slot {s}");
        }
        // Tight: a drawn, folded parent stayed folded because its children did not fit.
        for s in 0..scene.nodes().len() as u32 {
            if f.hidden_below(s) > 0 {
                let c = h.children(s).len();
                assert!(
                    f.visible_count() + c > VISIBLE_BUDGET,
                    "{n}: slot {s} with {c} children would still have fit"
                );
            }
        }
        assert!(f.visible_count() > VISIBLE_BUDGET / 2, "the budget is used");
    }
}

/// A new file of `fns` functions at `path`, and the batch that removes it.
fn new_file(path: &str, fns: usize, tag: &str) -> (Resolved, Resolved) {
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let node = |id: String, kind| SceneNode {
        position: kgraf_graph::layout::position_for(dir, &id),
        id,
        path: path.into(),
        kind,
        qualified_name: format!("{tag}_q"),
    };
    let file = format!("{tag}-file");
    let mut nodes = vec![node(file.clone(), NodeKind::File)];
    let mut edges = Vec::new();
    for j in 0..fns {
        let id = format!("{tag}-fn{j}");
        nodes.push(node(id.clone(), NodeKind::Function));
        edges.push(SceneEdge {
            id: format!("{tag}-c{j}"),
            src: file.clone(),
            dst: id,
            kind: EdgeKind::Contains,
        });
    }
    let remove = Resolved {
        removed_nodes: nodes.iter().map(|n| n.id.clone()).collect(),
        removed_edges: edges.iter().map(|e| e.id.clone()).collect(),
        ..Resolved::default()
    };
    (
        Resolved {
            nodes,
            edges,
            ..Resolved::default()
        },
        remove,
    )
}

/// One more function in an existing file, and its removal.
fn one_more_function(snap: &GraphSnapshot, file: &str, k: usize) -> (Resolved, Resolved) {
    let path = snap
        .nodes
        .iter()
        .find(|n| n.id == file)
        .map(|n| n.path.clone())
        .expect("file");
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    let id = format!("extra-{k}");
    let node = SceneNode {
        id: id.clone(),
        path,
        kind: NodeKind::Function,
        qualified_name: format!("extra_{k}"),
        position: kgraf_graph::layout::position_for(&dir, &id),
    };
    let edge = SceneEdge {
        id: format!("extra-c-{k}"),
        src: file.to_owned(),
        dst: id.clone(),
        kind: EdgeKind::Contains,
    };
    (
        Resolved {
            nodes: vec![node],
            edges: vec![edge.clone()],
            ..Resolved::default()
        },
        Resolved {
            removed_nodes: vec![id],
            removed_edges: vec![edge.id],
            ..Resolved::default()
        },
    )
}

/// The drawn keys of a live, folded galaxy.
fn live_keys(model: &ProjectModel, g: &Galaxy, chunks: &Chunks) -> HashSet<String> {
    drawn_keys(model, chunks.fold(), g.scene())
}

/// How the drawn set moved from `a` to `b`, leaving out keys that exist on one side
/// only because the change itself added or removed them: (lost, gained).
fn churn(a: &HashSet<String>, b: &HashSet<String>, own: &HashSet<String>) -> (usize, usize) {
    let lost = a.difference(b).filter(|k| !own.contains(*k)).count();
    let gained = b.difference(a).filter(|k| !own.contains(*k)).count();
    (lost, gained)
}

/// With the cut kept by key and grown or shrunk with hysteresis, a function, a file or
/// a directory coming and going moves the view by about the change itself: at most one
/// parent's children (the fan-out) plus a few, not a level.
#[test]
fn functions_files_and_directories_come_and_go_with_little_churn() {
    let (device, _q) = gpu();
    let snap = demo::synthetic_project(10_500, 11);
    let (mut model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    let base = live_keys(&model, &g, &chunks);
    let bound = MAX_FANOUT + 8;
    let files: Vec<String> = snap
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .map(|n| n.id.clone())
        .collect();
    let mut worst = 0usize;
    // A change may take drawn nodes off the screen (a removal); the budget they free
    // is used again, so gains are allowed up to that. Nothing else may move by more
    // than one parent's children.
    let mut cycle = |label: &str, insert: &Resolved, remove: &Resolved, worst: &mut usize| {
        let proxies = |m: &ProjectModel| -> HashSet<String> {
            m.proxies().map(|(_, p)| p.key.clone()).collect()
        };
        let mut step = |batch: &Resolved, what: &str| {
            let before = live_keys(&model, &g, &chunks);
            let p0 = proxies(&model);
            model.apply(&mut g, batch).expect("apply");
            chunks.refresh(&model, &mut g, true);
            assert!(chunks.fold().visible_count() <= VISIBLE_BUDGET, "{label}");
            let after = live_keys(&model, &g, &chunks);
            let p1 = proxies(&model);
            let own: HashSet<String> = batch
                .nodes
                .iter()
                .map(|n| n.id.clone())
                .chain(batch.removed_nodes.iter().cloned())
                .chain(p0.symmetric_difference(&p1).cloned())
                .collect();
            let freed = before.iter().filter(|k| own.contains(*k)).count();
            let (lost, gained) = churn(&before, &after, &own);
            eprintln!("{label} ({what}): lost {lost}, gained {gained}, freed {freed}");
            assert!(
                lost <= bound,
                "{label} ({what}): lost {lost} (bound {bound})"
            );
            assert!(
                gained <= bound + freed,
                "{label} ({what}): gained {gained} (bound {bound} + freed {freed})"
            );
            lost.max(gained.saturating_sub(freed))
        };
        let a = step(insert, "in");
        let b = step(remove, "out");
        *worst = (*worst).max(a).max(b);
    };
    for k in 0..30 {
        let file = &files[(k * 37) % files.len()];
        let (i, r) = one_more_function(&snap, file, k);
        cycle(&format!("function {k}"), &i, &r, &mut worst);
    }
    // A file in an existing directory, and in a new directory (a new proxy).
    let dir = snap.nodes[0]
        .path
        .rsplit_once('/')
        .map_or("", |(d, _)| d)
        .to_owned();
    let (i, r) = new_file(&format!("{dir}/zz_new.rs"), 12, "nf");
    cycle("file in an existing directory", &i, &r, &mut worst);
    let (i, r) = new_file("brand/new/dir/zz.rs", 12, "nd");
    cycle("file in a new directory", &i, &r, &mut worst);
    // A whole directory leaving and coming back.
    let gone: Vec<&SceneNode> = snap
        .nodes
        .iter()
        .filter(|n| n.path.starts_with(&dir))
        .collect();
    let remove = Resolved {
        removed_nodes: gone.iter().map(|n| n.id.clone()).collect(),
        ..Resolved::default()
    };
    let back = Resolved {
        nodes: gone.iter().map(|n| (*n).clone()).collect(),
        edges: snap
            .edges
            .iter()
            .filter(|e| {
                let ids: HashSet<&str> = gone.iter().map(|n| n.id.as_str()).collect();
                ids.contains(e.src.as_str()) && ids.contains(e.dst.as_str())
            })
            .cloned()
            .collect(),
        ..Resolved::default()
    };
    cycle("a directory leaves and returns", &remove, &back, &mut worst);
    let end = live_keys(&model, &g, &chunks);
    eprintln!(
        "worst churn {worst} of {} drawn; {} keys differ from the start after all changes",
        base.len(),
        base.symmetric_difference(&end).count()
    );
}

/// The automatic cut is kept by key, so a reload that renumbers every slot draws the
/// same nodes.
#[test]
fn the_automatic_cut_survives_a_reload_that_renumbers_every_slot() {
    let (device, _q) = gpu();
    let snap = demo::synthetic_project(10_500, 5);
    let (model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    let before = live_keys(&model, &g, &chunks);
    let mut rev = snap.clone();
    rev.nodes.reverse();
    let (model2, scene2) = ProjectModel::build(&rev).expect("build");
    g.set_scene(scene2);
    chunks.forget_cut();
    chunks.refresh(&model2, &mut g, false);
    let after = live_keys(&model2, &g, &chunks);
    assert_eq!(before, after);
}

/// The screen point of slot `s` as drawn now, and its depth.
fn screen(g: &Galaxy, s: u32) -> Option<([f32; 2], f32)> {
    let frame = g.camera().frame(W as f32, H as f32);
    frame.project(Vec3::from(g.shown_position(s)?))
}

/// The drawn core radius of `s` on screen, in pixels.
fn drawn_px(g: &Galaxy, s: u32) -> f32 {
    let frame = g.camera().frame(W as f32, H as f32);
    let (_, d) = screen(g, s).expect("on screen");
    kgraf_galaxy::pick::core_px(
        g.drawn_size(s).expect("drawn"),
        d,
        frame.pixel_world_size_per_depth,
        g.budget().max_core_px(),
    )
}

/// A point where the picker returns `s`: its center, else on a ring's band.
fn pick_point(g: &Galaxy, s: u32) -> Option<[f32; 2]> {
    let (c, _) = screen(g, s)?;
    let r = drawn_px(g, s);
    [[0.0, 0.0], [0.8, 0.0], [-0.8, 0.0], [0.0, 0.8], [0.0, -0.8]]
        .into_iter()
        .map(|[dx, dy]| [c[0] + dx * r, c[1] + dy * r])
        .find(|&p| g.pick(p, W, H) == Some(s))
}

/// A click as the view makes it ([`kgraf_galaxy::GalaxyView`]): pick, note whether the
/// hit was already selected, select, and hand the click to the fold.
fn click_at(
    g: &mut Galaxy,
    model: &ProjectModel,
    chunks: &mut Chunks,
    at: [f32; 2],
) -> (Option<u32>, Option<Toggle>) {
    let hit = g.pick(at, W, H);
    let was_selected = hit.is_some() && g.selection().map(|s| s.selected()) == hit;
    g.select(hit);
    let t = chunks.click(
        model,
        g,
        Click {
            hit,
            was_selected,
            zone: None,
        },
    );
    (hit, t)
}

/// Lets tweens and the camera finish.
fn settle(g: &mut Galaxy) {
    for _ in 0..600 {
        if !g.update(1.0 / 30.0) {
            return;
        }
    }
    panic!("the galaxy never settled");
}

/// A measurement found 3 to 7.5% of clicks on a child returning its selected parent
/// (the parent won anywhere in its disc). Now a child in front of its parent is picked
/// wherever its core is under the cursor, and a proxy's selection lights all its
/// children (139 of 139 were dimmed before).
#[test]
fn a_click_on_a_child_picks_the_child_and_a_proxys_selection_lights_its_children() {
    let (device, _q) = gpu();
    let snap = demo::synthetic_project(10_500, 7);
    let (model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    g.frame_all();
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    // Folded files, and a few directory proxies folded by hand so they can be clicked
    // open.
    let dirs: Vec<u32> = model
        .proxies()
        .filter(|(s, p)| {
            p.kind == kgraf_galaxy::tree::ProxyKind::Directory
                && chunks.fold().is_expanded(*s)
                && model.hierarchy().children(*s).len() > 1
        })
        .map(|(s, _)| s)
        .take(6)
        .collect();
    let mut targets: Vec<u32> = (0..g.scene().nodes().len() as u32)
        .filter(|&s| chunks.fold().hidden_below(s) > 0 && g.is_drawn(s))
        .take(40)
        .collect();
    targets.extend(&dirs);
    let (mut expanded, mut children, mut swallowed, mut dimmed, mut proxies) = (0, 0, 0, 0, 0);
    let (mut in_hollow, mut hollow_swallowed) = (0, 0);
    for t in targets {
        if dirs.contains(&t) {
            chunks.toggle(&model, &mut g, Toggle::Collapse(t));
        }
        g.frame_all();
        settle(&mut g);
        // Where the picker finds the target: its center, or a ring's band (the hollow
        // of a proxy ring has no pick area).
        let Some(p) = pick_point(&g, t) else {
            continue;
        };
        let (_, tog) = click_at(&mut g, &model, &mut chunks, p);
        assert_eq!(tog, Some(Toggle::Expand(t)));
        settle(&mut g);
        expanded += 1;
        let is_proxy = model.proxy_at(t).is_some();
        proxies += usize::from(is_proxy);
        let sel = g.selection().expect("selected").clone();
        let (parent_at, parent_depth) = screen(&g, t).expect("on screen");
        let parent_r = drawn_px(&g, t);
        for &c in model.hierarchy().children(t) {
            children += 1;
            if is_proxy && !sel.is_highlighted(c) {
                dimmed += 1;
            }
            let Some((cp, cd)) = screen(&g, c) else {
                continue;
            };
            // A child nearer the eye than its parent, clicked at its center.
            if cd < parent_depth && g.pick(cp, W, H) == Some(t) {
                swallowed += 1;
            }
            // A child seen through a proxy ring's hollow (3 of 158 folded the
            // proxy when clicked there).
            let off = (cp[0] - parent_at[0]).hypot(cp[1] - parent_at[1]);
            if is_proxy && off < 0.6 * parent_r {
                in_hollow += 1;
                if g.pick(cp, W, H) == Some(t) {
                    hollow_swallowed += 1;
                }
            }
        }
        chunks.toggle(&model, &mut g, Toggle::Collapse(t));
        g.select(None);
        settle(&mut g);
    }
    eprintln!(
        "expanded {expanded} ({proxies} proxies): {children} children, {swallowed} swallowed, {dimmed} dimmed, {in_hollow} in a ring's hollow ({hollow_swallowed} picked the ring)"
    );
    assert_eq!(hollow_swallowed, 0, "a ring's hollow has no pick area");
    assert!(
        expanded - proxies >= 8 && proxies >= 3,
        "{expanded} {proxies}"
    );
    assert_eq!(
        swallowed, 0,
        "a nearer child is never swallowed by its parent"
    );
    assert_eq!(dimmed, 0, "a selected proxy lights every child");
}

#[test]
fn expanding_past_the_budget_is_reported_not_hidden() {
    let (device, _q) = gpu();
    let snap = demo::synthetic_project(2_000, 3);
    let (model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    let mut chunks = Chunks::new(150);
    chunks.refresh(&model, &mut g, false);
    assert_eq!(chunks.over_budget(), None);
    for _ in 0..10 {
        let Some(s) =
            (0..g.scene().nodes().len() as u32).find(|&s| chunks.fold().hidden_below(s) > 0)
        else {
            break;
        };
        chunks.toggle(&model, &mut g, Toggle::Expand(s));
    }
    let (drawn, budget) = chunks.over_budget().expect("over the budget");
    assert_eq!(budget, 150);
    assert_eq!(drawn, chunks.fold().visible_count());
    assert!(drawn > 150);
}

/// On the CPU budget a fold compacts the upload to what is drawn: before, a folded
/// 100,000-node scene still sent every hidden instance to the rasterizer.
#[test]
fn on_the_cpu_budget_a_fold_uploads_only_what_is_drawn() {
    let (device, queue) = gpu();
    let snap = demo::synthetic_project(10_500, 7);
    let (model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Cpu);
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    g.upload(&device, &queue).expect("upload");
    let drawn = chunks.fold().visible_count();
    assert!(g.folded_count() > 9_000);
    assert!(
        (g.drawn_sprites() as usize) <= drawn + 16,
        "{} sprites for {drawn} drawn",
        g.drawn_sprites()
    );
    // And a fold toggle animates and stays compact.
    let s = (0..g.scene().nodes().len() as u32)
        .find(|&s| chunks.fold().hidden_below(s) > 0)
        .expect("folded");
    chunks.toggle(&model, &mut g, Toggle::Expand(s));
    g.upload(&device, &queue).expect("upload");
    settle(&mut g);
    g.upload(&device, &queue).expect("upload");
    assert!((g.drawn_sprites() as usize) <= chunks.fold().visible_count() + 16);
}

#[test]
fn an_expansion_survives_a_reload_that_renumbers_every_slot() {
    let (device, _q) = gpu();
    let snap = demo::synthetic_project(2_000, 5);
    let (model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    let mut chunks = Chunks::new(150);
    chunks.refresh(&model, &mut g, false);
    let target = (0..g.scene().nodes().len() as u32)
        .find(|&s| chunks.fold().hidden_below(s) > 0 && model.node_id(s).is_some())
        .expect("a folded code node");
    let key = model.fold_key(target).expect("key");
    chunks.toggle(&model, &mut g, Toggle::Expand(target));
    let mut rev = snap.clone();
    rev.nodes.reverse();
    let (model2, scene2) = ProjectModel::build(&rev).expect("build");
    let again = model2.slot_of_fold_key(&key).expect("still there");
    assert_ne!(again, target, "renumbered");
    g.set_scene(scene2);
    chunks.forget_cut();
    chunks.refresh(&model2, &mut g, false);
    assert!(
        chunks.fold().is_expanded(again),
        "the user's expansion is kept"
    );
    for &c in model2.hierarchy().children(again) {
        assert!(g.is_drawn(c));
    }
}

/// Notices count once on their work node (never folded), and the carrier rule that a
/// hidden node's count uses (STG-DRAW-8) is its drawn ancestor, or its CPU group.
#[test]
fn notices_count_on_the_work_node_and_hidden_nodes_carry_to_their_ancestor_or_group() {
    let (device, _q) = gpu();
    let snap = demo::synthetic_project(12_000, 9);
    let (mut model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    let node = |key: &str, state, asks| PlanNode {
        key: key.into(),
        state,
        label: key.into(),
        blocked_by: Vec::<(String, BlockKind)>::new(),
        asks,
        parent: None,
    };
    model
        .set_plan(
            &mut g,
            &[
                node("run/c1", WorkState::InFlight, 2),
                node("run/gate/g", WorkState::GateHolding, 0),
                node("run/c2", WorkState::HeldByCap, 0),
            ],
        )
        .expect("plan");
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    let badges = chunks.badges(&model, &g);
    let c1 = model.task_slot("run/c1").expect("c1");
    let gate = model.task_slot("run/gate/g").expect("gate");
    let on = |s: u32| badges.iter().find(|b| b.at == Carrier::Node(s));
    assert_eq!(on(c1).map(|b| b.tally.of(NoticeKind::NeedsInput)), Some(2));
    assert_eq!(on(gate).map(|b| b.tally.of(NoticeKind::Failed)), Some(1));
    assert!(
        model.task_slot("run/c2").and_then(on).is_none(),
        "a cap hold is not a notice"
    );
    assert_eq!(fold::total(&badges).attention(), 3);

    // A folded code node carries to its drawn ancestor.
    let hidden = (0..g.scene().nodes().len() as u32)
        .find(|&s| chunks.fold().is_hidden(s))
        .expect("hidden");
    let anc = chunks.fold().carrier(hidden).expect("carrier");
    assert_eq!(chunks.carrier(&g, hidden), Some(Carrier::Node(anc)));
    // Unfolded on the CPU budget (12,000 nodes over the 10,000 cap), a capped node
    // carries to the group sprite standing for it.
    let (m2, s2) = ProjectModel::build(&snap).expect("build");
    let mut cpu = Galaxy::new(&device, s2, RenderBudget::Cpu);
    let mut open = Chunks::new(usize::MAX);
    open.refresh(&m2, &mut cpu, false);
    let capped = (0..cpu.scene().nodes().len() as u32)
        .find(|&s| cpu.group_of(s).is_some())
        .expect("a grouped node");
    let grp = cpu.group_of(capped).expect("group");
    assert_eq!(open.carrier(&cpu, capped), Some(Carrier::Group(grp)));
}

/// Brightest pixel sum in a 5 by 5 box around `p`.
fn brightness_near(img: &[u8], p: [f32; 2]) -> u32 {
    let (cx, cy) = (p[0] as i64, p[1] as i64);
    let mut best = 0;
    for y in cy - 2..=cy + 2 {
        for x in cx - 2..=cx + 2 {
            if x < 0 || y < 0 || x >= i64::from(W) || y >= i64::from(H) {
                continue;
            }
            let i = ((y as u32 * W + x as u32) * 4) as usize;
            best = best.max(u32::from(img[i]) + u32::from(img[i + 1]) + u32::from(img[i + 2]));
        }
    }
    best
}

/// Folding writes only the edges whose drawn state changed (an edge into code that
/// stays folded is empty before and after). This checks, on the in-place upload path,
/// that an edge between a drawn parent and its child disappears when the parent folds
/// and comes back when it unfolds, by the pixels on its midpoint.
#[test]
fn a_fold_rewrites_the_edges_it_changes_in_place() {
    let (device, queue) = gpu();
    let at = |x: f32, y: f32| Position { x, y, z: 0.0 };
    let node = |id: &str, path: &str, kind, p: Position| SceneNode {
        id: id.into(),
        path: path.into(),
        kind,
        qualified_name: id.into(),
        position: p,
    };
    let contains = |a: &str, b: &str| SceneEdge {
        id: format!("{a}>{b}"),
        src: a.into(),
        dst: b.into(),
        kind: EdgeKind::Contains,
    };
    // A file and its function far apart on one line; filler elsewhere so the fold
    // change is a small share of the scene and is written in place.
    let mut snap = GraphSnapshot {
        nodes: vec![
            node("f", "a/b/c/x.rs", NodeKind::File, at(-300.0, 0.0)),
            node("g", "a/b/c/x.rs", NodeKind::Function, at(300.0, 0.0)),
            node("filler", "d/e/f/y.rs", NodeKind::File, at(0.0, 600.0)),
        ],
        edges: vec![contains("f", "g")],
        ..GraphSnapshot::default()
    };
    for k in 0..40 {
        let id = format!("h{k}");
        snap.nodes.push(node(
            &id,
            "d/e/f/y.rs",
            NodeKind::Function,
            at(k as f32 * 5.0, 650.0),
        ));
        snap.edges.push(contains("filler", &id));
    }
    let (model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    g.frame_all();
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    let target = RenderTarget::new(&device, W, H).expect("target");
    let render = |g: &mut Galaxy| {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        g.render(&device, &queue, &mut encoder, &target)
            .expect("render");
        queue.submit([encoder.finish()]);
        target.read_pixels(&device, &queue).expect("readback")
    };
    let bg: u32 = BACKGROUND_SRGB.iter().map(|&c| u32::from(c)).sum();
    let mid = screen(&g, model.slot_of("f").expect("f"))
        .expect("f on screen")
        .0;
    let far = screen(&g, model.slot_of("g").expect("g"))
        .expect("g on screen")
        .0;
    let mid = [(mid[0] + far[0]) / 2.0, (mid[1] + far[1]) / 2.0];
    let img = render(&mut g);
    assert!(brightness_near(&img, mid) > bg, "the edge is drawn");
    let full = g.upload_stats().full;

    let f = model.slot_of("f").expect("f");
    chunks.toggle(&model, &mut g, Toggle::Collapse(f));
    settle(&mut g);
    let img = render(&mut g);
    assert_eq!(brightness_near(&img, mid), bg, "folded: the edge is gone");
    chunks.toggle(&model, &mut g, Toggle::Expand(f));
    settle(&mut g);
    let img = render(&mut g);
    assert!(
        brightness_near(&img, mid) > bg,
        "unfolded: the edge is back"
    );
    assert_eq!(
        g.upload_stats().full,
        full,
        "every change was written in place"
    );
}

/// A plan node with a parent.
fn work(key: &str, state: WorkState, asks: usize, parent: Option<&str>) -> PlanNode {
    PlanNode {
        key: key.into(),
        state,
        label: key.into(),
        blocked_by: Vec::new(),
        asks,
        parent: parent.map(str::to_owned),
    }
}

/// On work nodes: a phase is folded by default (the cut never
/// opens a work node, and a notice never unfolds anything), so an item's question
/// stacks on the phase as a yellow count with the kind in words; expanding the phase
/// splits the count back onto the item.
#[test]
fn an_items_ask_stacks_on_its_folded_phase_and_splits_on_expand() {
    let (device, _q) = gpu();
    let (mut model, scene) = ProjectModel::build(&GraphSnapshot::default()).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    model
        .set_plan(
            &mut g,
            &[
                work("work:p/phase4", WorkState::PresumedPartial, 0, None),
                work("r1/import", WorkState::InFlight, 1, Some("work:p/phase4")),
                work(
                    "work:p/tests",
                    WorkState::PresumedRemaining,
                    0,
                    Some("work:p/phase4"),
                ),
            ],
        )
        .expect("plan");
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    let phase = model.task_slot("work:p/phase4").expect("phase");
    let item = model.task_slot("r1/import").expect("item");
    assert_eq!(model.hierarchy().parent(item), Some(phase));
    assert!(!g.is_drawn(item), "folded by default");
    assert_eq!(chunks.fold().hidden_below(phase), 2, "+2 on the phase");

    let badges = chunks.badges(&model, &g);
    let on = |b: &[fold::Badge], s: u32| b.iter().find(|x| x.at == Carrier::Node(s)).cloned();
    let stacked = on(&badges, phase).expect("stacked on the phase");
    assert_eq!(stacked.tally.attention(), 1);
    assert_eq!(stacked.tally.words(), "1 approval");
    assert!(on(&badges, item).is_none());

    chunks.toggle(&model, &mut g, Toggle::Expand(phase));
    assert!(g.is_drawn(item));
    let badges = chunks.badges(&model, &g);
    assert!(on(&badges, phase).is_none(), "the count left the phase");
    assert_eq!(on(&badges, item).map(|b| b.tally.attention()), Some(1));
    assert_eq!(
        fold::total(&badges).attention(),
        1,
        "the total never changes"
    );

    chunks.toggle(&model, &mut g, Toggle::Collapse(phase));
    let badges = chunks.badges(&model, &g);
    assert_eq!(on(&badges, phase).map(|b| b.tally.attention()), Some(1));
}

/// A contract whose handoff was split keeps its children (`<id>.hN`) under it. A
/// contract has no failed state of its own: a failure is a gate, and a gate folds
/// under its major contract. So a gate failing over the handoff child `r1/a.h1`
/// stacks on the folded child, and with it on the folded parent `r1/a`, with the
/// child's own question.
#[test]
fn a_handoff_childs_failure_stacks_on_its_folded_parent_contract() {
    let (device, _q) = gpu();
    let (mut model, scene) = ProjectModel::build(&GraphSnapshot::default()).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    model
        .set_plan(
            &mut g,
            &[
                work("r1/a", WorkState::Split, 0, None),
                work("r1/a.h1", WorkState::HeldForUser, 0, Some("r1/a")),
                work("r1/a.h2", WorkState::Planned, 0, Some("r1/a")),
                work("r1/gate/g1", WorkState::GateHolding, 0, Some("r1/a.h1")),
            ],
        )
        .expect("plan");
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    let parent = model.task_slot("r1/a").expect("parent");
    let child = model.task_slot("r1/a.h1").expect("child");
    let gate = model.task_slot("r1/gate/g1").expect("gate");
    assert_eq!(model.hierarchy().parent(gate), Some(child));
    assert_eq!(chunks.fold().hidden_below(parent), 3);

    let badges = chunks.badges(&model, &g);
    let on = |b: &[fold::Badge], s: u32| b.iter().find(|x| x.at == Carrier::Node(s)).cloned();
    let stacked = on(&badges, parent).expect("stacked on the contract");
    assert_eq!(
        stacked.tally.attention(),
        2,
        "the failure counts in the yellow pill"
    );
    assert_eq!(stacked.tally.of(NoticeKind::Failed), 1);
    assert_eq!(stacked.tally.words(), "1 failed gate, 1 held for you");

    // Expand the contract: the child carries its hold and its folded gate's failure.
    chunks.toggle(&model, &mut g, Toggle::Expand(parent));
    let badges = chunks.badges(&model, &g);
    let on_child = on(&badges, child).expect("on the child");
    assert_eq!(on_child.tally.words(), "1 failed gate, 1 held for you");
    // Expand the child: the gate carries its own failure (orange on the item itself).
    chunks.toggle(&model, &mut g, Toggle::Expand(child));
    let badges = chunks.badges(&model, &g);
    assert_eq!(
        on(&badges, gate).map(|b| b.tally.of(NoticeKind::Failed)),
        Some(1)
    );
    assert_eq!(
        on(&badges, child).map(|b| b.tally.words()),
        Some("1 held for you".into())
    );

    // Selecting the expanded contract lights its children (it has no edges to them).
    g.select(Some(parent));
    let sel = g.selection().expect("selected");
    assert!(
        sel.is_highlighted(child) && sel.is_highlighted(model.task_slot("r1/a.h2").expect("h2"))
    );

    // A parent the plan does not draw, and a cycle, leave the node at the top.
    model
        .set_plan(
            &mut g,
            &[
                work("r1/x", WorkState::Planned, 0, Some("r1/missing")),
                work("r1/y", WorkState::Planned, 0, Some("r1/z")),
                work("r1/z", WorkState::Planned, 0, Some("r1/y")),
            ],
        )
        .expect("plan");
    let x = model.task_slot("r1/x").expect("x");
    assert_eq!(model.hierarchy().parent(x), None);
    assert_eq!(model.refused_work_parents(), 1, "the cycle is counted once");
    assert_eq!(model.refused_contains(), 0, "and not as a contains edge");
    assert!(model.task_slot("r1/a").is_none(), "the old plan left");

    // A false count once seen: a parent order that flips direction between two plans
    // (c under b becomes b under c) is no cycle and counts nothing.
    let before = model.refused_work_parents();
    model
        .set_plan(
            &mut g,
            &[
                work("r1/b", WorkState::Planned, 0, None),
                work("r1/c", WorkState::Planned, 0, Some("r1/b")),
            ],
        )
        .expect("plan");
    model
        .set_plan(
            &mut g,
            &[
                work("r1/b", WorkState::Planned, 0, Some("r1/c")),
                work("r1/c", WorkState::Planned, 0, None),
            ],
        )
        .expect("plan");
    let (b, c) = (
        model.task_slot("r1/b").expect("b"),
        model.task_slot("r1/c").expect("c"),
    );
    assert_eq!(model.hierarchy().parent(b), Some(c), "flipped");
    assert_eq!(model.refused_work_parents(), before, "no false count");
}

/// The fold handles drawn over the stage at the fit view and zoomed in, through a real
/// egui pass: no handle covers another drawn node's center (336 such
/// centers under 200 always-on handles), and a click on a covered center would still go
/// to the node, since a click picks first ([`kgraf_galaxy::view::click_at`]).
#[test]
fn no_fold_handle_covers_another_nodes_center() {
    let (device, _q) = gpu();
    let snap = demo::synthetic_project(10_500, 7);
    let (model, scene) = ProjectModel::build(&snap).expect("build");
    let mut g = Galaxy::new(&device, scene, RenderBudget::Gpu);
    g.frame_all();
    let mut chunks = Chunks::default();
    chunks.refresh(&model, &mut g, false);
    let ctx = egui::Context::default();
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(W as f32, H as f32));
    let marks_now = |g: &Galaxy, chunks: &Chunks| {
        let mut marks = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            marks = kgraf_galaxy::badges::draw_fold_marks(
                ui,
                rect,
                g,
                chunks.fold(),
                None,
                &kgraf_galaxy::Palette::default(),
            );
        });
        out.textures_delta.clear();
        marks
    };
    let ppp = ctx.pixels_per_point();
    let mut total = 0;
    // A folded file, to fly to: the handles appear as it grows on screen.
    let file = (0..g.scene().nodes().len() as u32)
        .find(|&s| chunks.fold().hidden_below(s) > 0 && model.node_id(s).is_some())
        .expect("a folded file");
    let at = Vec3::from(g.shown_position(file).expect("pos"));
    for spread in [None, Some(400.0f32), Some(120.0), Some(40.0)] {
        g.frame_all();
        if let Some(sp) = spread {
            g.camera_mut().fly_to(at, sp);
        }
        settle(&mut g);
        let label = spread.map_or("fit".to_owned(), |sp| format!("fly-to spread {sp}"));
        let marks = marks_now(&g, &chunks);
        total += marks.len();
        let mut covered = 0;
        for s in 0..g.scene().nodes().len() as u32 {
            if !g.is_drawn(s) {
                continue;
            }
            let Some((p, _)) = screen(&g, s) else {
                continue;
            };
            let at = egui::pos2(p[0] / ppp, p[1] / ppp);
            if marks.iter().any(|m| m.owner != s && m.rect.contains(at)) {
                covered += 1;
            }
        }
        eprintln!(
            "{label}: {} handles, {covered} centers covered",
            marks.len()
        );
        assert_eq!(covered, 0, "{label}");
        assert!(marks.len() <= kgraf_galaxy::badges::MAX_FOLD_MARKS);
    }
    assert!(total > 0, "zoomed in, handles are drawn");
}

/// The click rule over hot zones: pick first; a zone wins only over empty space or
/// over its own node.
#[test]
fn a_zone_never_takes_a_click_on_another_node() {
    use kgraf_galaxy::view::click_at;
    let zone = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(20.0, 10.0));
    let zones = [(zone, 7)];
    let at = egui::pos2(15.0, 15.0);
    let on_other = click_at(&zones, at, Some(3), None);
    assert_eq!(
        (on_other.zone, on_other.hit),
        (None, Some(3)),
        "the node under it wins"
    );
    assert_eq!(
        click_at(&zones, at, None, None).zone,
        Some(0),
        "empty space: the zone"
    );
    assert_eq!(
        click_at(&zones, at, Some(7), None).zone,
        Some(0),
        "its own node: the zone"
    );
    let off = click_at(&zones, egui::pos2(50.0, 50.0), Some(3), Some(3));
    assert_eq!((off.zone, off.was_selected), (None, true));
}
