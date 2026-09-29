//! A project directory's galaxy in an egui panel: the feed, the model and the view
//! together.
//!
//! State machine (what the panel shows):
//! ```text
//!   Opening --Unindexed--> Unindexed (a clear message; the feed keeps checking)
//!   Opening --Loaded-----> Live
//!   Unindexed --Loaded---> Live (the user ran `kgraf index`)
//!   Live --Changes-------> Live (applied in place, animated)
//!   Live --Loaded--------> Live (a reload: the scene is replaced, the camera kept)
//!   any --Failed---------> same state, with the error shown until the next success
//! ```
//!
//! Folding (decision 0068): after every load, change batch and plan update the view
//! refolds ([`Chunks::refresh`]); a click expands a folded node or folds the selected
//! one; notices stack on what is drawn as badges, and their total never changes with the fold.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Color32, FontId, Rect};

use crate::badges;
use crate::budget::RenderBudget;
use crate::chunks::Chunks;
use crate::feed::{Feed, FeedEvent, FeedOptions};
use crate::labels::{self, Label};
use crate::palette::Palette;
use crate::plan::PlanNode;
use crate::project::{ApplyReport, ProjectModel};
use crate::view::GalaxyView;
use std::collections::HashMap;

/// The last load and the last change batch, for the status line and `--measure`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectTimings {
    /// Reading the snapshot from SQLite.
    pub snapshot_read: Option<Duration>,
    /// Building the model and scene from it.
    pub scene_build: Option<Duration>,
    /// Applying the last change batch to the scene.
    pub last_apply: Option<Duration>,
    pub last_report: ApplyReport,
    pub batches: u64,
}

struct Live {
    model: ProjectModel,
    view: GalaxyView,
}

enum State {
    Opening,
    Unindexed(String),
    /// Boxed: the loaded state is large and the others are a string at most.
    Live(Box<Live>),
}

/// A project's galaxy panel.
/// A warning the view says on the stage even with its HUD hidden (fail loud).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub text: String,
    pub kind: WarningKind,
}

/// What a warning means, which sets its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningKind {
    /// Broken data (COL-7): a refused cycle, a lease on a path the index does not know.
    Broken,
    /// A state, not a fault (COL-5): the draw is over the budget.
    Neutral,
}

/// The warnings for these counts, in a fixed order. Pure.
pub fn warnings_of(
    unindexed_leases: usize,
    over_budget: Option<(usize, usize)>,
    refused_contains: usize,
    refused_work_parents: usize,
) -> Vec<Warning> {
    let mut out = Vec::new();
    let mut push = |kind, text: String| out.push(Warning { text, kind });
    if unindexed_leases > 0 {
        push(
            WarningKind::Broken,
            format!("{unindexed_leases} lease(s) on unindexed paths"),
        );
    }
    if let Some((drawn, budget)) = over_budget {
        push(
            WarningKind::Neutral,
            format!("drawn {drawn}, over budget {budget}: fold something to get back under"),
        );
    }
    if refused_contains > 0 {
        push(
            WarningKind::Broken,
            format!("{refused_contains} contains edge(s) refused as cycles"),
        );
    }
    if refused_work_parents > 0 {
        push(
            WarningKind::Broken,
            format!("{refused_work_parents} work parent(s) refused as cycles"),
        );
    }
    out
}

/// How a warning is painted: `(fill, text)`. Broken data is an orange
/// `stage_danger` pill with dark text, the dark-icon rule of STG-DRAW-11 (COL-7);
/// a neutral state is gray text with no fill (COL-5). Never yellow: nothing here needs
/// the user's input (COL-3).
pub fn warning_look(kind: WarningKind, p: &Palette) -> (Option<Color32>, Color32) {
    match kind {
        WarningKind::Broken => (Some(p.danger), p.pill_text),
        WarningKind::Neutral => (None, p.hud),
    }
}

/// Where the warnings start below the stage's top edge by default, in points. The shell
/// sets its own ([`ProjectView::set_overlay_top`]) below its selection title.
pub const OVERLAY_TOP: f32 = 8.0;

pub struct ProjectView {
    root: PathBuf,
    budget: RenderBudget,
    feed: Feed,
    state: State,
    /// The last feed or apply failure, shown until the next successful event.
    error: Option<String>,
    timings: ProjectTimings,
    /// The plan overlay as last given; reapplied after a reload replaces the model.
    plan: Vec<PlanNode>,
    /// Counts changes to the model, so a reader's derived index knows when to
    /// rebuild (the search index).
    generation: u64,
    /// Per agent, keyed by lease holder without its `@nonce`, what its label adds:
    /// its current action and pending approvals. Set by the shell from the
    /// transcripts and the box.
    agent_notes: HashMap<String, String>,
    /// Code slots, largest first, and the generation they were sorted at.
    code_by_size: (u64, Vec<u32>),
    /// The fold: the automatic cut and the user's expansions, kept across reloads.
    chunks: Chunks,
    /// The fold handles and pills drawn last frame, in the order given to the view as
    /// hot zones: a click in zone `i` does `marks[i].action`.
    marks: Vec<badges::Mark>,
    /// Every color the galaxy draws ([`Self::set_palette`]).
    palette: Palette,
    /// Whether the stage HUD (the status line and the scene counts) is drawn.
    hud: bool,
    /// Where the warnings start below the stage's top edge, in points.
    overlay_top: f32,
}

impl std::fmt::Debug for ProjectView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectView")
            .field("root", &self.root)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl ProjectView {
    /// Starts the feed for `root`; `ctx` is woken whenever it has news.
    pub fn open(root: PathBuf, budget: RenderBudget, ctx: &egui::Context) -> std::io::Result<Self> {
        Self::open_with(root, budget, FeedOptions::default(), ctx)
    }

    /// [`Self::open`] with feed options (tests poll faster).
    pub fn open_with(
        root: PathBuf,
        budget: RenderBudget,
        options: FeedOptions,
        ctx: &egui::Context,
    ) -> std::io::Result<Self> {
        let wake_ctx = ctx.clone();
        let feed = Feed::spawn(root.clone(), options, move || wake_ctx.request_repaint())?;
        Ok(Self {
            root,
            budget,
            feed,
            state: State::Opening,
            error: None,
            timings: ProjectTimings::default(),
            plan: Vec::new(),
            generation: 0,
            agent_notes: HashMap::new(),
            code_by_size: (u64::MAX, Vec::new()),
            chunks: Chunks::default(),
            marks: Vec::new(),
            palette: Palette::default(),
            hud: true,
            overlay_top: OVERLAY_TOP,
        })
    }

    /// Replaces every color the galaxy draws: the scene's (code dots, edges, proxies,
    /// agents, work items, the selection's relations) and the overlay's (labels, pills,
    /// handles, HUD, errors). The shell hands in its run-time palette here.
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        if let State::Live(live) = &mut self.state {
            let Live { model, view } = live.as_mut();
            if let Err(e) = model.set_palette(view.galaxy_mut(), palette) {
                self.error = Some(format!("recoloring the galaxy: {e}"));
            }
            view.set_palette(&palette);
        }
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Shows or hides the stage HUD: the status line (the project path, the agent and
    /// work counts, the selection's description) and the scene counts. The shell hides
    /// it where its own project row says the same. Errors are always shown.
    /// Where the warnings drawn with the HUD hidden start, in points below the stage's
    /// top edge: the shell passes the bottom of what it draws there (its selection
    /// title), so the two never overlap.
    pub fn set_overlay_top(&mut self, top: f32) {
        self.overlay_top = top;
    }

    pub fn set_hud(&mut self, on: bool) {
        self.hud = on;
        if let State::Live(live) = &mut self.state {
            live.view.set_hud(on);
        }
    }

    /// The fold state.
    pub fn chunks(&self) -> &Chunks {
        &self.chunks
    }

    /// What the stage's pills sum to, whatever the fold: failures plus needs-input
    /// items (approvals, nodes held for the user), each once on its work node
    /// ([`crate::fold::Tally::attention`]). Not the bell: the shell's masthead bell
    /// is the one count, over every open project and over items no node carries (a
    /// blocked run, unreadable records); decision 0068 lists where the two differ.
    /// `None` before a load.
    pub fn stage_tally(&self) -> Option<crate::fold::Tally> {
        let State::Live(live) = &self.state else {
            return None;
        };
        let badges = self.chunks.badges(&live.model, live.view.galaxy());
        Some(crate::fold::total(&badges))
    }

    /// The fold handles and pills drawn last frame (click targets, in points).
    pub fn marks(&self) -> &[badges::Mark] {
        &self.marks
    }

    /// The status line: the project, the overlay counts, what is wrong, and the
    /// selection. Says so when the fold draws more than its budget, or when the data
    /// had a `contains` cycle (fail loud).
    pub fn status_line(&self) -> Option<String> {
        let State::Live(live) = &self.state else {
            return None;
        };
        let Live { model, view } = live.as_ref();
        let (agents, work) = model.overlay_counts();
        let mut line = format!(
            "{} | {agents} agent(s), {work} work item(s)",
            self.root.display()
        );
        for w in self.warnings() {
            line.push_str(" | ");
            line.push_str(&w.text);
        }
        if let Some(sel) = view.galaxy().selection()
            && let Some(text) = model.describe(sel.selected())
        {
            line.push_str(&format!("\n{text}"));
        }
        Some(line)
    }

    /// What the view must say even with the HUD hidden (fail loud): activity
    /// on unindexed paths, a draw over the budget, and refused cycles. Empty when all
    /// is well.
    pub fn warnings(&self) -> Vec<Warning> {
        let State::Live(live) = &self.state else {
            return Vec::new();
        };
        let model = &live.model;
        warnings_of(
            model.unindexed_activity(),
            self.chunks.over_budget(),
            model.refused_contains(),
            model.refused_work_parents(),
        )
    }

    /// Refolds the loaded galaxy from the model.
    fn refresh_fold(&mut self, animate: bool) {
        if let State::Live(live) = &mut self.state {
            let Live { model, view } = live.as_mut();
            self.chunks.refresh(model, view.galaxy_mut(), animate);
        }
    }

    /// Sets what each agent's label adds (its action, pending approvals).
    pub fn set_agent_notes(&mut self, notes: HashMap<String, String>) {
        self.agent_notes = notes;
    }

    /// The labels for this frame: work nodes and agents first, then the largest
    /// code nodes, ported from codebase-memory-mcp's `NodeLabels` (see `labels.rs`).
    fn labels(&mut self) -> Vec<Label> {
        let State::Live(live) = &self.state else {
            return Vec::new();
        };
        let Live { model, view } = live.as_ref();
        let galaxy = view.galaxy();
        if self.code_by_size.0 != self.generation {
            let scene = galaxy.scene();
            let mut slots: Vec<u32> = model.code_nodes().map(|(s, ..)| s).collect();
            slots.sort_by(|&a, &b| {
                scene.nodes()[b as usize]
                    .size
                    .total_cmp(&scene.nodes()[a as usize].size)
                    .then(a.cmp(&b))
            });
            self.code_by_size = (self.generation, slots);
        }
        let agents: Vec<(&str, u32)> = model.agents().collect();
        let mut overlay: Vec<u32> = model.plan_nodes().map(|(_, s)| s).collect();
        overlay.extend(agents.iter().map(|(_, s)| *s));
        // Proxies name the chunks, so they are labeled before code: the ones hiding the
        // most first.
        let fold = self.chunks.fold();
        let mut clusters: Vec<(u32, u32)> = model
            .proxies()
            .map(|(s, _)| (u32::MAX - fold.hidden_below(s), s))
            .collect();
        clusters.sort_unstable();
        // STG-DRAW-10: the short name below 2x the fit scale, the full name above.
        let full_names = galaxy.zoom_over_fit() >= labels::FULL_NAME_ZOOM;
        let (over, code) = labels::choose(
            galaxy,
            &overlay,
            clusters
                .iter()
                .map(|&(_, s)| s)
                .chain(self.code_by_size.1.iter().copied()),
            galaxy.budget().label_cap(),
        );
        let mut out = Vec::with_capacity(over.len() + code.len());
        for slot in over {
            if let Some(n) = model.plan_at(slot) {
                out.push(Label {
                    slot,
                    text: n.label.clone(),
                    color: self.palette.text,
                    overlay: true,
                    group: false,
                });
            } else if let Some(holder) = model.agent_at(slot) {
                // Notes are keyed by the holder without its process nonce.
                let key = holder.rsplit_once('@').map_or(holder, |(h, _)| h);
                let text = match self.agent_notes.get(key) {
                    Some(note) => format!("{holder}: {note}"),
                    None => holder.to_owned(),
                };
                out.push(Label {
                    slot,
                    text,
                    color: self.palette.text,
                    overlay: true,
                    group: false,
                });
            }
        }
        for slot in code {
            if let Some(p) = model.proxy_at(slot) {
                out.push(Label {
                    slot,
                    text: if full_names {
                        p.full.clone()
                    } else {
                        p.short.clone()
                    },
                    color: self.palette.group_label,
                    overlay: false,
                    group: true,
                });
                continue;
            }
            let Some(qual) = model.qualified_name(slot) else {
                continue;
            };
            let name = qual.rsplit("::").next().unwrap_or(qual);
            out.push(Label {
                slot,
                text: name.to_owned(),
                color: self.palette.text,
                overlay: false,
                group: false,
            });
        }
        out
    }

    /// Increases whenever the model changes (a load, a change batch, a plan).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Sets the plan overlay (work status from the run records). Applied now when
    /// the project is loaded, and again after every reload.
    pub fn set_plan(&mut self, nodes: Vec<PlanNode>) {
        if nodes == self.plan {
            return;
        }
        self.plan = nodes;
        self.apply_plan();
    }

    /// Folds a session graph into the model (decision 0067): a session
    /// diamond for a lease-less session, `spawned` and `connection` overlay edges, and
    /// a subagent's fold placement. The caller (the shell's `session_feed`) already
    /// gates the read on the change log's watermark, so every call here is a real
    /// change; unlike [`Self::set_plan`] there is no cheap equality check to skip one
    /// (`SessionGraph` carries no `PartialEq`, by design: it is a read, not a value
    /// two frames should ever compare).
    pub fn set_sessions(&mut self, graph: &kgraf_graph::sessions::SessionGraph) {
        self.generation += 1;
        if let State::Live(live) = &mut self.state {
            let Live { model, view } = live.as_mut();
            if let Err(e) = model.set_sessions(view.galaxy_mut(), graph) {
                self.error = Some(format!("drawing sessions: {e}"));
            }
        }
        // Subagents may have joined or left the fold hierarchy: refold.
        self.refresh_fold(true);
    }

    fn apply_plan(&mut self) {
        self.generation += 1;
        if let State::Live(live) = &mut self.state {
            let Live { model, view } = live.as_mut();
            if let Err(e) = model.set_plan(view.galaxy_mut(), &self.plan) {
                self.error = Some(format!("drawing the work status: {e}"));
            }
        }
        // Work nodes joined or left the hierarchy: refold.
        self.refresh_fold(true);
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The galaxy, once the project is loaded.
    pub fn view(&self) -> Option<&GalaxyView> {
        match &self.state {
            State::Live(live) => Some(&live.view),
            _ => None,
        }
    }

    pub fn view_mut(&mut self) -> Option<&mut GalaxyView> {
        match &mut self.state {
            State::Live(live) => Some(&mut live.view),
            _ => None,
        }
    }

    pub fn model(&self) -> Option<&ProjectModel> {
        match &self.state {
            State::Live(live) => Some(&live.model),
            _ => None,
        }
    }

    /// Why the project is not shown, when it has no readable index.
    pub fn unindexed(&self) -> Option<&str> {
        match &self.state {
            State::Unindexed(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn timings(&self) -> ProjectTimings {
        self.timings
    }

    /// Applies every waiting feed message. Returns how many were applied.
    pub fn drain(&mut self, render_state: &egui_wgpu::RenderState) -> usize {
        let mut n = 0;
        while let Some(event) = self.feed.try_recv() {
            self.handle(event, render_state);
            n += 1;
        }
        n
    }

    fn handle(&mut self, event: FeedEvent, render_state: &egui_wgpu::RenderState) {
        match event {
            FeedEvent::Unindexed { reason } => {
                self.error = None;
                self.state = State::Unindexed(reason);
            }
            FeedEvent::Loaded { snapshot, read } => {
                let t0 = Instant::now();
                match ProjectModel::build_with(&snapshot, self.palette) {
                    Ok((model, scene)) => {
                        self.generation += 1;
                        self.timings.snapshot_read = Some(read);
                        self.timings.scene_build = Some(t0.elapsed());
                        self.error = None;
                        let state = std::mem::replace(&mut self.state, State::Opening);
                        self.state = match state {
                            State::Live(mut live) => {
                                live.view.set_scene(scene);
                                live.model = model;
                                State::Live(live)
                            }
                            State::Opening | State::Unindexed(_) => {
                                let mut view = GalaxyView::new(render_state, scene, self.budget);
                                view.galaxy_mut().frame_all();
                                view.set_palette(&self.palette);
                                view.set_hud(self.hud);
                                State::Live(Box::new(Live { model, view }))
                            }
                        };
                        // A new model numbers its hierarchy afresh: the cached cut is
                        // stale whatever its generation says. Folded at once, not
                        // animated: a load is not a change the user watches.
                        self.chunks.forget_cut();
                        self.refresh_fold(false);
                        self.apply_plan();
                    }
                    Err(e) => self.error = Some(format!("building the scene: {e}")),
                }
            }
            FeedEvent::Changes { batch, .. } => {
                if let State::Live(live) = &mut self.state {
                    let t0 = Instant::now();
                    match live.model.apply(live.view.galaxy_mut(), &batch) {
                        Ok(report) => {
                            self.generation += 1;
                            let Live { model, view } = live.as_mut();
                            self.chunks.refresh(model, view.galaxy_mut(), true);
                            self.timings.last_apply = Some(t0.elapsed());
                            self.timings.last_report = report;
                            self.timings.batches += 1;
                            self.error = (report.edges_skipped > 0).then(|| {
                                format!(
                                    "{} edge(s) named a node the scene does not hold",
                                    report.edges_skipped
                                )
                            });
                        }
                        Err(e) => {
                            // A batch that failed half way leaves the model and scene
                            // out of step; a fresh snapshot is the only sound repair.
                            self.error = Some(format!("applying a change (reloading): {e}"));
                            self.feed.request_reload();
                        }
                    }
                }
            }
            FeedEvent::Failed(e) => self.error = Some(e),
        }
    }

    /// Fills the available space with the galaxy, or with the reason there is none.
    pub fn show(&mut self, ui: &mut egui::Ui, render_state: &egui_wgpu::RenderState) {
        self.drain(render_state);
        let rect = ui.available_rect_before_wrap();
        let palette = self.palette;
        match &mut self.state {
            State::Opening => {
                center_text(
                    ui,
                    rect,
                    &format!("Opening {}", self.root.display()),
                    palette.hud,
                );
            }
            State::Unindexed(reason) => {
                center_text(
                    ui,
                    rect,
                    &format!(
                        "{}\n\n{reason}\n\nThis view loads the index when it appears.",
                        self.root.display()
                    ),
                    palette.notice,
                );
            }
            State::Live(live) => {
                live.view.show(ui, render_state);
                if let Some(click) = live.view.take_click() {
                    let Live { model, view } = live.as_mut();
                    let acted = match click.zone.and_then(|z| self.marks.get(z)) {
                        Some(m) => {
                            match m.action {
                                badges::MarkAction::Toggle(t) => {
                                    self.chunks.toggle(model, view.galaxy_mut(), t);
                                }
                                badges::MarkAction::Select(s) => view.galaxy_mut().select(Some(s)),
                            }
                            true
                        }
                        None => self.chunks.click(model, view.galaxy_mut(), click).is_some(),
                    };
                    if acted {
                        // The fold animates from the next frame on.
                        ui.ctx().request_repaint();
                    }
                }
                let labels = self.labels();
                let line = self.status_line().unwrap_or_default();
                let warnings = self.warnings();
                let State::Live(live) = &mut self.state else {
                    return;
                };
                let Live { model, view } = live.as_mut();
                if let Some(r) = view.last_rect() {
                    labels::draw(ui, r, view.galaxy(), &labels, palette.text_halo);
                    let galaxy = view.galaxy();
                    badges::draw_selection(ui, r, galaxy, &palette);
                    badges::draw_actors(ui, r, galaxy, &actor_marks(model), &palette);
                    let mut marks = badges::draw_fold_marks(
                        ui,
                        r,
                        galaxy,
                        self.chunks.fold(),
                        view.hovered(),
                        &palette,
                    );
                    let carried = self.chunks.badges(model, galaxy);
                    let name_of = |s: u32| {
                        model
                            .plan_at(s)
                            .map(|n| n.key.clone())
                            .or_else(|| model.fold_key(s))
                            .unwrap_or_else(|| format!("node {s}"))
                    };
                    marks.extend(badges::draw(ui, r, galaxy, &carried, &palette, &name_of));
                    view.set_hot_zones(marks.iter().map(|m| (m.rect, m.owner)).collect());
                    self.marks = marks;
                }
                if self.hud {
                    ui.painter().text(
                        rect.left_top() + egui::vec2(8.0, 8.0),
                        egui::Align2::LEFT_TOP,
                        line,
                        FontId::monospace(11.0),
                        palette.hud,
                    );
                } else {
                    // The HUD is hidden (the shell's rows say the routine counts), but
                    // a warning is still said on the stage: top center, one
                    // per line, from `overlay_top` down.
                    let painter = ui.painter();
                    let mut y = rect.top() + self.overlay_top;
                    for w in &warnings {
                        let (fill, text) = warning_look(w.kind, &palette);
                        let galley = painter.layout_no_wrap(
                            w.text.clone(),
                            FontId::proportional(12.0),
                            text,
                        );
                        let size = galley.size() + egui::vec2(16.0, 6.0);
                        let r = Rect::from_min_size(
                            egui::pos2(rect.center().x - size.x / 2.0, y),
                            size,
                        );
                        if let Some(fill) = fill {
                            painter.rect_filled(r, size.y / 2.0, fill);
                        }
                        painter.galley(r.min + egui::vec2(8.0, 3.0), galley, text);
                        y += size.y + 4.0;
                    }
                }
            }
        }
        if let Some(err) = &self.error {
            // Fail loud: the panel says what went wrong.
            ui.painter().text(
                rect.center_bottom() + egui::vec2(0.0, -28.0),
                egui::Align2::CENTER_BOTTOM,
                err,
                FontId::proportional(13.0),
                palette.error,
            );
        }
    }
}

/// Each agent's letter and state for its disk (STG-DRAW-7): the state is the most
/// urgent of its tasks' (failed, then needs input, then working), except that nothing
/// overrides a failure. Covers both lease-holding agents (`model.agents()`) and
/// lease-less sessions (`model.session_overlay()`, decision 0067), so a
/// subagent or a worker waiting for a slot gets the same dot a lease-holder does.
fn actor_marks(model: &ProjectModel) -> Vec<(u32, char, badges::ActorState, bool)> {
    let mut out = Vec::new();
    let overlay = model.session_overlay();
    for (holder, slot) in model.agents().chain(overlay.slots()) {
        let is_resolver = overlay.is_resolver(holder);
        let name = holder.rsplit_once('@').map_or(holder, |(h, _)| h);
        let name = name.rsplit('/').next().unwrap_or(name);
        // A resolver's letter is always "R" (style guide CMP-AV-6), never the holder's own initial:
        // the mark must read apart from an ordinary actor at a glance, not merely by shape.
        let letter = if is_resolver {
            'R'
        } else {
            name.chars()
                .find(|c| c.is_alphanumeric())
                .map_or('?', |c| c.to_ascii_uppercase())
        };
        let mut state = badges::ActorState::Working;
        for task in model.agent_tasks(holder) {
            let Some(n) = model.task_slot(task).and_then(|s| model.plan_at(s)) else {
                continue;
            };
            if n.state.is_failed() {
                state = badges::ActorState::Failed;
                break;
            }
            if n.asks > 0 || n.state == crate::plan::WorkState::HeldForUser {
                state = badges::ActorState::NeedsInput;
            }
        }
        // A session the orchestrator's
        // pacer is delaying carries the soft-limit notice on this same dot, but gray
        // (COL-5, a state and not a fault), never yellow -- yellow means "needs your
        // input" and feeds the project dot, hamburger and bell aggregates, and pacing
        // needs nothing from the user. Never overrides a real `Failed` or `NeedsInput`
        // state: pacing is the least urgent thing this dot shows.
        if state == badges::ActorState::Working && overlay.is_soft_limited(holder) {
            state = badges::ActorState::Paced;
        }
        out.push((slot, letter, state, is_resolver));
    }
    out
}

fn center_text(ui: &mut egui::Ui, rect: Rect, text: &str, color: Color32) {
    ui.allocate_rect(rect, egui::Sense::hover());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(15.0),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Refused cycles and leases on unindexed paths are broken data; over the budget is
    /// a state. COL-7 and COL-5.
    #[test]
    fn each_warning_has_its_kind() {
        use WarningKind::{Broken, Neutral};
        let kinds: Vec<WarningKind> = warnings_of(1, Some((1_200, 1_000)), 2, 3)
            .iter()
            .map(|w| w.kind)
            .collect();
        assert_eq!(kinds, [Broken, Neutral, Broken, Broken]);
        assert!(warnings_of(0, None, 0, 0).is_empty());
    }

    /// Broken data is an orange pill with dark text (STG-DRAW-11); a neutral state is
    /// gray text. Nothing is yellow (COL-3).
    #[test]
    fn warning_colors_follow_col_3_and_col_7() {
        let p = Palette::default();
        assert_eq!(
            warning_look(WarningKind::Broken, &p),
            (Some(p.danger), p.pill_text)
        );
        assert_eq!(warning_look(WarningKind::Neutral, &p), (None, p.hud));
        for k in [WarningKind::Broken, WarningKind::Neutral] {
            let (fill, text) = warning_look(k, &p);
            assert_ne!(fill, Some(p.notice));
            assert_ne!(text, p.notice);
        }
    }

    /// Decision 0067: a session the
    /// orchestrator's pacer is delaying draws its soft-limit notice on the actor's own
    /// state dot (STG-DRAW-7), a gray ring (COL-5: a state, not a fault) -- never
    /// yellow, since yellow means "needs your input" and feeds `stage_tally` (the
    /// project's aggregate dot, hamburger and bell). A real needs-input item (a plan
    /// node held for the user) is present too, so the test proves the paced session
    /// adds nothing to that count, not merely that the count started at zero.
    #[test]
    fn a_paced_session_is_drawn_gray_and_never_counts_toward_the_aggregate_dot() {
        use kgraf_core::AgentId;
        use kgraf_core::session::{ReasoningVisibility, SessionFacts, SessionKind};
        use kgraf_graph::sessions::SessionGraph;
        use kgraf_graph::snapshot::GraphSnapshot;

        fn session(id: &str) -> SessionFacts {
            SessionFacts {
                id: AgentId::new(id),
                run: kgraf_core::RunId::new("run-1"),
                kind: SessionKind::Worker,
                parent: None,
                contract: None,
                route: None,
                model: None,
                depth: 0,
                transcript: None,
                worktree: None,
                write_scope: Vec::new(),
                reasoning: ReasoningVisibility::Unknown,
                started_ms: 0,
                addressable: true,
            }
        }

        let (mut model, mut scene) = ProjectModel::build(&GraphSnapshot::default()).expect("build");
        model
            .set_plan(
                &mut scene,
                &[crate::plan::PlanNode {
                    key: "c".into(),
                    state: crate::plan::WorkState::HeldForUser,
                    label: "c: held for you".into(),
                    blocked_by: Vec::new(),
                    asks: 0,
                    parent: None,
                }],
            )
            .expect("plan");
        let before = model.notices();
        assert_eq!(before.len(), 1, "the plan node's own needs-input notice");

        let graph = SessionGraph {
            nodes: vec![session("run/paced@1"), session("run/quiet@1")],
            spawned: Vec::new(),
            connections: Vec::new(),
            paced: std::collections::BTreeSet::from([AgentId::new("run/paced@1")]),
        };
        model
            .set_sessions(&mut scene, &graph)
            .expect("set_sessions");

        // The aggregate the shell's dot, hamburger and bell sum (`stage_tally`) reads
        // exactly these notices: unchanged by a paced session appearing.
        let after = model.notices();
        assert_eq!(
            after, before,
            "a paced session adds nothing to the needs-input aggregate"
        );

        let marks = actor_marks(&model);
        let paced_slot = model
            .session_overlay()
            .slot_of("run/paced@1")
            .expect("a slot");
        let quiet_slot = model
            .session_overlay()
            .slot_of("run/quiet@1")
            .expect("a slot");
        let state_of = |slot: u32| {
            marks
                .iter()
                .find(|(s, ..)| *s == slot)
                .map(|(_, _, state, _)| *state)
                .expect("drawn")
        };
        assert_eq!(
            state_of(paced_slot),
            badges::ActorState::Paced,
            "gray, not yellow"
        );
        assert_ne!(state_of(paced_slot), badges::ActorState::NeedsInput);
        assert_eq!(state_of(quiet_slot), badges::ActorState::Working);
    }
}
