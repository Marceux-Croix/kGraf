//! The egui widget path the shell mounts, driven headlessly through real egui passes
//! and a real egui-wgpu render state: frames render on show, not while idle, and a
//! click selects and animates.

use std::time::Duration;

use egui::{Event, PointerButton, Pos2, RawInput, Rect, pos2, vec2};
use glam::Vec3;
use kgraf_galaxy::badges::MarkAction;
use kgraf_galaxy::feed::FeedOptions;
use kgraf_galaxy::fold::Toggle;
use kgraf_galaxy::gpu::BACKGROUND_SRGB;
use kgraf_galaxy::{GalaxyView, NodeInput, ProjectView, RenderBudget, Scene, SceneInput, demo};

const W: f32 = 400.0;
const H: f32 = 300.0;

fn render_state() -> egui_wgpu::RenderState {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(egui_wgpu::RenderState::create(
        &egui_wgpu::WgpuConfiguration::default(),
        &instance,
        None,
        egui_wgpu::RendererOptions::default(),
    ))
    .expect("headless egui-wgpu render state")
}

fn input(events: Vec<Event>) -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(W, H))),
        events,
        ..Default::default()
    }
}

/// One egui pass with the galaxy filling the screen; returns egui's repaint delay.
fn pass(
    ctx: &egui::Context,
    view: &mut GalaxyView,
    rs: &egui_wgpu::RenderState,
    events: Vec<Event>,
) -> Duration {
    let mut out = ctx.run_ui(input(events), |ui| view.show(ui, rs));
    // The font atlas upload is not applied in a test; egui asserts on that.
    out.textures_delta.clear();
    out.viewport_output
        .get(&egui::ViewportId::ROOT)
        .map_or(Duration::MAX, |v| v.repaint_delay)
}

/// Three nodes at known positions; the click target is chosen from these, not from
/// the picker under test.
fn three_nodes() -> Scene {
    let node = |x: f32, y: f32| NodeInput {
        position: [x, y, 0.0],
        color: [1.0, 1.0, 1.0],
        size: 20.0,
    };
    Scene::new(SceneInput {
        nodes: vec![node(-200.0, 0.0), node(0.0, 0.0), node(200.0, 100.0)],
        edges: vec![],
    })
    .expect("scene")
}

#[test]
fn widget_renders_once_then_idles_and_a_click_selects() {
    let rs = render_state();
    let ctx = egui::Context::default();
    let mut view = GalaxyView::new(&rs, three_nodes(), RenderBudget::Gpu);

    pass(&ctx, &mut view, &rs, vec![]);
    assert_eq!(view.galaxy().frames_rendered(), 1, "first show renders");
    let target = view.target().expect("target created");
    let pixels = target.read_pixels(&rs.device, &rs.queue).expect("readback");
    let lit = pixels
        .chunks_exact(4)
        .filter(|p| p[..3] != BACKGROUND_SRGB)
        .count();
    assert!(lit > 50, "the panel shows the galaxy: {lit} lit pixels");
    let (w, h) = (target.width, target.height);

    // egui asks for one follow-up frame itself; after that an idle galaxy asks for none
    // and renders nothing.
    pass(&ctx, &mut view, &rs, vec![]);
    let delay = pass(&ctx, &mut view, &rs, vec![]);
    assert_ne!(delay, Duration::ZERO, "idle: no repaint requested");
    assert_eq!(
        view.galaxy().frames_rendered(),
        1,
        "idle: no frames rendered"
    );

    // Click where node 2 projects (from its known position through the camera).
    let frame = view.galaxy().camera().frame(w as f32, h as f32);
    let ppp = ctx.pixels_per_point();
    let (p, _) = frame
        .project(Vec3::new(200.0, 100.0, 0.0))
        .expect("node 2 in front of the camera");
    let at = pos2(p[0] / ppp, p[1] / ppp);
    let button = |pressed| Event::PointerButton {
        pos: at,
        button: PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    pass(
        &ctx,
        &mut view,
        &rs,
        vec![Event::PointerMoved(at), button(true)],
    );
    let delay = pass(&ctx, &mut view, &rs, vec![button(false)]);
    let sel = view.galaxy().selection().expect("click selected a node");
    assert_eq!(sel.selected(), 2, "the click lands on node 2");
    assert!(
        view.galaxy().camera().is_flying(),
        "selection flies the camera"
    );
    assert_eq!(delay, Duration::ZERO, "flying: repaint requested");
    assert!(
        view.galaxy().frames_rendered() >= 2,
        "selection re-rendered"
    );
}

#[test]
fn dropping_the_view_frees_its_egui_texture() {
    let rs = render_state();
    let ctx = egui::Context::default();
    let scene = Scene::new(demo::synthetic(500, 7)).expect("scene");
    let mut view = GalaxyView::new(&rs, scene, RenderBudget::Gpu);
    pass(&ctx, &mut view, &rs, vec![]);
    let id = view.texture_id().expect("registered");
    assert!(
        rs.renderer.read().texture(&id).is_some(),
        "registered while shown"
    );
    drop(view);
    assert!(rs.renderer.read().texture(&id).is_none(), "freed on drop");
}

/// The panel the shell mounts for a project directory: an unindexed directory shows
/// its reason instead of a galaxy (and does not crash); once indexed, the galaxy loads
/// without reopening, and rendering it draws the project.
#[test]
fn a_project_panel_reports_an_unindexed_directory_then_shows_the_indexed_galaxy() {
    let rs = render_state();
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn alpha() { beta(); }\npub fn beta() {}\n",
    )
    .expect("write");
    let options = FeedOptions {
        poll: Duration::from_millis(20),
        recheck: Duration::from_millis(20),
        ..FeedOptions::default()
    };
    let mut panel =
        ProjectView::open_with(dir.path().to_path_buf(), RenderBudget::Gpu, options, &ctx)
            .expect("feed");
    let show = |panel: &mut ProjectView| {
        let mut out = ctx.run_ui(input(vec![]), |ui| panel.show(ui, &rs));
        out.textures_delta.clear();
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while panel.unindexed().is_none() {
        assert!(std::time::Instant::now() < deadline, "no unindexed state");
        show(&mut panel);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(panel.unindexed().is_some_and(|r| r.contains("not indexed")));
    assert!(panel.view().is_none());

    std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
    let store = kgraf_graph::GraphStore::open(
        &kgraf_graph::snapshot::graph_db_path(dir.path()),
        dir.path(),
    )
    .expect("store");
    kgraf_graph::Indexer::new(&store).index().expect("index");
    let nodes = store.node_count().expect("n") as usize;
    while panel.model().is_none_or(|m| m.code_node_count() != nodes) {
        assert!(
            std::time::Instant::now() < deadline,
            "the index never loaded"
        );
        show(&mut panel);
        std::thread::sleep(Duration::from_millis(10));
    }
    // One more pass so the loaded scene is rendered.
    show(&mut panel);
    assert!(panel.error().is_none(), "{:?}", panel.error());
    let view = panel.view().expect("live");
    assert!(view.galaxy().frames_rendered() >= 1);
    let pixels = view
        .target()
        .expect("target")
        .read_pixels(&rs.device, &rs.queue)
        .expect("readback");
    let lit = pixels
        .chunks_exact(4)
        .filter(|p| p[..3] != BACKGROUND_SRGB)
        .count();
    assert!(lit > 0, "the project's galaxy is drawn");
    assert!(panel.timings().scene_build.is_some());
}

/// The fold through the panel the shell mounts, driven by real egui clicks (decision
/// 0068): a click on an expanded file selects it, a second click folds its chunks into
/// it, and a third unfolds them, each through the widget's picker.
#[test]
fn clicks_on_a_project_panel_fold_and_unfold_a_file() {
    let rs = render_state();
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub struct S;\nimpl S {\n    pub fn a(&self) { self.b(); }\n    pub fn b(&self) {}\n}\n\
         pub fn alpha() { beta(); }\npub fn beta() {}\n",
    )
    .expect("write");
    std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
    let store = kgraf_graph::GraphStore::open(
        &kgraf_graph::snapshot::graph_db_path(dir.path()),
        dir.path(),
    )
    .expect("store");
    kgraf_graph::Indexer::new(&store).index().expect("index");
    let nodes = store.node_count().expect("n") as usize;
    let options = FeedOptions {
        poll: Duration::from_millis(20),
        recheck: Duration::from_millis(20),
        ..FeedOptions::default()
    };
    let mut panel =
        ProjectView::open_with(dir.path().to_path_buf(), RenderBudget::Gpu, options, &ctx)
            .expect("feed");
    let show = |panel: &mut ProjectView, events: Vec<Event>| {
        let mut out = ctx.run_ui(input(events), |ui| panel.show(ui, &rs));
        out.textures_delta.clear();
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while panel.model().is_none_or(|m| m.code_node_count() != nodes) {
        assert!(
            std::time::Instant::now() < deadline,
            "the index never loaded"
        );
        show(&mut panel, vec![]);
        std::thread::sleep(Duration::from_millis(10));
    }
    show(&mut panel, vec![]);
    let model = panel.model().expect("live");
    let file = model
        .code_nodes()
        .find(|(_, _, _, kind, _)| *kind == kgraf_graph::NodeKind::File)
        .map(|(s, ..)| s)
        .expect("the file node");
    let children: Vec<u32> = model.hierarchy().children(file).to_vec();
    assert!(children.len() >= 3, "struct, impl and two functions");
    let drawn = |panel: &ProjectView, s: u32| panel.view().expect("live").galaxy().is_drawn(s);
    assert!(
        children.iter().all(|&c| drawn(&panel, c)),
        "a small project is all drawn"
    );

    // Lets the camera and the fold animation finish.
    let settle = |panel: &mut ProjectView| {
        for _ in 0..900 {
            show(panel, vec![]);
            let g = panel.view().expect("live").galaxy();
            if !g.camera().is_flying() && !g.is_animating() {
                return;
            }
        }
        panic!("never settled");
    };
    // Clicks where the picker finds the file (its children may sit in front of its
    // center).
    let click_file = |panel: &mut ProjectView| {
        let view = panel.view().expect("live");
        let g = view.galaxy();
        let target = view.target().expect("target");
        let (w, h) = (target.width, target.height);
        let frame = g.camera().frame(w as f32, h as f32);
        let (c, _) = frame
            .project(Vec3::from(g.shown_position(file).expect("pos")))
            .expect("in front");
        let px = (0..=12)
            .flat_map(|r| {
                let r = r as f32;
                [[0.0, 0.0], [r, 0.0], [-r, 0.0], [0.0, r], [0.0, -r]]
            })
            .map(|[dx, dy]| [c[0] + dx, c[1] + dy])
            .find(|&p| g.pick(p, w, h) == Some(file))
            .expect("the file is visible somewhere");
        let ppp = ctx.pixels_per_point();
        let at = pos2(px[0] / ppp, px[1] / ppp);
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        show(panel, vec![Event::PointerMoved(at), button(true)]);
        show(panel, vec![button(false)]);
        settle(panel);
    };

    click_file(&mut panel);
    let g = panel.view().expect("live").galaxy();
    assert_eq!(
        g.selection().map(|s| s.selected()),
        Some(file),
        "the first click selects"
    );
    assert!(
        children.iter().all(|&c| drawn(&panel, c)),
        "and folds nothing"
    );

    click_file(&mut panel);
    assert!(
        children.iter().all(|&c| !drawn(&panel, c)),
        "the second click folds"
    );
    let g = panel.view().expect("live").galaxy();
    assert_eq!(g.folded_count(), panel.chunks().fold().hidden_count());
    assert!(panel.chunks().fold().hidden_below(file) as usize >= children.len());

    click_file(&mut panel);
    assert!(
        children.iter().all(|&c| drawn(&panel, c)),
        "the third unfolds"
    );

    assert_eq!(panel.view().expect("live").galaxy().folded_count(), 0);
    assert_eq!(
        panel.stage_tally().map(|b| b.attention()),
        Some(0),
        "no work, no notices"
    );
}

/// The fold handles (STG-DRAW-5) through real egui clicks: "-" on the selected expanded
/// file folds it, "+N" unfolds it, and neither click changes the selection.
#[test]
fn the_fold_handles_fold_and_unfold_without_touching_the_selection() {
    let rs = render_state();
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn alpha() { beta(); }\npub fn beta() {}\npub fn gamma() {}\n",
    )
    .expect("write");
    std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
    let store = kgraf_graph::GraphStore::open(
        &kgraf_graph::snapshot::graph_db_path(dir.path()),
        dir.path(),
    )
    .expect("store");
    kgraf_graph::Indexer::new(&store).index().expect("index");
    let nodes = store.node_count().expect("n") as usize;
    let options = FeedOptions {
        poll: Duration::from_millis(20),
        recheck: Duration::from_millis(20),
        ..FeedOptions::default()
    };
    let mut panel =
        ProjectView::open_with(dir.path().to_path_buf(), RenderBudget::Gpu, options, &ctx)
            .expect("feed");
    let show = |panel: &mut ProjectView, events: Vec<Event>| {
        let mut out = ctx.run_ui(input(events), |ui| panel.show(ui, &rs));
        out.textures_delta.clear();
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while panel.model().is_none_or(|m| m.code_node_count() != nodes) {
        assert!(std::time::Instant::now() < deadline, "never loaded");
        show(&mut panel, vec![]);
        std::thread::sleep(Duration::from_millis(10));
    }
    let settle = |panel: &mut ProjectView| {
        for _ in 0..900 {
            show(panel, vec![]);
            let g = panel.view().expect("live").galaxy();
            if !g.camera().is_flying() && !g.is_animating() {
                show(panel, vec![]);
                return;
            }
        }
        panic!("never settled");
    };
    let click = |panel: &mut ProjectView, at: Pos2| {
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        show(panel, vec![Event::PointerMoved(at), button(true)]);
        show(panel, vec![button(false)]);
    };
    let model = panel.model().expect("live");
    let file = model
        .code_nodes()
        .find(|(_, _, _, kind, _)| *kind == kgraf_graph::NodeKind::File)
        .map(|(s, ..)| s)
        .expect("file");
    let kids: Vec<u32> = model.hierarchy().children(file).to_vec();
    assert_eq!(kids.len(), 3);
    panel
        .view_mut()
        .expect("live")
        .galaxy_mut()
        .select(Some(file));
    settle(&mut panel);
    let handle = |panel: &ProjectView, want: Toggle| {
        panel
            .marks()
            .iter()
            .find(|m| m.action == MarkAction::Toggle(want))
            .map(|m| m.rect.center())
    };
    let drawn = |panel: &ProjectView, s: u32| panel.view().expect("live").galaxy().is_drawn(s);

    let at = handle(&panel, Toggle::Collapse(file)).expect("a fold handle on the selected file");
    click(&mut panel, at);
    settle(&mut panel);
    assert!(
        kids.iter().all(|&k| !drawn(&panel, k)),
        "folded by the handle"
    );
    let g = panel.view().expect("live").galaxy();
    assert_eq!(
        g.selection().map(|s| s.selected()),
        Some(file),
        "still selected"
    );

    let at = handle(&panel, Toggle::Expand(file)).expect("a +N handle on the folded file");
    click(&mut panel, at);
    settle(&mut panel);
    assert!(
        kids.iter().all(|&k| drawn(&panel, k)),
        "unfolded by the handle"
    );
    let g = panel.view().expect("live").galaxy();
    assert_eq!(
        g.selection().map(|s| s.selected()),
        Some(file),
        "still selected"
    );
    let line = panel.status_line().expect("line");
    assert!(!line.contains("over budget"), "{line}");
}

/// The stage HUD (the status line and the scene counts) can be hidden, as the shell
/// does where its project row says the same; the texts leave the painted output.
#[test]
fn the_stage_hud_can_be_hidden() {
    let rs = render_state();
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn alpha() {}\n").expect("write");
    std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
    let store = kgraf_graph::GraphStore::open(
        &kgraf_graph::snapshot::graph_db_path(dir.path()),
        dir.path(),
    )
    .expect("store");
    kgraf_graph::Indexer::new(&store).index().expect("index");
    let nodes = store.node_count().expect("n") as usize;
    let options = FeedOptions {
        poll: Duration::from_millis(20),
        recheck: Duration::from_millis(20),
        ..FeedOptions::default()
    };
    let mut panel =
        ProjectView::open_with(dir.path().to_path_buf(), RenderBudget::Gpu, options, &ctx)
            .expect("feed");
    // Every text painted in one pass.
    let texts = |panel: &mut ProjectView| -> Vec<String> {
        let mut out = ctx.run_ui(input(vec![]), |ui| panel.show(ui, &rs));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::epaint::Shape::Text(t) => Some(t.galley.job.text.clone()),
                _ => None,
            })
            .collect()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while panel.model().is_none_or(|m| m.code_node_count() != nodes) {
        assert!(std::time::Instant::now() < deadline, "never loaded");
        texts(&mut panel);
        std::thread::sleep(Duration::from_millis(10));
    }
    let hud = |t: &[String]| {
        t.iter()
            .any(|s| s.contains("agent(s)") || s.contains(" edges"))
    };
    assert!(hud(&texts(&mut panel)), "the HUD is drawn by default");
    panel.set_hud(false);
    assert!(!hud(&texts(&mut panel)), "hidden");
    panel.set_hud(true);
    assert!(hud(&texts(&mut panel)), "shown again");

    // Fail loud: with the HUD hidden, a warning is still painted. Two work
    // nodes that name each other as parent: one parent is refused as a cycle.
    panel.set_hud(false);
    let work = |key: &str, parent: &str| kgraf_galaxy::PlanNode {
        key: key.into(),
        state: kgraf_galaxy::WorkState::Planned,
        label: key.into(),
        blocked_by: vec![],
        asks: 0,
        parent: Some(parent.into()),
    };
    panel.set_plan(vec![work("r1/a", "r1/b"), work("r1/b", "r1/a")]);
    let painted = texts(&mut panel);
    assert!(!hud(&painted), "the HUD stays hidden");
    assert!(
        painted
            .iter()
            .any(|s| s.contains("1 work parent(s) refused as cycles")),
        "{painted:?}"
    );

    // The warning as painted: broken data is dark text on an orange pill (COL-7,
    // STG-DRAW-11), starting at the overlay top the shell sets.
    let palette = *panel.palette();
    let warning = |panel: &mut ProjectView| {
        let mut out = ctx.run_ui(input(vec![]), |ui| panel.show(ui, &rs));
        out.textures_delta.clear();
        let text = out
            .shapes
            .iter()
            .find_map(|c| match &c.shape {
                egui::epaint::Shape::Text(t) if t.galley.job.text.contains("refused as cycles") => {
                    Some((t.pos.y, t.galley.job.sections[0].format.color))
                }
                _ => None,
            })
            .expect("the warning is painted");
        let pill = out
            .shapes
            .iter()
            .any(|c| matches!(&c.shape, egui::epaint::Shape::Rect(r) if r.fill == palette.danger));
        (text, pill)
    };
    let ((y_default, color), pill) = warning(&mut panel);
    assert_eq!(color, palette.pill_text, "dark text");
    assert!(pill, "on an orange pill");
    assert!(y_default < 100.0);
    panel.set_overlay_top(100.0);
    let ((y, _), _) = warning(&mut panel);
    assert!(y >= 100.0, "below the overlay top: {y}");
}

/// Section 3.4 through the painted output: a group (directory) label is painted in
/// white at 55%, a code label in white at 88%.
#[test]
fn group_labels_paint_dimmer_than_code_labels() {
    let rs = render_state();
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().expect("tempdir");
    for d in ["src/alpha_dir", "src/beta_dir"] {
        std::fs::create_dir_all(dir.path().join(d)).expect("mkdir");
    }
    std::fs::write(dir.path().join("src/alpha_dir/x.rs"), "pub fn alpha() {}\n").expect("x");
    std::fs::write(dir.path().join("src/beta_dir/y.rs"), "pub fn beta() {}\n").expect("y");
    std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
    let store = kgraf_graph::GraphStore::open(
        &kgraf_graph::snapshot::graph_db_path(dir.path()),
        dir.path(),
    )
    .expect("store");
    kgraf_graph::Indexer::new(&store).index().expect("index");
    let nodes = store.node_count().expect("n") as usize;
    let options = FeedOptions {
        poll: Duration::from_millis(20),
        recheck: Duration::from_millis(20),
        ..FeedOptions::default()
    };
    let mut panel =
        ProjectView::open_with(dir.path().to_path_buf(), RenderBudget::Gpu, options, &ctx)
            .expect("feed");
    // Every text painted in one pass, with its color.
    let texts = |panel: &mut ProjectView| -> Vec<(String, egui::Color32)> {
        let mut out = ctx.run_ui(input(vec![]), |ui| panel.show(ui, &rs));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::epaint::Shape::Text(t) => {
                    let color = t.galley.job.sections.first()?.format.color;
                    Some((
                        t.galley.job.text.clone(),
                        t.override_text_color.unwrap_or(color),
                    ))
                }
                _ => None,
            })
            .collect()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while panel.model().is_none_or(|m| m.code_node_count() != nodes) {
        assert!(std::time::Instant::now() < deadline, "never loaded");
        texts(&mut panel);
        std::thread::sleep(Duration::from_millis(10));
    }
    let model = panel.model().expect("live");
    let groups: Vec<(String, String)> = model
        .proxies()
        .map(|(_, p)| (p.short.clone(), p.full.clone()))
        .collect();
    assert!(!groups.is_empty(), "two directories make group proxies");
    let palette = *panel.palette();
    assert_ne!(palette.group_label, palette.text);
    let painted = texts(&mut panel);
    let colors_of = |name: &str| -> Vec<egui::Color32> {
        painted
            .iter()
            .filter(|(t, _)| t == name)
            .map(|&(_, c)| c)
            .collect()
    };
    let group_colors: Vec<egui::Color32> = groups
        .iter()
        .flat_map(|(s, f)| {
            let mut v = colors_of(s);
            if f != s {
                v.extend(colors_of(f));
            }
            v
        })
        .collect();
    assert!(
        group_colors.contains(&palette.group_label),
        "a group label is painted at 55%: {painted:?}"
    );
    assert!(!group_colors.contains(&palette.text), "never at 88%");
    let code = colors_of("alpha");
    assert!(code.contains(&palette.text), "code at 88%: {painted:?}");
    assert!(!code.contains(&palette.group_label));
}
