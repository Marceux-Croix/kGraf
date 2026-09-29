//! Interaction against a real project: a real index, a real run record in
//! its store, the real galaxy panel on a headless egui-wgpu render state, and the
//! shell's interaction state between them. Each test drives the mechanism and reads
//! its effect on the graph.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Pos2, RawInput, Rect, vec2};
use kgraf_core::RunId;
use kgraf_galaxy::feed::FeedOptions;
use kgraf_galaxy::{ProjectView, RenderBudget, WorkState};
use kgraf_graph::GraphStore;
use kgraf_orchestrator::{ContractStatus, OpenContract, RunStateRecord, RunStore, WaitState};
use kgraf_shell::interaction::Interaction;

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

fn input() -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
        ..Default::default()
    }
}

/// A two-function project, indexed, with one run record: contract `a` running,
/// `b` held by the cap, `c` blocked on `a`.
fn project() -> (tempfile::TempDir, RunStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn parse_config() { render_frame(); }\npub fn render_frame() {}\n",
    )
    .expect("write");
    let db = kgraf_graph::snapshot::graph_db_path(dir.path());
    std::fs::create_dir_all(db.parent().expect("parent")).expect("state");
    let store = GraphStore::open(&db, dir.path()).expect("store");
    kgraf_graph::Indexer::new(&store).index().expect("index");
    let runs = RunStore::new(Arc::new(Mutex::new(store)));
    let mut r = RunStateRecord::new(RunId::new("r1"), "make the parser faster", 10);
    let contract = |id: &str, status| {
        let mut c = OpenContract::planned(id, format!("r1/{id}").as_str().into(), "tune it", "ok");
        c.status = status;
        c
    };
    let mut c = contract("c", ContractStatus::Planned);
    c.depends_on = vec!["a".into()];
    r.open_contracts = vec![
        contract("a", ContractStatus::Dispatched),
        contract("b", ContractStatus::Planned),
        c,
    ];
    r.coordination.waits.insert("a".into(), WaitState::Running);
    r.coordination.waits.insert(
        "b".into(),
        WaitState::Capacity {
            detail: "work-item cap 1 reached".into(),
        },
    );
    r.coordination.waits.insert(
        "c".into(),
        WaitState::Dependency {
            on: vec!["a".into()],
        },
    );
    runs.save(&r).expect("save");
    runs.set_active(&RunId::new("r1")).expect("active");
    (dir, runs)
}

/// Frames until `done` holds: the panel drains its feed, the interaction takes
/// the run watch's news and hands the plan to the panel, as the app does.
fn settle(
    ctx: &egui::Context,
    rs: &egui_wgpu::RenderState,
    panel: &mut ProjectView,
    interaction: &mut Interaction,
    done: impl Fn(&ProjectView) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        interaction.poll();
        if panel.model().is_some()
            && let Some((plan, notes)) = interaction.take_plan()
        {
            panel.set_plan(plan);
            panel.set_agent_notes(notes);
        }
        let mut out = ctx.run_ui(input(), |ui| panel.show(ui, rs));
        out.textures_delta.clear();
        if done(panel) {
            return;
        }
        assert!(Instant::now() < deadline, "the graph never settled");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn open(root: &Path, ctx: &egui::Context) -> (ProjectView, Interaction) {
    let options = FeedOptions {
        poll: Duration::from_millis(20),
        recheck: Duration::from_millis(20),
        ..FeedOptions::default()
    };
    let panel =
        ProjectView::open_with(root.to_path_buf(), RenderBudget::Gpu, options, ctx).expect("feed");
    (panel, Interaction::new(ctx, Some(root), None))
}

#[test]
fn the_run_record_is_drawn_as_work_status_on_the_graph() {
    let (dir, _runs) = project();
    let rs = render_state();
    let ctx = egui::Context::default();
    let (mut panel, mut interaction) = open(dir.path(), &ctx);
    settle(&ctx, &rs, &mut panel, &mut interaction, |p| {
        p.model().is_some_and(|m| m.plan_nodes().count() == 3)
    });
    let model = panel.model().expect("live");
    let state = |key: &str| {
        let slot = model.task_slot(key).expect("work node");
        model.plan_at(slot).expect("plan node").state
    };
    assert_eq!(state("r1/a"), WorkState::InFlight);
    assert_eq!(state("r1/b"), WorkState::HeldByCap);
    assert_eq!(state("r1/c"), WorkState::Blocked);
    // The block is an edge from c to a, in the scene the renderer draws.
    let scene = panel.view().expect("view").galaxy().scene();
    let (a, c) = (
        model.task_slot("r1/a").expect("a"),
        model.task_slot("r1/c").expect("c"),
    );
    assert!(
        scene.incident_edges(c).iter().any(
            |&e| scene.edges()[e as usize].source == c && scene.edges()[e as usize].target == a
        ),
        "blocked-by is drawn as an edge"
    );
}

#[test]
fn a_search_is_a_selection_on_the_graph_with_the_camera_on_it() {
    let (dir, _runs) = project();
    let rs = render_state();
    let ctx = egui::Context::default();
    let (mut panel, mut interaction) = open(dir.path(), &ctx);
    settle(&ctx, &rs, &mut panel, &mut interaction, |p| {
        p.model().is_some()
    });
    let target_before = panel.view().expect("view").galaxy().camera().target();

    interaction.search.query = "parse_config".into();
    interaction.search_now(Some(&mut panel));
    let model = panel.model().expect("model");
    let want = model
        .code_nodes()
        .find(|(_, q, ..)| q.ends_with("parse_config"))
        .map(|(s, ..)| s)
        .expect("indexed");
    let galaxy = panel.view().expect("view").galaxy();
    let sel = galaxy.selection().expect("the result is a selection");
    assert_eq!(sel.members(), &[want], "exactly the match is lit");
    assert!(galaxy.camera().is_flying() || galaxy.camera().target() != target_before);

    // A work-state query lights the work node, through the same selection.
    interaction.search.query = "status:cap".into();
    settle(&ctx, &rs, &mut panel, &mut interaction, |p| {
        p.model().is_some_and(|m| m.plan_nodes().count() == 3)
    });
    interaction.search_now(Some(&mut panel));
    let model = panel.model().expect("model");
    let capped = model.task_slot("r1/b").expect("b");
    let sel = panel
        .view()
        .expect("view")
        .galaxy()
        .selection()
        .expect("selected");
    assert_eq!(sel.members(), &[capped]);

    // The user clicks another node; the model then changes and the same query
    // re-runs. The user's selection stays, and the camera is not sent back.
    let clicked = model
        .code_nodes()
        .find(|(_, q, ..)| q.ends_with("render_frame"))
        .map(|(s, ..)| s)
        .expect("indexed");
    panel
        .view_mut()
        .expect("view")
        .galaxy_mut()
        .select(Some(clicked));
    interaction.on_selection(&kgraf_shell::interaction::Selected {
        slot: Some(clicked),
        agent: None,
        work: None,
    });
    panel.set_plan(Vec::new());
    interaction.search_now(Some(&mut panel));
    assert_eq!(
        panel
            .view()
            .expect("view")
            .galaxy()
            .selection()
            .map(|s| s.selected()),
        Some(clicked),
        "a re-run on a changed model does not take the user's selection"
    );

    // Clearing the field leaves the user's own selection alone.
    interaction.search.query.clear();
    interaction.search_now(Some(&mut panel));
    assert_eq!(
        panel
            .view()
            .expect("view")
            .galaxy()
            .selection()
            .map(|s| s.selected()),
        Some(clicked)
    );
    // While the search owns the selection, clearing the field clears it.
    interaction.search.query = "parse_config".into();
    interaction.search_now(Some(&mut panel));
    interaction.search.query.clear();
    interaction.search_now(Some(&mut panel));
    assert!(panel.view().expect("view").galaxy().selection().is_none());
}

/// Every text egui painted in one pass of the panel.
fn painted_texts(
    ctx: &egui::Context,
    rs: &egui_wgpu::RenderState,
    panel: &mut ProjectView,
) -> Vec<String> {
    fn collect(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.galley.text().to_owned()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
            _ => {}
        }
    }
    let mut out = ctx.run_ui(input(), |ui| panel.show(ui, rs));
    out.textures_delta.clear();
    let mut texts = Vec::new();
    for s in &out.shapes {
        collect(&s.shape, &mut texts);
    }
    texts
}

/// Status is on the nodes, as text a reader sees at a glance: the capped node
/// says it is held by the cap, the blocked one says by what, and the agent's
/// label carries what it is doing.
#[test]
fn status_labels_are_painted_on_the_work_nodes_and_agents() {
    let (dir, _runs) = project_with_agent();
    let rs = render_state();
    let ctx = egui::Context::default();
    let (mut panel, mut interaction) = open(dir.path(), &ctx);
    let deadline = Instant::now() + Duration::from_secs(10);
    while interaction.corpus.get("r1", WORKER).is_none() {
        assert!(Instant::now() < deadline, "the transcript never loaded");
        interaction.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    settle(&ctx, &rs, &mut panel, &mut interaction, |p| {
        p.model()
            .is_some_and(|m| m.plan_nodes().count() == 3 && m.agent_slot(HOLDER).is_some())
    });
    let texts = painted_texts(&ctx, &rs, &mut panel);
    let has = |needle: &str| texts.iter().any(|t| t.contains(needle));
    assert!(has("b: held by the concurrency cap"), "{texts:?}");
    assert!(has("c: blocked (by a)"), "{texts:?}");
    assert!(has("a: in flight"), "{texts:?}");
    assert!(
        has("r1/a@n1: apply_patch *** Begin Patch"),
        "the agent's current action: {texts:?}"
    );
}

const HOLDER: &str = "r1/a@n1";
const WORKER: &str = "r1-a-r0-a0";

/// The project, plus: contract `a` has worker `r1-a-r0-a0`, which holds a lease on
/// `src/lib.rs` as `r1/a@n1` (so it is an agent on the graph), and whose transcript
/// is on disk.
fn project_with_agent() -> (tempfile::TempDir, RunStore) {
    let (dir, runs) = project();
    let mut r = runs.load(&RunId::new("r1")).expect("load").expect("record");
    if let Some(c) = r.contract_mut("a") {
        c.workers = vec![WORKER.into()];
    }
    r.updated_ms = 20;
    runs.save(&r).expect("save");
    {
        let g = runs.graph().lock().expect("lock");
        let reg = kgraf_graph::LeaseRegistry::new(&g);
        let got = reg
            .acquire(
                HOLDER,
                "r1/a",
                &["src/lib.rs".to_owned()],
                &[],
                &[],
                kgraf_graph::snapshot::now_ms(),
            )
            .expect("lease");
        assert!(matches!(got, kgraf_graph::MixedAcquisition::Granted { .. }));
    }
    let workers = dir.path().join(".kgraf/runs/r1/workers");
    std::fs::create_dir_all(&workers).expect("mkdir");
    let line = |seq: u64, event: serde_json::Value| {
        format!("{}\n", serde_json::json!({"seq": seq, "event": event}))
    };
    let transcript = [
        line(
            0,
            serde_json::json!({"type": "text_delta", "text": "We chose the arena allocator "}),
        ),
        line(
            1,
            serde_json::json!({"type": "text_delta", "text": "because frees are batched."}),
        ),
        line(
            2,
            serde_json::json!({"type": "tool_call_started", "call_id": "t1", "tool": "apply_patch",
                "input": {"patch": "*** Begin Patch"}}),
        ),
    ]
    .concat();
    std::fs::write(workers.join(format!("{WORKER}.jsonl")), transcript).expect("write");
    (dir, runs)
}

/// Selecting an agent fills the one text surface with its verbatim transcript:
/// the deltas concatenated exactly as sent, the tool call with its arguments.
#[test]
fn selecting_an_agent_shows_its_verbatim_transcript() {
    use egui_kittest::kittest::Queryable;
    use kgraf_shell::interaction::Selected;
    use kgraf_shell::text_surface::Feed;
    let (dir, _runs) = project_with_agent();
    let rs = render_state();
    let ctx = egui::Context::default();
    let (mut panel, mut interaction) = open(dir.path(), &ctx);
    settle(&ctx, &rs, &mut panel, &mut interaction, |p| {
        p.model().is_some_and(|m| m.agent_slot(HOLDER).is_some())
    });
    // The transcript corpus arrives on its own thread.
    let deadline = Instant::now() + Duration::from_secs(10);
    // The run records arrive on their own thread too; the conversation's
    // workers come from them (this raced under a loaded machine).
    while interaction.corpus.get("r1", WORKER).is_none() || interaction.runs.runs.is_empty() {
        assert!(Instant::now() < deadline, "the transcript never loaded");
        interaction.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    let slot = panel
        .model()
        .expect("model")
        .agent_slot(HOLDER)
        .expect("agent");
    panel
        .view_mut()
        .expect("view")
        .galaxy_mut()
        .select(Some(slot));
    interaction.on_selection(&Selected {
        slot: Some(slot),
        agent: Some(HOLDER.into()),
        work: None,
    });
    let Feed::Conversation(c) = &interaction.text.feed else {
        panic!("{:?}", interaction.text.feed);
    };
    assert_eq!(c.sources, [("r1".to_owned(), WORKER.to_owned())]);
    assert_eq!(c.briefs, [("a".to_owned(), "tune it".to_owned())]);

    // Drawn: the verbatim text is on the panel.
    let mut h = egui_kittest::Harness::new_ui_state(
        |ui, i: &mut Interaction| {
            // The theme's fonts apply from the pass after they are set.
            if kgraf_shell::theme::ensure(ui.ctx()) {
                i.text_surface(ui, None);
            }
        },
        interaction,
    );
    h.run();
    h.get_by_label("We chose the arena allocator because frees are batched.");
    h.get_by_label_contains("*** Begin Patch");
}

/// A phrase from a conversation fills the panel with a snippet; opening it selects
/// its node on the graph and expands the whole conversation, focused on the turn.
#[test]
fn a_snippet_routes_back_to_its_node_and_expands_in_place() {
    use kgraf_shell::text_surface::Feed;
    let (dir, _runs) = project_with_agent();
    let rs = render_state();
    let ctx = egui::Context::default();
    let (mut panel, mut interaction) = open(dir.path(), &ctx);
    settle(&ctx, &rs, &mut panel, &mut interaction, |p| {
        p.model().is_some_and(|m| m.plan_nodes().count() == 3)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while interaction.corpus.get("r1", WORKER).is_none() {
        assert!(Instant::now() < deadline, "the transcript never loaded");
        interaction.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    interaction.search.query = "\"frees are batched\"".into();
    interaction.search_now(Some(&mut panel));
    assert_eq!(interaction.text.feed, Feed::Snippets);
    assert_eq!(interaction.search.results.turns.len(), 1);
    let hit = interaction.search.results.turns[0].clone();
    let at = hit.at.clone();
    assert_eq!(at.worker, WORKER);

    interaction.open_snippet(&hit, Some(&mut panel));
    let model = panel.model().expect("model");
    let selected = panel
        .view()
        .expect("view")
        .galaxy()
        .selection()
        .expect("the snippet's node is selected")
        .selected();
    assert!(
        selected == model.task_slot("r1/a").expect("work")
            || Some(selected) == model.agent_slot(HOLDER),
        "the node of that conversation"
    );
    let Feed::Conversation(c) = &interaction.text.feed else {
        panic!("{:?}", interaction.text.feed);
    };
    assert_eq!(c.focus.as_ref(), Some(&at), "expanded in place at the turn");
}
