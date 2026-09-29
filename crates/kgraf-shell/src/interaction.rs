//! Interaction, wired into the shell: the orchestrator box, the stored runs
//! it and the graph read, play and the user's order, and the work status the graph
//! draws.
//!
//! One struct owns the interaction state so `app.rs` only calls into it. It is made
//! on the first frame, when the egui context exists for the wake functions every
//! background part calls (the UI repaints only on change, 0054).
//!
//! Nothing here is a second work surface: the box is the
//! orchestrator's channel, the order is set by selecting nodes on the graph and
//! shown there as `#n` on each node, and everything about work state is drawn on the
//! graph. No list of work items exists anywhere.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kgraf_core::RunId;
use kgraf_galaxy::PlanNode;
use kgraf_orchestrator::{Admission, StoredRun};

use crate::chrome;
use crate::orchestrator_box::{self, BackendState, BoxBackend, BoxModel, Line};
use crate::orchestrator_host::{HostParts, NO_HOST, OrchestratorHost};
use crate::run_view;
use crate::run_watch::{POLL, RunWatch, RunsUpdate};
use crate::search::{self, ConvSource, Index, Results};
use crate::settings::ParallelismSlider;
use crate::text_surface::{self, TextSurface};
use crate::theme;
use crate::transcripts::{self, Corpus, TranscriptWatch};

/// The stored runs as last read.
#[derive(Debug, Default)]
pub struct Runs {
    pub runs: Vec<StoredRun>,
    /// Runs whose lease a live process holds now (another window, the CLI).
    pub leased: std::collections::BTreeSet<String>,
    /// Why the store could not be read, until it can.
    pub error: Option<String>,
    /// The presumed work nodes (decision 0064) with their derived states.
    pub presumed: Vec<kgraf_interpret::PresumedView>,
    /// Why the presumed nodes could not be read, until they can.
    pub presumed_error: Option<String>,
}

/// What Play could resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resumable {
    /// This run: marked for resume, not completed, and held by no live process.
    Run(String),
    /// Runs marked for resume exist, but a live process holds each.
    HeldElsewhere(Vec<String>),
    None,
}

/// What Play would resume from the stored runs. A completed run is never offered
/// (resume refuses it: `OrchestratorError::AlreadyCompleted`), nor one whose lease
/// a live process holds; the newest remaining one is.
pub fn resumable(runs: &Runs) -> Resumable {
    let candidates: Vec<&kgraf_orchestrator::RunStateRecord> = runs
        .runs
        .iter()
        .filter(|r| r.active)
        .filter_map(|r| r.record.as_ref().ok())
        .filter(|r| r.status != kgraf_orchestrator::RunStatus::Completed)
        .collect();
    let free = candidates
        .iter()
        .filter(|r| !runs.leased.contains(r.run.as_str()))
        .max_by_key(|r| r.updated_ms);
    match free {
        Some(r) => Resumable::Run(r.run.to_string()),
        None if !candidates.is_empty() => {
            Resumable::HeldElsewhere(candidates.iter().map(|r| r.run.to_string()).collect())
        }
        None => Resumable::None,
    }
}

/// The user's order after adding `add` (0014): the current order with the
/// contracts no longer open dropped (a verified one cannot be ordered, and the
/// orchestrator refuses the whole order if it names one), then `add`.
pub fn next_order(current: &[String], open: &[&str], add: &str) -> Vec<String> {
    let mut order: Vec<String> = current
        .iter()
        .filter(|c| open.contains(&c.as_str()))
        .cloned()
        .collect();
    if !order.iter().any(|c| c == add) {
        order.push(add.to_owned());
    }
    order
}

/// What is selected on the graph, as the interaction needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selected {
    /// The selected slot, if any.
    pub slot: Option<u32>,
    /// A lease holder (an agent node).
    pub agent: Option<String>,
    /// A plan node's key (a contract or a gate).
    pub work: Option<String>,
}

/// What the play control does now (0014: play hands scheduling to the
/// orchestrator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayAction {
    /// Resume this stored run in this window.
    Resume(String),
    /// Stop the hosted run; its record stays resumable.
    Stop,
    /// Nothing to do; the reason is the control's hover text.
    Disabled(String),
}

/// The play control's action. Pure.
pub fn play_action(state: &BackendState, resumable: &Resumable) -> PlayAction {
    match state {
        BackendState::Running { .. } => PlayAction::Stop,
        BackendState::Starting => PlayAction::Disabled("the run is starting".to_owned()),
        BackendState::Idle {
            can_start: false,
            reason,
        } => PlayAction::Disabled(reason.clone()),
        BackendState::Idle {
            can_start: true, ..
        } => match resumable {
            Resumable::Run(run) => PlayAction::Resume(run.clone()),
            Resumable::HeldElsewhere(runs) => PlayAction::Disabled(format!(
                "run {} is held by another live process (its run lease); Play resumes only a \
                 run nobody holds",
                runs.join(", ")
            )),
            Resumable::None => PlayAction::Disabled(
                "no stopped run to resume: start one with /run <instruction> in the box".to_owned(),
            ),
        },
    }
}

/// Apply the parallelism-derived limits to one project's admission (decision
/// 0067). Work already admitted keeps running;
/// the next admission sees the new limits. `limits` is always in range by
/// construction (`Limits::new` already validated it), so there is nothing
/// left here to refuse.
///
/// `ConcurrencyCaps` used to fill this role and is retired as a user-facing
/// control (design, "Consequences": "The work-item cap as a user control is
/// gone"): every open project's own local ceiling now simply tracks the
/// slider's effective value instead of a fixed default, so raising the
/// slider really does raise what one project alone may hold, and the shared
/// `MachineGate` (not this per-project number) is what keeps several
/// projects together from exceeding it.
pub fn apply_parallelism(limits: kgraf_orchestrator::Limits, admission: &Admission) {
    admission.gate().set_limits(limits);
}

/// The parallelism slider (decision 0067): one number, agent sessions
/// live at once process-wide, with a per-machine maximum and its breakdown.
/// Settings is configuration only (0017): the breakdown states the
/// current bound and its source, never a readout of running work.
///
/// STYLE_GUIDE rules cited: CMP-SET-1 (a setting row's label and tabular-numeral
/// value), CMP-SET-2 (the slider itself), CMP-SET-3 (the machine maximum
/// shown as a legend, with a "Return to maximum" action once pinned away
/// from it, exactly as a slider returns to its recommendation), CMP-SET-6 (a
/// note with `--additive` fill for stating a fixed fact, the breakdown
/// states what bound the machine's maximum, not a warning). Colors come
/// only from `crate::theme::Palette` (`kgraf_core::palette::Semantic`
/// underneath it), never a hand-picked value.
pub fn parallelism_controls(
    ui: &mut egui::Ui,
    p: &crate::theme::Palette,
    slider: &mut ParallelismSlider,
    breakdown: Option<&kgraf_capacity::Capacity>,
    hosting: bool,
    max_attempts: Option<usize>,
) {
    ui.label("Parallelism (agent sessions live at once)");
    let max = slider.max();
    // The widget's own range must cover a pinned value that now exceeds the
    // maximum (CMP-SET-2 does not itself say what happens above the range;
    // this crate's own rule is: never hide the user's own number, state the
    // clamp instead, per `clamp_note` below).
    let hi = max.max(slider.value()).max(1);
    let mut value = slider.value();
    let changed = ui.add(egui::Slider::new(&mut value, 1..=hi)).changed();
    if changed {
        slider.set_by_user(value);
    }
    // CMP-SET-3: the maximum is the "recommendation" a parallelism slider
    // follows; stated as a legend, with a return action once pinned away
    // from it.
    ui.label(
        egui::RichText::new(format!("Machine maximum: {max}"))
            .small()
            .color(p.text_2),
    );
    if !slider.follows_max() && chrome::text_button(ui, p.primary, "Return to maximum").clicked() {
        slider.return_to_max();
    }
    if let Some(note) = slider.clamp_note() {
        ui.colored_label(p.danger, note);
    }
    // A live-agent ceiling below a contract's parallel attempts would hold
    // that contract forever. Settings is configuration only (0017): it
    // states the cap fact alone; which contract cannot fit is said on its node.
    // `ConcurrencyCaps::live_agents()` used to be the number compared here;
    // `effective()` is the slider's own equivalent (item 5).
    if let Some(attempts) = max_attempts
        && (slider.effective() as usize) < attempts
    {
        ui.colored_label(
            p.danger,
            format!("Below the largest attempt count: {attempts}"),
        );
    }
    let note = if hosting {
        "Binds every run this window hosts, from the next dispatch."
    } else {
        "Binds runs this window hosts; with no project open it hosts none."
    };
    ui.label(egui::RichText::new(note).weak());
    if let Some(capacity) = breakdown {
        // CMP-SET-6: a note stating a fixed fact (which bound currently
        // binds, and why), `--additive` fill.
        egui::Frame::new()
            .fill(p.additive)
            .corner_radius(theme::radius::CARD)
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Capacity breakdown")
                        .small()
                        .color(p.text_2),
                );
                for bound in &capacity.bounds {
                    let source = match bound.source {
                        kgraf_capacity::LimitSource::Header => "header",
                        kgraf_capacity::LimitSource::Configured => "configured",
                        kgraf_capacity::LimitSource::Learned => "learned",
                        kgraf_capacity::LimitSource::Unmeasured => "unmeasured",
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "{}: {} ({source}): {}",
                            bound.name, bound.value, bound.detail
                        ))
                        .small()
                        .color(p.text_2),
                    );
                }
            });
    }
}

/// The search field's hint.
pub const SEARCH_HINT: &str =
    "symbol, agent:, task:, status:, file:, \"phrase\", before:/after: (file time)";

/// The search field's hover help. `before:` and `after:` bound a transcript by
/// its file's last write, because transcript events carry no time.
pub const SEARCH_HELP: &str = "Operators: agent:, symbol:, file:, task: or contract:, \
touched:, status: (done, running, blocked, held, cap, lease, waiting, planned, gate), \
vendor: or model:, before: and after: (YYYY-MM-DD). before: and after: compare each \
transcript's file modification time, not the time of a turn: transcript events carry no \
time. Quote a phrase; operators do not work inside quotes.";

/// Debounce before a typed query runs.
pub const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// The count line under the search field. Says when a count was cut.
pub fn results_line(r: &Results, running: bool) -> String {
    let mut text = format!("{} lit", r.hits.len());
    if r.hits_dropped > 0 {
        text.push_str(&format!(
            " (the best {} of {}; {} more not lit)",
            r.hits.len(),
            r.hits.len() + r.hits_dropped,
            r.hits_dropped
        ));
    }
    if !r.turns.is_empty() {
        text.push_str(&format!(", {} turn(s) in the text panel", r.turns.len()));
    }
    if r.turns_dropped > 0 {
        text.push_str(&format!(" ({} older turn(s) not listed)", r.turns_dropped));
    }
    if !r.briefs.is_empty() {
        text.push_str(&format!(", {} brief(s)", r.briefs.len()));
    }
    if r.unplaced > 0 {
        text.push_str(&format!(
            ", {} match(es) with no node on the graph",
            r.unplaced
        ));
    }
    if running {
        text.push_str(", searching");
    }
    text
}

/// The search field's state (0032).
#[derive(Debug, Default)]
pub struct SearchState {
    pub query: String,
    /// The query the results are for.
    ran: String,
    pub error: Option<String>,
    pub results: Results,
    index: Option<Arc<Index>>,
    /// (model generation, runs generation) the index was built at.
    built_at: Option<(u64, u64)>,
    /// The field's text as last seen, and when it last changed (for the debounce).
    typed: String,
    typed_at: Option<std::time::Instant>,
    /// How long typing must pause before a query runs.
    pub debounce: std::time::Duration,
    /// The query on the search thread: its number, whether the query text
    /// changed (not only the model), and the slots lit before it.
    pending: Option<(u64, bool, Vec<u32>)>,
    seq: u64,
    worker: Option<search::SearchWorker>,
    /// Whether the graph selection is the search's (so clearing the query clears
    /// it, and a click elsewhere is left alone).
    lit: bool,
    /// Time of the last query, for the report.
    pub last_query: Option<std::time::Duration>,
    /// The query's text terms, for highlighting snippets.
    terms: Vec<String>,
}

impl SearchState {
    /// The index the results refer to (briefs by position).
    pub fn index(&self) -> Option<&Index> {
        self.index.as_deref()
    }

    /// The query the shown results are for.
    pub fn ran(&self) -> &str {
        &self.ran
    }

    /// Whether a query is on the search thread now.
    pub fn searching(&self) -> bool {
        self.pending.is_some()
    }
}

/// The shell's interaction state.
pub struct Interaction {
    pub box_model: BoxModel,
    host: OrchestratorHost,
    /// Why the host could not start (its runtime), shown in the box.
    host_error: Option<String>,
    watch: Option<RunWatch>,
    pub runs: Runs,
    /// The project's state directory (`.kgraf`), where transcripts live.
    pub state_dir: Option<PathBuf>,
    /// The parallelism-derived limits last applied, and the admission they
    /// were applied to: a reload replaces the host's admission, and the
    /// limits must reach the new one (D2). `ConcurrencyCaps` filled this role
    /// before item 5 retired it as a user-facing control.
    applied_limits: Option<(kgraf_orchestrator::Limits, Arc<Admission>)>,
    /// The user's order for the hosted run, as last sent.
    order: Vec<String>,
    /// The hosted run the order belongs to.
    order_run: Option<String>,
    /// The order before the last one sent, restored if that one is refused.
    order_previous: Option<Vec<String>>,
    /// The plan overlay changed since the galaxy last took it.
    plan_dirty: bool,
    /// Pending asks as last counted into the plan.
    asks_seen: Vec<u64>,
    /// Scheduled work nodes from the scheduler thread, added to the plan.
    schedule_nodes: Vec<PlanNode>,
    pub search: SearchState,
    /// Counts run and transcript updates, so derived indexes know when to rebuild.
    runs_generation: u64,
    transcripts: Option<TranscriptWatch>,
    pub corpus: Arc<Corpus>,
    /// Every loaded transcript as a search source (the corpus's own `Arc`s).
    convs: Arc<Vec<ConvSource>>,
    pub text: TextSurface,
    /// The selection the text panel last routed on.
    routed: Selected,
    /// A slot the text panel itself selected (an opened snippet): not rerouted.
    opened_slot: Option<u32>,
}

/// Every loaded transcript as a search source. The transcript text is not copied:
/// each source shares the corpus's `Arc`. A worker's contract comes from its id
/// (`transcripts::contract_of`); its work key doubles as the holder prefix
/// `agent:` matches, since a contract that is its own lineage root holds leases
/// as `<run>/<contract>@...`.
pub fn conv_sources(corpus: &Corpus) -> Vec<ConvSource> {
    corpus
        .transcripts
        .values()
        .map(|t| {
            let contract =
                transcripts::contract_of(&t.run, &t.worker).unwrap_or_else(|| t.worker.clone());
            let work_key = run_view::contract_key(&t.run, &contract);
            ConvSource {
                transcript: Arc::clone(t),
                holder: work_key.clone(),
                work_key,
            }
        })
        .collect()
}

impl std::fmt::Debug for Interaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interaction").finish_non_exhaustive()
    }
}

impl Interaction {
    pub fn new(ctx: &egui::Context, project: Option<&Path>, host: Option<HostParts>) -> Self {
        let wake_ctx = ctx.clone();
        let wake: kgraf_orchestrator::Wake = Arc::new(move || wake_ctx.request_repaint());
        let (host, host_error) = match OrchestratorHost::new(host, wake.clone()) {
            Ok(h) => (h, None),
            // A host that cannot run is still a box that says so.
            Err(e) => (
                OrchestratorHost::idle(wake.clone()),
                Some(format!(
                    "the orchestrator host could not start its runtime: {e}"
                )),
            ),
        };
        let mut runs = Runs::default();
        let watch = project.and_then(|root| {
            let w = wake.clone();
            match RunWatch::spawn(root.to_owned(), POLL, move || w()) {
                Ok(watch) => Some(watch),
                Err(e) => {
                    runs.error = Some(format!("the run watch could not start: {e}"));
                    None
                }
            }
        });
        let search_wake = wake.clone();
        let search = SearchState {
            debounce: SEARCH_DEBOUNCE,
            worker: match search::SearchWorker::spawn(move || search_wake()) {
                Ok(w) => Some(w),
                Err(e) => {
                    runs.error = Some(format!("the search thread could not start: {e}"));
                    None
                }
            },
            ..SearchState::default()
        };
        let transcripts = project.and_then(|root| {
            let w = wake.clone();
            let dir = root.join(kgraf_graph::snapshot::STATE_DIR).join("runs");
            match TranscriptWatch::spawn(dir, transcripts::POLL, move || w()) {
                Ok(t) => Some(t),
                Err(e) => {
                    runs.error = Some(format!("the transcript watch could not start: {e}"));
                    None
                }
            }
        });
        Self {
            box_model: BoxModel::default(),
            host,
            host_error,
            watch,
            runs,
            state_dir: project.map(|p| p.join(kgraf_graph::snapshot::STATE_DIR)),
            applied_limits: None,
            order: Vec::new(),
            order_run: None,
            order_previous: None,
            plan_dirty: true,
            asks_seen: Vec::new(),
            schedule_nodes: Vec::new(),
            search,
            runs_generation: 0,
            transcripts,
            corpus: Arc::new(Corpus::default()),
            convs: Arc::new(Vec::new()),
            text: TextSurface::default(),
            routed: Selected::default(),
            opened_slot: None,
        }
    }

    /// Take what the background parts sent. Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        if self.host.take_order_refused() {
            self.order_refused();
        }
        if let Some(corpus) = self.transcripts.as_ref().and_then(TranscriptWatch::latest) {
            self.convs = Arc::new(conv_sources(&corpus));
            self.corpus = corpus;
            self.runs_generation += 1;
            // Agents' current actions come from the transcripts.
            self.plan_dirty = true;
        }
        let asks: Vec<u64> = self.host.pending_asks().iter().map(|a| a.id).collect();
        if asks != self.asks_seen {
            self.asks_seen = asks;
            self.plan_dirty = true;
        }
        let Some(update) = self.watch.as_ref().and_then(RunWatch::latest) else {
            return self.plan_dirty;
        };
        match update {
            RunsUpdate::Runs {
                runs,
                leased,
                presumed,
            } => {
                self.runs.runs = runs;
                self.runs.leased = leased;
                self.runs.error = None;
                match presumed {
                    Ok(p) => {
                        self.runs.presumed = p;
                        self.runs.presumed_error = None;
                    }
                    // The last good read stays drawn; the box says why it is stale.
                    Err(e) => self.runs.presumed_error = Some(e),
                }
            }
            RunsUpdate::Unavailable(e) => self.runs.error = Some(e),
        }
        self.host
            .set_record_status(run_view::newest_status(&self.runs.runs));
        self.plan_dirty = true;
        self.runs_generation += 1;
        true
    }

    /// The search field: always present, distinct from the orchestrator box
    /// (0032). Results light on the graph through the galaxy's own selection and
    /// camera; the only other output is a count, never a list.
    pub fn search_field(
        &mut self,
        ui: &mut egui::Ui,
        project: Option<&mut kgraf_galaxy::ProjectView>,
    ) {
        let label = ui.label("Search");
        let resp = ui
            .add(
                egui::TextEdit::singleline(&mut self.search.query)
                    .hint_text(SEARCH_HINT)
                    .desired_width(320.0),
            )
            .labelled_by(label.id)
            .on_hover_text(SEARCH_HELP);
        if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.search.query.clear();
        }
        self.run_search(Some(ui.ctx()), project);
        let s = &self.search;
        if let Some(e) = &s.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        } else if !s.ran.trim().is_empty() {
            ui.label(
                egui::RichText::new(results_line(&s.results, s.pending.is_some()))
                    .small()
                    .weak(),
            );
        }
    }

    /// Runs the query when it, the model or the runs changed, and lights the
    /// result on the graph. Typing is debounced ([`SearchState::debounce`]) and
    /// the scan runs on the search thread; its answer is applied on a later frame
    /// (the thread wakes the UI). `ctx` schedules the frame that ends a debounce.
    pub fn run_search(
        &mut self,
        ctx: Option<&egui::Context>,
        project: Option<&mut kgraf_galaxy::ProjectView>,
    ) {
        let Some(project) = project else { return };
        if let Some(done) = self
            .search
            .worker
            .as_ref()
            .and_then(search::SearchWorker::try_latest)
        {
            self.apply_search(done, project);
        }
        let now = std::time::Instant::now();
        if self.search.query != self.search.typed {
            self.search.typed = self.search.query.clone();
            self.search.typed_at = Some(now);
        }
        let generation = (project.generation(), self.runs_generation);
        let stale_index = self.search.built_at != Some(generation);
        let query_changed = self.search.query != self.search.ran;
        if !query_changed && !stale_index {
            return;
        }
        if query_changed && let Some(at) = self.search.typed_at {
            let waited = now.duration_since(at);
            if waited < self.search.debounce {
                if let Some(ctx) = ctx {
                    ctx.request_repaint_after(self.search.debounce - waited);
                }
                return;
            }
        }
        let query = self.search.query.clone();
        let parsed = match search::parse(&query) {
            Ok(q) => q,
            Err(e) => {
                self.search.error = Some(e.to_string());
                self.search.ran = query;
                return;
            }
        };
        self.search.error = None;
        self.search.ran = query;
        self.search.terms = parsed
            .terms
            .iter()
            .filter_map(|t| match t {
                search::Term::Text(s) => Some(s.clone()),
                search::Term::Op(..) => None,
            })
            .collect();
        if parsed.is_empty() {
            self.search.pending = None;
            self.search.results = Results::default();
            if query_changed {
                self.text.on_search(&self.search.results);
                if self.search.lit
                    && let Some(v) = project.view_mut()
                {
                    v.galaxy_mut().select_set(&[]);
                }
                self.search.lit = false;
            }
            return;
        }
        let Some(model) = project.model() else {
            return;
        };
        if stale_index || self.search.index.is_none() {
            self.search.index = Some(Arc::new(Index::build(
                model,
                &self.runs.runs,
                self.convs.clone(),
            )));
            self.search.built_at = Some(generation);
        }
        let (Some(index), Some(worker)) = (&self.search.index, &self.search.worker) else {
            self.search.error = Some("search is unavailable: its thread did not start".to_owned());
            return;
        };
        self.search.seq += 1;
        let before: Vec<u32> = self.search.results.hits.iter().map(|h| h.slot).collect();
        // A model-only re-run keeps "the query changed" if an earlier dispatch of
        // this same query has not been applied yet.
        let changed = query_changed || self.search.pending.as_ref().is_some_and(|p| p.1);
        self.search.pending = Some((self.search.seq, changed, before));
        if let Err(e) = worker.submit(search::SearchJob {
            seq: self.search.seq,
            index: Arc::clone(index),
            query: parsed,
        }) {
            self.search.error = Some(e);
            self.search.pending = None;
        }
    }

    /// Applies the search thread's answer, if it is for the newest query.
    fn apply_search(&mut self, done: search::SearchDone, project: &mut kgraf_galaxy::ProjectView) {
        let Some((seq, query_changed, before)) = self.search.pending.take() else {
            return;
        };
        if seq != done.seq {
            // An older answer: the newest query is still on the thread.
            self.search.pending = Some((seq, query_changed, before));
            return;
        }
        self.search.results = done.results;
        self.search.last_query = Some(done.took);
        let slots: Vec<u32> = self.search.results.hits.iter().map(|h| h.slot).collect();
        // A new query lights its result. The same query re-run because the model
        // or the runs changed relights only while the search still owns the
        // selection and its result changed: a node the user clicked since stays
        // selected, and an unchanged result does not fly the camera again.
        if query_changed || (self.search.lit && slots != before) {
            if let Some(v) = project.view_mut() {
                v.galaxy_mut().select_set(&slots);
            }
            self.search.lit = true;
            self.text.on_search(&self.search.results);
        }
    }

    /// Runs the current query now, bypassing the debounce, and waits for the
    /// search thread's answer (tests, and anything that must see the result in
    /// the same frame).
    pub fn search_now(&mut self, project: Option<&mut kgraf_galaxy::ProjectView>) {
        let Some(project) = project else { return };
        self.search.typed = self.search.query.clone();
        self.search.typed_at = None;
        self.run_search(None, Some(project));
        if self.search.pending.is_none() {
            return;
        }
        let waited = self
            .search
            .worker
            .as_ref()
            .and_then(|w| w.wait(std::time::Duration::from_secs(10)));
        if let Some(done) = waited {
            self.apply_search(done, project);
        }
    }

    /// Route the text panel on the user's graph selection. The search's own
    /// selection and a snippet the panel opened are not the user's and do not
    /// reroute it.
    pub fn on_selection(&mut self, selected: &Selected) {
        if *selected == self.routed {
            return;
        }
        self.routed = selected.clone();
        let from_search = self.search.lit
            && selected.slot.is_some()
            && self.search.results.hits.first().map(|h| h.slot) == selected.slot;
        if from_search || (selected.slot.is_some() && selected.slot == self.opened_slot) {
            return;
        }
        self.opened_slot = None;
        if selected.slot.is_some() {
            // A click on the graph is the user's own selection now.
            self.search.lit = false;
        }
        let conversation = text_surface::conversation_for(
            selected.agent.as_deref(),
            selected.work.as_deref(),
            &self.runs.runs,
            &self.corpus,
        );
        self.text.on_selection(conversation);
    }

    /// The one text surface. An opened snippet selects its node on the graph and
    /// expands to its conversation in place (0034).
    pub fn text_surface(
        &mut self,
        ui: &mut egui::Ui,
        project: Option<&mut kgraf_galaxy::ProjectView>,
    ) {
        let opened = self.text.show(
            ui,
            &self.corpus,
            self.runs_generation as usize,
            self.search.index.as_deref(),
            &self.search.results,
            &self.search.terms,
        );
        if let Some(at) = opened {
            self.open_snippet(&at, project);
        }
    }

    /// Open one conversation turn: select its node, expand it in the panel.
    pub fn open_snippet(
        &mut self,
        hit: &search::TurnHit,
        project: Option<&mut kgraf_galaxy::ProjectView>,
    ) {
        let at = &hit.at;
        let present = self
            .corpus
            .get(&at.run, &at.worker)
            .is_some_and(|t| at.turn < t.turns.len());
        if !present {
            self.box_model.say(Line::Error(format!(
                "that turn is gone from the transcripts ({} turn {})",
                at.worker, at.turn
            )));
            return;
        }
        let slot = self
            .search
            .index
            .as_ref()
            .and_then(|i| i.slots_for(&hit.work_key).first().copied());
        match (slot, project.and_then(|p| p.view_mut())) {
            (Some(slot), Some(view)) => {
                view.galaxy_mut().select(Some(slot));
                self.opened_slot = Some(slot);
                self.search.lit = false;
            }
            _ => self.box_model.say(Line::Notice(format!(
                "{} has no node on the graph (its run is not shown); its conversation is open",
                hit.work_key
            ))),
        }
        self.text.open(at, &hit.work_key, &self.runs.runs);
    }

    /// The largest parallel-attempt count among the shown runs (for the caps).
    pub fn max_attempts(&self) -> Option<usize> {
        run_view::max_attempts(&self.runs.runs).map(|(n, _)| n)
    }

    /// Until when a person is at the window (see
    /// `OrchestratorHost::set_attended_until`).
    pub fn set_attended_until(&self, until: Option<std::time::Instant>) {
        self.host.set_attended_until(until);
    }

    /// The host, for play, the user's order and tests.
    pub fn host_mut(&mut self) -> &mut OrchestratorHost {
        &mut self.host
    }

    /// The box's model and its backend, together (the styled box draws both).
    pub fn box_and_host(&mut self) -> (&mut BoxModel, &mut OrchestratorHost) {
        (&mut self.box_model, &mut self.host)
    }

    /// The host, read only (the project switcher's status reads it).
    pub fn host(&self) -> &OrchestratorHost {
        &self.host
    }

    /// Takes the backend's events into the box's history. The shown project's box
    /// does this as it draws; a project in the background does it here each
    /// frame, so its hosted run advances and its record is current (0066).
    pub fn absorb_backend(&mut self) -> bool {
        self.box_model.absorb(&mut self.host)
    }

    /// Whether runs are hosted in this window.
    pub fn hosting(&self) -> bool {
        self.host.is_hosting()
    }

    /// Apply `limits` (derived each frame from the parallelism slider's
    /// effective value, decision 0067) to the
    /// host's admission when they changed.
    pub fn sync_parallelism(&mut self, limits: kgraf_orchestrator::Limits) {
        // The host opens on first need (its run.json read then); until it has,
        // there is no admission to bind, and the limits wait.
        let Some(admission) = self.host.admission() else {
            return;
        };
        if self
            .applied_limits
            .as_ref()
            .is_some_and(|(l, a)| *l == limits && Arc::ptr_eq(a, &admission))
        {
            return;
        }
        apply_parallelism(limits, &admission);
        self.applied_limits = Some((limits, admission));
        // The ceiling decides which nodes can never fit.
        self.plan_dirty = true;
    }

    /// The scheduled work nodes (decision 0052 section 3): each schedule on its
    /// work node, with its last run and any disable reason on the label. They join
    /// the plan overlay; a change marks it for the galaxy to take.
    pub fn set_schedule_nodes(&mut self, nodes: Vec<PlanNode>) {
        if nodes != self.schedule_nodes {
            self.schedule_nodes = nodes;
            self.plan_dirty = true;
        }
    }

    /// The plan overlay, when it changed since the last call: the runs' work nodes,
    /// then the scheduled work nodes, then the presumed work nodes (decision 0064)
    /// that neither already draws.
    pub fn take_plan(&mut self) -> Option<(Vec<PlanNode>, HashMap<String, String>)> {
        if !self.plan_dirty {
            return None;
        }
        self.plan_dirty = false;
        let asks = self.host.pending_asks();
        let mut plan = run_view::plan_nodes(
            &self.runs.runs,
            &asks,
            self.hosting()
                .then(|| {
                    self.applied_limits
                        .as_ref()
                        .map(|(l, _)| l.max_live_agents())
                })
                .flatten(),
        );
        // Schedule nodes are built without the desk; their waiting asks are set here,
        // the same way run_view sets them (`PlanNode::asks`).
        plan.extend(self.schedule_nodes.iter().cloned().map(|mut n| {
            n.asks = asks.iter().filter(|a| a.task == n.key).count();
            n
        }));
        run_view::fold_presumed(&mut plan, &self.runs.runs, &self.runs.presumed);
        Some((
            plan,
            run_view::agent_notes(&self.runs.runs, &self.corpus, &asks),
        ))
    }

    /// The newest stored run marked for resume.
    /// What Play would do now.
    pub fn play_action(&self) -> PlayAction {
        play_action(&self.host.state(), &resumable(&self.runs))
    }

    /// Presses Play (or Stop, while a run is hosted).
    pub fn play(&mut self) {
        match self.play_action() {
            PlayAction::Resume(run) => match self.host.resume(RunId::new(run.clone())) {
                Ok(()) => self
                    .box_model
                    .say(Line::Notice(format!("resuming run {run}"))),
                Err(e) => self
                    .box_model
                    .say(Line::Error(format!("run {run} did not resume: {e}"))),
            },
            PlayAction::Stop => {
                if let Some(r) = self.host.running() {
                    r.stop();
                    self.box_model.say(Line::Notice(
                        "stopping: workers are cancelled, the run stays resumable".to_owned(),
                    ));
                }
            }
            PlayAction::Disabled(reason) => self.box_model.say(Line::Error(reason)),
        }
    }

    /// Append the selected work node to the user's order (0014): it is dispatched
    /// after the nodes already ordered and before the ones the user did not order.
    pub fn pin(&mut self, key: &str) -> Result<(), String> {
        // A graph work node (presumed, 0064, or scheduled, 0052) is in no run: it
        // has no place in a run's order. Said, rather than mistaken for a run id.
        if key.starts_with("work:") {
            return Err(
                "that work node is not in a run: a presumed node runs once an approved plan \
                 adopts it (`kgraf orchestrate --corpus`, then --approve), a scheduled one on \
                 its schedule"
                    .to_owned(),
            );
        }
        let Some((run, contract)) = key.split_once('/') else {
            return Err(format!("`{key}` is not a work node"));
        };
        if contract.starts_with("gate/") {
            return Err("a gate runs when what it covers is done; it takes no order".to_owned());
        }
        let hosted = match self.host.state() {
            BackendState::Running { run } => run,
            _ => {
                return Err(format!(
                    "the order is kept by the run's orchestrator, and none runs in this window. {}",
                    if self.hosting() { "" } else { NO_HOST }
                ));
            }
        };
        if hosted != run {
            return Err(format!(
                "that node belongs to run {run}; this window runs {hosted}"
            ));
        }
        let record = run_view::records(&self.runs.runs).find(|r| r.run.as_str() == run);
        if self.order_run.as_deref() != Some(run) {
            // A new hosted run: start from the order its record already holds.
            self.order = record
                .map(|r| r.coordination.user_order.clone())
                .unwrap_or_default();
            self.order_run = Some(run.to_owned());
        }
        let open: Vec<&str> = record
            .map(|r| {
                r.open_contracts
                    .iter()
                    .map(|c| c.contract.as_str())
                    .collect()
            })
            .unwrap_or_default();
        if !open.contains(&contract) {
            return Err(format!(
                "{contract} is not open in run {run} (verified, superseded, or not planned yet)"
            ));
        }
        self.set_order(next_order(&self.order, &open, contract))
    }

    /// Sends `order`, keeping the previous one to restore if the orchestrator
    /// refuses it.
    fn set_order(&mut self, order: Vec<String>) -> Result<(), String> {
        let previous = std::mem::replace(&mut self.order, order.clone());
        match self.host.set_order(order) {
            Ok(()) => {
                self.order_previous = Some(previous);
                Ok(())
            }
            Err(e) => {
                self.order = previous;
                Err(e)
            }
        }
    }

    /// The orchestrator refused the order last sent: go back to the one before.
    pub fn order_refused(&mut self) {
        if let Some(previous) = self.order_previous.take() {
            self.order = previous;
        }
    }

    /// The order as this window last sent it.
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// Hand sequencing back to the orchestrator.
    pub fn clear_order(&mut self) -> Result<(), String> {
        self.set_order(Vec::new())
    }

    /// Play, and the order controls for a selected work node.
    fn drive(&mut self, ui: &mut egui::Ui, selected: &Selected) {
        ui.horizontal(|ui| {
            let action = self.play_action();
            let (text, hint, enabled) = match &action {
                PlayAction::Resume(run) => ("Play", format!("resume run {run}"), true),
                PlayAction::Stop => (
                    "Stop",
                    "cancel the workers; the run stays resumable".to_owned(),
                    true,
                ),
                PlayAction::Disabled(reason) => ("Play", reason.clone(), false),
            };
            if ui
                .add_enabled(enabled, egui::Button::new(text))
                .on_hover_text(&hint)
                .on_disabled_hover_text(&hint)
                .clicked()
            {
                self.play();
            }
            if let Some(key) = &selected.work {
                let key = key.clone();
                if ui
                    .button("Add to my order")
                    .on_hover_text(
                        "dispatch the selected node after those already in your order and \
                         before the rest; admission still binds",
                    )
                    .clicked()
                    && let Err(e) = self.pin(&key)
                {
                    self.box_model.say(Line::Error(format!("not ordered: {e}")));
                }
                if !self.order.is_empty()
                    && ui
                        .button("Clear order")
                        .on_hover_text("hand sequencing back to the orchestrator")
                        .clicked()
                    && let Err(e) = self.clear_order()
                {
                    self.box_model
                        .say(Line::Error(format!("order not cleared: {e}")));
                }
            }
        });
    }

    /// The orchestrator box, with play and the order controls above it.
    pub fn orchestrator_box(&mut self, ui: &mut egui::Ui, selected: &Selected) {
        if let Some(e) = &self.host_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        if let Some(e) = &self.runs.error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("run records unreadable: {e}"),
            );
        }
        if let Some(e) = &self.runs.presumed_error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("presumed work nodes unreadable: {e}"),
            );
        }
        self.drive(ui, selected);
        let worker = selected
            .agent
            .as_deref()
            .and_then(|a| run_view::worker_for_holder(&self.runs.runs, a));
        orchestrator_box::show(ui, &mut self.box_model, &mut self.host, worker.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_orchestrator::{Limits, Refusal};

    #[test]
    fn play_resumes_stops_or_says_why_not() {
        let r0 = Resumable::Run("r0".into());
        let running = BackendState::Running { run: "r1".into() };
        assert_eq!(play_action(&running, &r0), PlayAction::Stop);
        let idle = BackendState::Idle {
            can_start: true,
            reason: String::new(),
        };
        assert_eq!(play_action(&idle, &r0), PlayAction::Resume("r0".into()));
        assert!(matches!(
            play_action(&idle, &Resumable::None),
            PlayAction::Disabled(_)
        ));
        assert!(matches!(
            play_action(&idle, &Resumable::HeldElsewhere(vec!["r0".into()])),
            PlayAction::Disabled(m) if m.contains("held by another live process")
        ));
        let no_host = BackendState::Idle {
            can_start: false,
            reason: NO_HOST.into(),
        };
        assert_eq!(
            play_action(&no_host, &r0),
            PlayAction::Disabled(NO_HOST.into())
        );
    }

    fn stored(run: &str, status: kgraf_orchestrator::RunStatus, updated: i64) -> StoredRun {
        let mut r =
            kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new(run), "go", updated);
        r.status = status;
        StoredRun {
            run: run.into(),
            active: true,
            record: Ok(r),
        }
    }

    /// Play never offers a completed run (resume's AlreadyCompleted guard) nor one
    /// a live process holds; it offers the newest remaining one.
    #[test]
    fn play_skips_completed_runs_and_runs_held_elsewhere() {
        use kgraf_orchestrator::RunStatus;
        let mut runs = Runs {
            runs: vec![
                stored("done", RunStatus::Completed, 30),
                stored("held", RunStatus::Stopped, 20),
                stored("old", RunStatus::Stopped, 10),
            ],
            ..Runs::default()
        };
        runs.leased.insert("held".into());
        assert_eq!(resumable(&runs), Resumable::Run("old".into()));
        runs.leased.insert("old".into());
        assert_eq!(
            resumable(&runs),
            Resumable::HeldElsewhere(vec!["held".into(), "old".into()])
        );
        runs.runs.retain(|r| r.run == "done");
        assert_eq!(resumable(&runs), Resumable::None);
    }

    /// Pin a, a is verified, pin c: the order sent names c alone, since a
    /// verified contract cannot be ordered and would make the whole order refused.
    #[test]
    fn an_order_sends_only_open_contracts() {
        let order = next_order(&[], &["a", "b", "c"], "a");
        assert_eq!(order, ["a"]);
        // a is verified now: no longer open.
        let order = next_order(&order, &["b", "c"], "c");
        assert_eq!(order, ["c"]);
        assert_eq!(next_order(&order, &["b", "c"], "c"), ["c"], "no repeats");
    }

    /// When the orchestrator refuses an order, the previous one comes back.
    #[test]
    fn a_refused_order_rolls_back() {
        let ctx = egui::Context::default();
        let mut i = Interaction::new(&ctx, None, None);
        i.order = vec!["a".into()];
        i.order_previous = Some(vec![]);
        i.order = vec!["a".into(), "zz".into()];
        i.order_previous = Some(vec!["a".into()]);
        i.order_refused();
        assert_eq!(i.order(), ["a"]);
    }
    /// A live-agent ceiling below a contract's attempts is said on the node
    /// and in the settings surface, never a silent forever-hold.
    #[test]
    fn a_ceiling_below_a_contracts_attempts_shows_can_never_fit() {
        use egui_kittest::kittest::Queryable;
        use kgraf_galaxy::WorkState;
        let mut r = kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "go", 1);
        let mut c = kgraf_orchestrator::OpenContract::planned("a", "r1/a".into(), "make a", "ok");
        c.attempts = 3;
        r.open_contracts = vec![c];
        let runs = vec![StoredRun {
            run: "r1".into(),
            active: true,
            record: Ok(r),
        }];
        let nodes = run_view::plan_nodes(&runs, &[], Some(2));
        assert_eq!(nodes[0].state, WorkState::HeldByCap);
        assert!(
            nodes[0].label.contains("can never fit"),
            "{}",
            nodes[0].label
        );
        let nodes = run_view::plan_nodes(&runs, &[], Some(3));
        assert_eq!(nodes[0].state, WorkState::Planned, "fits at 3");

        let most = run_view::max_attempts(&runs).expect("attempts");
        assert_eq!(most, (3, "r1/a".to_owned()));
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, s: &mut ParallelismSlider| {
                parallelism_controls(
                    ui,
                    &crate::theme::Palette::dark(),
                    s,
                    None,
                    true,
                    Some(most.0),
                )
            },
            ParallelismSlider::new(2),
        );
        h.run();
        h.get_by_label("Below the largest attempt count: 3");
        assert!(
            h.query_by_label_contains("r1/a").is_none(),
            "settings names no contract (configuration only)"
        );
    }

    /// `ConcurrencyCaps` is retired as a user
    /// limit. `apply_parallelism` plumbs the slider's effective value (not a
    /// fixed default) straight into the admission's live-agent ceiling; the
    /// setting's effect, not its value, is what a test must prove.
    #[test]
    fn moving_the_slider_changes_what_the_admission_refuses() {
        let admission = Admission::new(Limits::new(4, 8).expect("limits"));
        let a = admission.gate().try_admit(1).expect("one");
        apply_parallelism(Limits::new(1, 8).expect("limits"), &admission);
        assert_eq!(
            admission.gate().try_admit(1).expect_err("capped"),
            Refusal::WorkItemCap {
                in_flight: 1,
                cap: 1
            }
        );
        apply_parallelism(Limits::new(2, 8).expect("limits"), &admission);
        let b = admission.gate().try_admit(1).expect("under the raised cap");
        // The ceiling on live agents follows the slider, independently of
        // the work-item cap `kgraf-capacity`'s `max_writers` drives.
        apply_parallelism(Limits::new(16, 2).expect("limits"), &admission);
        assert!(matches!(
            admission.gate().try_admit(1),
            Err(Refusal::LiveAgentCeiling { .. })
        ));
        admission.gate().release(a);
        admission.gate().release(b);
    }

    /// The slider in the settings surface moves the effective limit (kittest
    /// drives the real widget), and the moved value then binds the admission.
    #[test]
    fn the_settings_slider_sets_the_limit_the_admission_enforces() {
        use egui_kittest::kittest::Queryable;
        let mut h = egui_kittest::Harness::new_ui_state(
            |ui, s: &mut ParallelismSlider| {
                // A pinned slider draws "Return to maximum" (`chrome::text_button`),
                // which needs the custom font family `theme::ensure` installs; the
                // slider below moves it away from following, so this must run first.
                if !crate::theme::ensure(ui.ctx()) {
                    return;
                }
                parallelism_controls(ui, &crate::theme::Palette::dark(), s, None, true, None)
            },
            ParallelismSlider::new(4),
        );
        h.run();
        let slider = h
            .get_all_by_role(egui::accesskit::Role::Slider)
            .next()
            .expect("slider");
        slider.focus();
        h.run();
        // Three steps left: 4 (the default) to 1.
        for _ in 0..3 {
            h.key_press(egui::Key::ArrowLeft);
            h.run();
        }
        let set = *h.state();
        assert_eq!(set.effective(), 1, "the slider moved the limit");
        let admission = Admission::new(Limits::default());
        apply_parallelism(
            Limits::new(4, set.effective() as usize).expect("limits"),
            &admission,
        );
        let _a = admission.gate().try_admit(1).expect("first");
        assert!(
            admission.gate().try_admit(1).is_err(),
            "the second is refused"
        );
    }

    /// two projects sharing one `MachineGate`
    /// with the slider at 12 admit twelve agents in total, not eight per
    /// project (`DEFAULT_MAX_LIVE_AGENTS`, `ConcurrencyCaps`'s old fixed
    /// default). Project A alone takes nine -- one more than the retired
    /// default -- proving its own local ceiling now tracks the slider, not a
    /// number `ConcurrencyCaps` used to apply identically to every project.
    #[test]
    fn the_sliders_effective_value_bounds_every_project_not_eight_each() {
        let gate =
            kgraf_orchestrator::MachineGate::new(kgraf_core::ParallelismLimits::derive(12, 4, 4));
        let a = Admission::with_machine(
            Limits::new(4, kgraf_orchestrator::DEFAULT_MAX_LIVE_AGENTS).expect("limits"),
            gate.clone(),
        );
        let b = Admission::with_machine(
            Limits::new(4, kgraf_orchestrator::DEFAULT_MAX_LIVE_AGENTS).expect("limits"),
            gate.clone(),
        );
        // The work-item cap is generous (16, `MAX_WORK_ITEMS_CAP`): each
        // `try_admit(1)` below is its own work item, and this test's point is
        // the live-agent ceiling, not the item cap.
        let limits = Limits::new(16, 12).expect("limits");
        apply_parallelism(limits, &a);
        apply_parallelism(limits, &b);

        let held_a: Vec<_> = (0..9)
            .map(|_| a.gate().try_admit(1).expect("within the shared twelve"))
            .collect();
        let held_b: Vec<_> = (0..3)
            .map(|_| b.gate().try_admit(1).expect("within the shared twelve"))
            .collect();
        assert_eq!(gate.agents_live(), 12);
        // The thirteenth, from either project, is refused: the shared gate
        // is full at the slider's own value, twelve, not some per-project
        // multiple of it.
        assert!(a.gate().try_admit(1).is_err());
        assert!(b.gate().try_admit(1).is_err());

        for h in held_a {
            a.gate().release(h);
        }
        for h in held_b {
            b.gate().release(h);
        }
    }

    /// D2: after a reload gives the host a new admission, the same limits reach it.
    #[test]
    fn the_limits_reach_a_reloaded_hosts_admission() {
        use crate::orchestrator_host::{HostParts, RunHousekeeping};
        use kgraf_orchestrator::{
            Orchestrator, OrchestratorError, OrchestratorFactory, ReadinessFailure,
        };
        struct Nothing;
        #[async_trait::async_trait]
        impl OrchestratorFactory for Nothing {
            async fn configured(&self) -> Result<(), ReadinessFailure> {
                Ok(())
            }
            fn build(&self, _run: &RunId) -> Result<Orchestrator, OrchestratorError> {
                Err(OrchestratorError::NoRoutes)
            }
        }
        struct Swappable(std::sync::Mutex<Arc<Admission>>);
        impl RunHousekeeping for Swappable {
            fn admission(&self) -> Option<Arc<Admission>> {
                Some(self.0.lock().expect("lock").clone())
            }
            fn finish_run(&self, _run: &RunId) -> Vec<String> {
                Vec::new()
            }
            fn take_notices(&self) -> Vec<String> {
                Vec::new()
            }
        }
        let new_admission = || Admission::new(Limits::default());
        let first = new_admission();
        let house = Arc::new(Swappable(std::sync::Mutex::new(first.clone())));
        let parts = HostParts {
            factory: Arc::new(Nothing),
            housekeeping: house.clone(),
            desk: Arc::new(kgraf_orchestrator::AskDesk::new(None, Arc::new(|| {}))),
            router: None,
        };
        let ctx = egui::Context::default();
        let mut i = Interaction::new(&ctx, None, Some(parts));
        let set = Limits::new(2, 8).expect("limits");
        i.sync_parallelism(set);
        assert_eq!(first.gate().limits().max_work_items(), 2);
        // The run.json reload: a new host, a new admission at its defaults.
        let second = new_admission();
        *house.0.lock().expect("lock") = second.clone();
        assert_ne!(second.gate().limits().max_work_items(), 2);
        i.sync_parallelism(set);
        assert_eq!(
            second.gate().limits().max_work_items(),
            2,
            "the unchanged limits reached the reloaded host"
        );
    }

    #[test]
    fn with_no_host_an_order_is_refused_with_the_reason() {
        let ctx = egui::Context::default();
        let mut i = Interaction::new(&ctx, None, None);
        let err = i.pin("r1/a").expect_err("no host");
        assert!(err.contains("none runs in this window"), "{err}");
        assert!(i.pin("r1/gate/g").expect_err("gate").contains("gate"));
        assert!(i.pin("nokey").is_err());
        // A graph work node (presumed, 0064) is in no run, and is not read as one.
        let err = i
            .pin("work:presumed/roadmap/ROADMAP.md/item/phases/1")
            .expect_err("not a run's");
        assert!(err.contains("not in a run"), "{err}");
    }

    /// Scheduled work nodes join the plan overlay the galaxy takes, and a
    /// change to them (a new last run, a disable reason) marks it again.
    #[test]
    fn scheduled_work_nodes_join_the_plan_overlay() {
        let ctx = egui::Context::default();
        let mut i = Interaction::new(&ctx, None, None);
        let _ = i.take_plan();
        let node = |label: &str| PlanNode {
            key: "work:schedules/nightly".into(),
            state: kgraf_galaxy::WorkState::HeldForUser,
            label: label.into(),
            blocked_by: Vec::new(),
            asks: 0,
            parent: None,
        };
        i.set_schedule_nodes(vec![node("nightly: held for you (disabled: x)")]);
        let (plan, _) = i.take_plan().expect("marked");
        assert_eq!(plan, vec![node("nightly: held for you (disabled: x)")]);
        i.set_schedule_nodes(vec![node("nightly: held for you (disabled: x)")]);
        assert!(i.take_plan().is_none(), "the same nodes do not mark it");
        i.set_schedule_nodes(vec![node("nightly: held for you (disabled: y)")]);
        assert!(i.take_plan().is_some());
    }
}
