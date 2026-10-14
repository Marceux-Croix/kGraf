//! The window's layout: the Stage style (`docs/STYLE_GUIDE.md`, its layout and
//! stage rules) drawn with egui.
//!
//! ```text
//!  masthead: [=] kGraf^branch | project   [ search .......... ][Q]   [Scheduler | n of m workers] [GPU] [bell]
//!  nav | chips ..............................................  | Up next (one text surface, 0034)
//!      | the stage: the galaxy on near-black, legend, minimap,  |   Selection | Search
//!      |            selection title, scrubber, view controls    |
//!      | project row, pinned cards, history, composer           | Live activity (0063)
//! ```
//!
//! Nothing here is a second work surface (0017, 0034,
//! 0063 and 0066): the stage is the graph; "Up next" is the one text surface;
//! "Live activity" is the one change-log card; the nav's project list is
//! navigation; settings holds configuration only. Colors come from
//! [`crate::theme`] alone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use kgraf_galaxy::{ProjectView, RenderBudget};

use crate::activity::{self, ActivityWatch};
use crate::app::ShellApp;
use crate::chrome::{self, avatar, glyph};
use crate::icons;
use crate::interaction::{PlayAction, Selected};
use crate::orchestrator_box::{BackendState, BoxBackend, BoxOptions, Line};
use crate::projects::{hamburger_look, wiggle_degrees};
use crate::session_panel;
use crate::settings::{
    Appearance, COMPACTION_MAX_PERCENT, COMPACTION_MIN_PERCENT, CompactionPoint, Recommendation,
};
use crate::settings_rows;
use crate::text_surface::{Feed, Tab};
use crate::theme::{self, Indicator, Palette, motion, size};
use crate::update_bg::UpdateState;

/// Where the nav and the settings dialog point: one tab per kind of
/// configuration (CMP-DLG-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    /// The window and updates.
    #[default]
    General,
    /// Everything a run takes from Settings.
    Runs,
    /// The user-level permission settings and rules.
    Permissions,
    /// Theme, color vision, motion and the graphics adapter.
    Appearance,
    /// The active project's routes, read-only.
    Providers,
}

impl SettingsTab {
    /// The tabs in the order the dialog lists them.
    pub const ALL: [SettingsTab; 5] = [
        Self::General,
        Self::Runs,
        Self::Permissions,
        Self::Appearance,
        Self::Providers,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Runs => "Runs",
            Self::Permissions => "Permissions",
            Self::Appearance => "Appearance",
            Self::Providers => "Providers",
        }
    }

    /// The tab's 20 px icon (CMP-DLG-3). Permissions uses the shield the
    /// orchestrator box's permission bar shows.
    pub fn icon(self) -> char {
        match self {
            Self::General => icons::SETTINGS,
            Self::Runs => icons::PLAY,
            Self::Permissions => icons::SHIELD,
            Self::Appearance => icons::DARK_MODE,
            Self::Providers => icons::PROVIDERS,
        }
    }
}

/// The nav's current entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Nav {
    #[default]
    Graph,
    Search,
}

/// The minimap's density image, for one scene generation.
struct Minimap {
    root: PathBuf,
    generation: u64,
    texture: egui::TextureHandle,
    /// World bounds (min x, min y, max x, max y) the image covers.
    bounds: [f32; 4],
}

/// A project's crate chips for one model generation: (root, generation,
/// (label, query) pairs).
type CrateChips = (PathBuf, u64, Vec<(String, String)>);

/// A project's `.kgraf/run.json`, as far as the chrome shows it.
#[derive(Debug, Clone, Default)]
pub struct RunJson {
    pub routes: Vec<String>,
    pub planner: Option<String>,
    /// Routes of other model kinds, `kind=route:model` (decision 0089).
    pub model_routes: Vec<String>,
    pub error: Option<String>,
}

/// What the folder picker answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerAnswer {
    Chosen(PathBuf),
    /// The user cancelled, or no dialog could open (a missing portal).
    Nothing,
    /// The picker thread ended without an answer.
    Failed,
}

/// The folder picker in flight. Its answer belongs to the Add form that asked:
/// when that form closes first, the answer is discarded when it comes
/// (low item), never applied to a later form.
#[derive(Default)]
pub(crate) struct Picker {
    rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    /// The form that asked has closed.
    abandoned: bool,
}

impl Picker {
    pub(crate) fn start(&mut self, rx: std::sync::mpsc::Receiver<Option<PathBuf>>) {
        self.rx = Some(rx);
        self.abandoned = false;
    }

    /// A dialog is open (asked by this form or an abandoned one).
    pub(crate) fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// The form that asked closed: drop its answer when it comes.
    pub(crate) fn abandon(&mut self) {
        self.abandoned = self.rx.is_some();
    }

    /// The answer, once it came and its form is still open.
    pub(crate) fn poll(&mut self) -> Option<PickerAnswer> {
        let answer = match self.rx.as_ref()?.try_recv() {
            Ok(Some(dir)) => PickerAnswer::Chosen(dir),
            Ok(None) => PickerAnswer::Nothing,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => PickerAnswer::Failed,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
        };
        self.rx = None;
        let abandoned = std::mem::take(&mut self.abandoned);
        (!abandoned).then_some(answer)
    }
}

/// Chrome state: which overlays and menus are open. Not persisted, except the
/// nav's open state, which lives in the settings.
pub struct ChromeState {
    pub legend_open: bool,
    pub minimap_open: bool,
    pub bell_open: bool,
    pub settings_tab: Option<SettingsTab>,
    pub nav: Nav,
    pub add_open: bool,
    pub add_path: String,
    pub add_error: Option<String>,
    /// The folder picker in flight, if any.
    picker: Picker,
    /// A line said once at the top of the stage (a refused close, a failed pick).
    pub notice: Option<(String, Instant)>,
    /// The pinned card to flash: (project folder, card key, since). The key is
    /// `ask:<id>` for an approval or a failure's key; a folder, not an index, so
    /// a close or an add in between cannot point it at another project.
    pub flash: Option<(PathBuf, String, Instant)>,
    pub focus_search: bool,
    pub hamburger_hovered: bool,
    /// The window is under 1360 points wide: short labels (LAY-8).
    pub compact: bool,
    minimap: Option<Minimap>,
    crates: Option<CrateChips>,
    attribution: Option<(PathBuf, Instant, String)>,
    branches: FileCache<Option<String>>,
    run_json: FileCache<RunJson>,
    /// Whether anything drawn this frame blinks or wiggles.
    animating: Option<Duration>,
    /// The color vision the open project views were last given ([`sync_stage_vision`]).
    stage_vision: Option<kgraf_core::palette::Vision>,
    /// Settings, install registries: each package manager's host list as it
    /// is being edited, one host per line, with the reason it is refused, if
    /// it is. Filled from the settings when first drawn.
    install_registry_text:
        std::collections::BTreeMap<kgraf_host::deps::Ecosystem, (String, Option<String>)>,
}

impl ChromeState {
    /// Closes the Add project form; a picker it opened is abandoned.
    pub(crate) fn close_add_form(&mut self) {
        self.add_open = false;
        self.picker.abandon();
    }
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            legend_open: true,
            minimap_open: true,
            bell_open: false,
            settings_tab: None,
            nav: Nav::Graph,
            add_open: false,
            add_path: String::new(),
            add_error: None,
            picker: Picker::default(),
            notice: None,
            flash: None,
            focus_search: false,
            hamburger_hovered: false,
            compact: false,
            minimap: None,
            crates: None,
            attribution: None,
            branches: FileCache::new(FILE_RECHECK),
            run_json: FileCache::new(FILE_RECHECK),
            animating: None,
            stage_vision: None,
            install_registry_text: std::collections::BTreeMap::new(),
        }
    }
}

impl ChromeState {
    /// Asks for a redraw within `after` (the nearest of all asked this frame).
    fn animate(&mut self, after: Duration) {
        self.animating = Some(self.animating.map_or(after, |a| a.min(after)));
    }
}

/// The branch checked out in `root`, from `.git/HEAD` (a linked worktree's `.git`
/// file points at its own HEAD). `None` for a detached head or no repository.
pub fn git_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_head_path(root)?).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

/// The `HEAD` file of `root`'s checkout (a linked worktree's lives in its gitdir).
pub fn git_head_path(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let dir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        let dir = if dir.is_absolute() {
            dir
        } else {
            root.join(dir)
        };
        Some(dir.join("HEAD"))
    } else {
        Some(dot.join("HEAD"))
    }
}

/// How often a [`FileCache`] looks at its file's modification time.
pub const FILE_RECHECK: Duration = Duration::from_secs(1);

/// Values read from files, read again when the file's modification time changes
/// (the branch and `run.json` were read once and never again). The
/// time is looked at no more than once per `recheck`, so a frame costs at most
/// one `stat` per value.
pub struct FileCache<T> {
    recheck: Duration,
    entries: HashMap<PathBuf, (Option<std::time::SystemTime>, Instant, T)>,
}

impl<T: Clone> FileCache<T> {
    pub fn new(recheck: Duration) -> Self {
        Self {
            recheck,
            entries: HashMap::new(),
        }
    }

    /// The value for `watch` (the file whose changes invalidate it), loaded by
    /// `load` on first use and whenever the file's modification time moved.
    pub fn get(&mut self, watch: &Path, load: impl FnOnce() -> T) -> T {
        use std::collections::hash_map::Entry;
        let now = Instant::now();
        let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        match self.entries.entry(watch.to_path_buf()) {
            Entry::Vacant(v) => {
                let value = load();
                v.insert((mtime(watch), now, value.clone()));
                value
            }
            Entry::Occupied(mut o) => {
                let (seen, checked, value) = o.get_mut();
                if now.duration_since(*checked) >= self.recheck {
                    *checked = now;
                    let m = mtime(watch);
                    if m != *seen {
                        *seen = m;
                        *value = load();
                    }
                }
                value.clone()
            }
        }
    }
}

/// A folder path the user typed: `~` and `~/...` expand to `home`; an absolute
/// path is taken as is; a relative path is refused, and the error says why (the
/// app's working directory is not a place the user chose). Pure.
pub fn resolve_typed_path(typed: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let t = typed.trim();
    let home_or = |rest: &str| {
        home.map(|h| {
            if rest.is_empty() {
                h.to_path_buf()
            } else {
                h.join(rest)
            }
        })
        .ok_or_else(|| format!("`{t}` starts with ~, and there is no home folder to expand it to"))
    };
    if t == "~" {
        return home_or("");
    }
    if let Some(rest) = t.strip_prefix("~/") {
        return home_or(rest);
    }
    // Windows users type `~\src\p` as often as `~/src/p`.
    if cfg!(windows)
        && let Some(rest) = t.strip_prefix("~\\")
    {
        return home_or(rest);
    }
    let p = PathBuf::from(t);
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(format!(
            "`{t}` is a relative path, which is refused: give a full path, or start it \
             with ~ for your home folder"
        ))
    }
}

/// Where a project's `run.json` is.
pub fn run_json_path(root: &Path) -> PathBuf {
    root.join(".kgraf")
        .join(crate::scheduler_bg::RUN_CONFIG_FILE)
}

/// Reads a project's `.kgraf/run.json` for the routes and the planner.
pub fn read_run_json(root: &Path) -> RunJson {
    let path = run_json_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return RunJson {
                error: Some(format!("{}: {e}", path.display())),
                ..RunJson::default()
            };
        }
    };
    let strings = |v: &serde_json::Value, key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => RunJson {
            routes: strings(&v, "routes"),
            planner: v.get("planner").and_then(|x| x.as_str()).map(str::to_owned),
            model_routes: strings(&v, "model_routes"),
            error: None,
        },
        Err(e) => RunJson {
            error: Some(format!("{}: {e}", path.display())),
            ..RunJson::default()
        },
    }
}

/// The filter chips of a model: the crates (or top-level folders) with code,
/// as (label, search query), in name order. The status chips come first and are
/// fixed.
pub fn crate_chips(model: &kgraf_galaxy::ProjectModel) -> Vec<(String, String)> {
    let mut seen = std::collections::BTreeSet::new();
    for (_, _, path, _, _) in model.code_nodes() {
        let mut parts = path.split('/');
        let first = parts.next().unwrap_or("");
        let key = match (first, parts.next()) {
            ("crates" | "packages" | "libs", Some(name)) if parts.next().is_some() => {
                format!("{first}/{name}/")
            }
            (dir, Some(_)) => format!("{dir}/"),
            _ => continue,
        };
        seen.insert(key);
    }
    seen.into_iter()
        .map(|k| {
            let label = k
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(&k)
                .to_owned();
            (label, format!("file:{k}"))
        })
        .collect()
}

/// The fixed status chips: (label, query). Each is a search, so its result is a
/// selection on the graph (0032).
pub const STATUS_CHIPS: [(&str, &str); 5] = [
    ("In flight", "status:running"),
    ("Blocked", "status:blocked"),
    ("Held", "status:held"),
    ("Waiting", "status:waiting"),
    ("Done", "status:done"),
];

/// A layer Escape can close (INT-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// The settings dialog.
    Dialog,
    /// An open menu: the notifications, or the add-project form.
    Menu,
    /// The selection on the stage.
    Selection,
    /// An active filter chip.
    Filter,
    /// A typed search.
    Search,
}

/// Which layers are open now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Layers {
    pub dialog: bool,
    pub menu: bool,
    pub selection: bool,
    pub filter: bool,
    pub search: bool,
}

/// The one layer an Escape press closes (INT-3, 1.2): the top one, in the order
/// dialog, menu, selection, filter, search. Tooltips close on their own. Pure.
pub fn escape_layer(l: Layers) -> Option<Layer> {
    [
        (l.dialog, Layer::Dialog),
        (l.menu, Layer::Menu),
        (l.selection, Layer::Selection),
        (l.filter, Layer::Filter),
        (l.search, Layer::Search),
    ]
    .into_iter()
    .find_map(|(open, layer)| open.then_some(layer))
}

/// Whether `query` is one of the filter chips' queries.
pub fn is_chip_query(query: &str) -> bool {
    STATUS_CHIPS.iter().any(|(_, q)| *q == query) || query.starts_with("file:")
}

/// The words on the scheduler control for a play action.
pub fn scheduler_words(action: &PlayAction) -> (&'static str, bool, bool, String) {
    match action {
        PlayAction::Stop => (
            "Scheduler running",
            true,
            true,
            "Pause the scheduler: the workers are cancelled and the run stays resumable".to_owned(),
        ),
        PlayAction::Resume(run) => (
            "Scheduler paused",
            false,
            true,
            format!("Resume the scheduler: run {run} continues where it stopped"),
        ),
        PlayAction::Disabled(reason) => ("Scheduler paused", false, false, reason.clone()),
    }
}

/// One notice line from a batch of notices collected since the last frame:
/// the scrubber's refused-credential notices (decision 0071) and the rate limiter's mismatch/lock
/// notices (decision 0072 follow-up 1) both drain into this. Names kinds or routes, never secret
/// values.
pub(crate) fn refusal_notice(notices: Vec<String>) -> Option<String> {
    match notices.len() {
        0 => None,
        1 => notices.into_iter().next(),
        n => Some(format!("{} (and {} more; see the log)", notices[0], n - 1)),
    }
}

/// Whether an update needs the user: a release is offered and not yet accepted. It
/// counts on the bell, like an approval.
pub(crate) fn update_needs_you(state: &Option<UpdateState>) -> bool {
    matches!(state, Some(UpdateState::Available(_)))
}

/// The bell's update row for `state`: the words and, for an offered release, the
/// candidate the "Download and install" button accepts. `None` while there is
/// nothing to say (idle, checking, up to date).
pub(crate) fn update_notice(
    state: &Option<UpdateState>,
) -> Option<(String, Option<crate::update_check::UpdateCandidate>)> {
    match state.as_ref()? {
        UpdateState::Available(c) => Some((
            format!("kGraf {} is available.", c.version),
            Some(c.clone()),
        )),
        UpdateState::Downloading => Some(("Downloading the update...".to_owned(), None)),
        UpdateState::Ready { words, .. } => Some((words.clone(), None)),
        UpdateState::Failed(message) => Some((format!("Update failed: {message}"), None)),
        UpdateState::Idle | UpdateState::Checking | UpdateState::UpToDate => None,
    }
}

impl ShellApp {
    /// Draws one frame of the window.
    pub(crate) fn frame_ui(&mut self, ui: &mut Ui, frame: &eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = Palette::current(&ctx);
        let t = self.started.elapsed().as_secs_f32();
        self.chrome.animating = None;
        self.workspace.tick(&ctx);
        self.take_new_project_offers();
        if let Some(e) = self.workspace.take_close_error() {
            self.chrome.notice = Some((e, Instant::now()));
        }
        if let Some(said) = refusal_notice(kgraf_secrets::take_refusal_notices()) {
            // Also in the log, which env_logger's filter may hide.
            eprintln!("warning: {said}");
            self.chrome.notice = Some((said, Instant::now()));
        }
        // Decision 0072 follow-up 1: a rate-limit mismatch, a registration
        // failure, or a stuck shared-store lock, all previously only an
        // `eprintln!` the desktop shell never shows, polled the same way as
        // the scrubber's refusal notices just above.
        if let Some(said) = refusal_notice(kgraf_provider::take_rate_limit_notices()) {
            eprintln!("warning: {said}");
            self.chrome.notice = Some((said, Instant::now()));
        }
        // The gate writers (`sync_capacity_sources`, `sync_parallelism` and
        // each project's own `sync_parallelism`) run in `App::logic`, not
        // here: a hidden or covered window gets no `ui` pass.
        for proj in self.workspace.projects_mut() {
            // Work status on the graph, from the run records (taken only once the
            // galaxy exists, so the first frame's overlay is not lost).
            if let (Some(g), Some(i)) = (proj.galaxy.as_mut(), proj.interaction.as_mut())
                && let Some((plan, notes)) = i.take_plan()
            {
                g.set_plan(plan);
                g.set_agent_notes(notes);
            }
            // Sessions on the graph (decision 0067): every open project tails its
            // own session graph, the same as the work status just above, so switching
            // to it never shows a stale galaxy. `session_feed` gates the SQLite read on
            // the change log's watermark off this thread, so a taken graph is always a
            // real change, applied once here and not reread until the feed sends
            // another.
            if proj.sessions.is_none() && proj.problem.is_none() {
                let wake = ctx.clone();
                match crate::session_feed::SessionFeed::spawn(
                    proj.root.clone(),
                    crate::session_feed::POLL,
                    move || wake.request_repaint(),
                ) {
                    Ok(f) => proj.sessions = Some(f),
                    Err(e) => {
                        proj.session_error = Some(format!("the session tail could not start: {e}"))
                    }
                }
            }
            if let Some(g) = proj.galaxy.as_mut()
                && let Some(graph) = proj.session_graph.take()
            {
                g.set_sessions(&graph);
            }
        }
        // A changed color vision reaches every open stage, not only the next opened.
        let vision = self.stage_vision();
        sync_stage_vision(
            &mut self.chrome.stage_vision,
            vision,
            self.workspace
                .projects_mut()
                .iter_mut()
                .filter_map(|p| p.galaxy.as_mut()),
        );
        // INT-3: Escape is taken here, before any widget (the stage clears its
        // selection on Escape by itself), and closes exactly one layer.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.escape();
        }
        // `/` focuses the search when nothing else has the keyboard.
        if ctx.memory(|m| m.focused().is_none())
            && ctx.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Text(s) if s == "/"))
            })
        {
            self.chrome.focus_search = true;
        }
        self.masthead(ui, &p, t);
        self.nav(ui, &p, t);
        self.secondary(ui, &p);
        self.center(ui, &p, frame);
        self.settings_dialog(&ctx, &p);
        self.bell_menu(&ctx, &p);
        if let Some(proj) = self.workspace.active_mut()
            && let Some(i) = proj.interaction.as_mut()
        {
            i.run_search(Some(&ctx), proj.galaxy.as_mut());
        }
        if let Some((_, _, at)) = &self.chrome.flash {
            let left = Duration::from_millis(1600).saturating_sub(at.elapsed());
            if left.is_zero() {
                self.chrome.flash = None;
            } else {
                self.chrome.animate(left);
            }
        }
        if let Some(after) = self.chrome.animating {
            ctx.request_repaint_after(after);
        }
        self.sync_project_settings();
    }

    /// The layers open now (INT-3).
    pub(crate) fn layers(&self) -> Layers {
        let proj = self.workspace.active();
        let query = proj
            .and_then(|p| p.interaction.as_ref())
            .map(|i| i.search.query.trim().to_owned())
            .unwrap_or_default();
        Layers {
            dialog: self.chrome.settings_tab.is_some(),
            menu: self.chrome.bell_open || self.chrome.add_open,
            selection: self.selected().slot.is_some(),
            filter: !query.is_empty() && is_chip_query(&query),
            search: !query.is_empty(),
        }
    }

    /// Closes the top layer (INT-3): one per Escape press.
    pub(crate) fn escape(&mut self) {
        match escape_layer(self.layers()) {
            Some(Layer::Dialog) => self.chrome.settings_tab = None,
            Some(Layer::Menu) => {
                self.chrome.bell_open = false;
                self.chrome.close_add_form();
            }
            Some(Layer::Selection) => {
                if let Some(v) = self
                    .workspace
                    .active_mut()
                    .and_then(|p| p.galaxy.as_mut())
                    .and_then(ProjectView::view_mut)
                {
                    v.galaxy_mut().select(None);
                }
            }
            Some(Layer::Filter | Layer::Search) => {
                if let Some(i) = self
                    .workspace
                    .active_mut()
                    .and_then(|p| p.interaction.as_mut())
                {
                    i.search.query.clear();
                }
            }
            None => {}
        }
    }

    // ---------------------------------------------------------------- masthead

    fn masthead(&mut self, ui: &mut Ui, p: &Palette, t: f32) {
        egui::Panel::top("masthead")
            .exact_size(size::MASTHEAD_H)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.base).inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 0,
            }))
            .show(ui, |ui| {
                // LAY-8: under 1360 points, long labels take their short form.
                self.chrome.compact = ui.ctx().content_rect().width() < 1360.0;
                ui.horizontal_centered(|ui| {
                    self.masthead_start(ui, p, t);
                    let start_right = ui.cursor().min.x;
                    let mid_y = ui.max_rect().center().y;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.masthead_end(ui, p);
                        // LAY-4: the search is centered between the two groups, at
                        // most 732 points wide, with a 24 point margin (12 when
                        // compact, LAY-8).
                        let end_left = ui.cursor().max.x;
                        let margin = if self.chrome.compact { 12.0 } else { 24.0 };
                        let (lo, hi) = (start_right + margin, end_left - margin);
                        let w = (hi - lo).min(size::SEARCH_MAX_W + size::SEARCH_BTN_W);
                        if w >= 120.0 {
                            let rect = Rect::from_min_size(
                                pos2(lo + (hi - lo - w) * 0.5, mid_y - size::SEARCH_H * 0.5),
                                vec2(w, size::SEARCH_H),
                            );
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(rect)
                                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                |ui| self.search_box(ui, p, w),
                            );
                        }
                    });
                });
            });
    }

    fn masthead_start(&mut self, ui: &mut Ui, p: &Palette, t: f32) {
        let aggregate = self.workspace.hamburger();
        let look = hamburger_look(
            aggregate,
            self.settings.guide_open,
            self.chrome.hamburger_hovered,
            self.settings.reduce_motion,
        );
        let mut color = look.indicator.map(|i| p.indicator(i)).unwrap_or(p.text);
        if look.blink {
            color = color.gamma_multiply(crate::projects::blink_alpha(t));
            self.chrome.animate(motion::BLINK_FRAME);
        }
        let angle = if look.wiggle {
            let phase = t.rem_euclid(motion::WIGGLE_PERIOD_S);
            if phase < motion::WIGGLE_S {
                self.chrome.animate(Duration::from_millis(16));
            } else {
                self.chrome
                    .animate(Duration::from_secs_f32(motion::WIGGLE_PERIOD_S - phase));
            }
            wiggle_degrees(t).to_radians()
        } else {
            0.0
        };
        let tip = if self.settings.guide_open {
            "Collapse the guide".to_owned()
        } else {
            // CMP-BDG-4: the projects by name, with the kinds and their counts.
            self.workspace.hamburger_words()
        };
        let resp = chrome::icon_button_ex(
            ui,
            p,
            icons::MENU,
            &tip,
            look.indicator.map(|_| color),
            angle,
        );
        self.chrome.hamburger_hovered = resp.hovered();
        if resp.clicked() {
            self.settings.guide_open = !self.settings.guide_open;
        }
        ui.add_space(8.0);
        // The kGraf mark and the branch as a superscript.
        let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
        chrome::kgraf_mark(ui.painter(), rect, p.text);
        ui.add_space(2.0);
        let word = ui.label(
            // Wordmark: 700 20, letter spacing -0.6 (the type scale).
            RichText::new("kGraf")
                .font(theme::bold(20.0))
                .extra_letter_spacing(-0.6)
                .color(p.text),
        );
        let branch = self.workspace.active().and_then(|proj| {
            let head = git_head_path(&proj.root)?;
            self.chrome.branches.get(&head, || git_branch(&proj.root))
        });
        if let Some(b) = branch.filter(|_| !self.chrome.compact) {
            let at = pos2(word.rect.right() + 2.0, word.rect.top() + 2.0);
            let galley = ui
                .painter()
                .layout_no_wrap(b.clone(), theme::regular(10.0), p.text_2);
            // A long branch is cut at 120 points; the tooltip has it whole.
            let w = galley.size().x.min(120.0);
            let (r, resp) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
            ui.painter()
                .with_clip_rect(Rect::from_min_size(at, vec2(w, 14.0)))
                .galley(at, galley, p.text_2);
            let _ = r;
            resp.on_hover_text(format!("branch {b}"));
            ui.add_space(6.0);
        }
        if let Some(proj) = self.workspace.active() {
            ui.add_space(8.0);
            let (line, _) = ui.allocate_exact_size(vec2(1.0, 24.0), Sense::hover());
            ui.painter().rect_filled(line, 0.0, p.outline);
            ui.add_space(8.0);
            // LAY-8: the context label keeps a short form; cut at 180 points.
            ui.add_sized(
                vec2(if self.chrome.compact { 120.0 } else { 180.0 }, 24.0),
                egui::Label::new(
                    RichText::new(proj.name())
                        .font(theme::medium(16.0))
                        .color(p.text),
                )
                .truncate(),
            )
            .on_hover_text(proj.root.display().to_string());
        }
    }

    fn search_box(&mut self, ui: &mut Ui, p: &Palette, w: f32) {
        let focus = std::mem::take(&mut self.chrome.focus_search);
        let (rect, _) = ui.allocate_exact_size(vec2(w, size::SEARCH_H), Sense::hover());
        let input = Rect::from_min_max(
            rect.min,
            pos2(rect.right() - size::SEARCH_BTN_W, rect.bottom()),
        );
        let button = Rect::from_min_max(pos2(input.right(), rect.top()), rect.max);
        let Some(proj) = self.workspace.active_mut() else {
            ui.painter().rect(
                input,
                egui::CornerRadius {
                    nw: 20,
                    sw: 20,
                    ne: 0,
                    se: 0,
                },
                p.search_bg,
                Stroke::new(1.0, p.search_border),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                input.left_center() + vec2(16.0, 0.0),
                egui::Align2::LEFT_CENTER,
                "Open a project to search its graph",
                theme::regular(14.0),
                p.text_dis,
            );
            return;
        };
        let Some(i) = proj.interaction.as_mut() else {
            return;
        };
        let text_rect = Rect::from_min_max(
            input.min + vec2(44.0, 0.0),
            input.max
                - vec2(
                    if i.search.query.is_empty() {
                        16.0
                    } else {
                        120.0
                    },
                    0.0,
                ),
        );
        let resp = ui.put(
            text_rect,
            egui::TextEdit::singleline(&mut i.search.query)
                // CMP-SRCH-1 (1.2): text 400 16/24, placeholder `--text-2`.
                .hint_text(
                    RichText::new(
                        "Search symbols, agents, conversations: agent: status: file: \"phrase\"",
                    )
                    .color(p.text_2),
                )
                .font(theme::regular(16.0))
                .frame(egui::Frame::NONE)
                .vertical_align(egui::Align::Center)
                .desired_width(text_rect.width()),
        );
        let resp = resp.on_hover_text(crate::interaction::SEARCH_HELP);
        if focus {
            resp.request_focus();
        }
        if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            i.search.query.clear();
        }
        let border = if resp.has_focus() {
            p.search_focus
        } else {
            p.search_border
        };
        ui.painter().rect_stroke(
            input,
            egui::CornerRadius {
                nw: 20,
                sw: 20,
                ne: 0,
                se: 0,
            },
            Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
        glyph(
            ui.painter(),
            input.left_center() + vec2(22.0, 0.0),
            icons::SEARCH,
            20.0,
            p.text_2,
            0.0,
        );
        // The count, or the parse error in the error color.
        let s = &i.search;
        if let Some(e) = &s.error {
            resp.clone().on_hover_text(e.clone());
            ui.painter().text(
                pos2(input.right() - 44.0, input.center().y),
                egui::Align2::RIGHT_CENTER,
                "not a query",
                theme::regular(12.0),
                p.danger,
            );
        } else if !s.ran().trim().is_empty() {
            let n = s.results.hits.len();
            let words = if n == 0 && !s.searching() {
                "No matches".to_owned()
            } else {
                format!("{n} lit")
            };
            let color = p.text_2;
            let label = ui.put(
                Rect::from_min_max(
                    pos2(input.right() - 116.0, input.top()),
                    pos2(input.right() - 44.0, input.bottom()),
                ),
                egui::Label::new(RichText::new(words).small().color(color)).truncate(),
            );
            label.on_hover_text(crate::interaction::results_line(&s.results, s.searching()));
        }
        if !i.search.query.is_empty() {
            let clear = ui.put(
                Rect::from_center_size(
                    pos2(input.right() - 22.0, input.center().y),
                    vec2(32.0, 32.0),
                ),
                egui::Button::new(icons::text(icons::CLOSE, 20.0).color(p.text)).frame(false),
            );
            if clear.on_hover_text("Clear search").clicked() {
                i.search.query.clear();
            }
        }
        let b = ui.interact(button, ui.id().with("search_go"), Sense::click());
        let fill = if b.hovered() {
            p.chip_hover
        } else {
            p.search_btn
        };
        ui.painter().rect(
            button,
            egui::CornerRadius {
                nw: 0,
                sw: 0,
                ne: 20,
                se: 20,
            },
            fill,
            Stroke::new(1.0, p.search_btn_border),
            egui::StrokeKind::Inside,
        );
        glyph(
            ui.painter(),
            button.center(),
            icons::SEARCH,
            24.0,
            p.text,
            0.0,
        );
        if b.on_hover_text("Search (/)").clicked() {
            resp.request_focus();
        }
    }

    fn masthead_end(&mut self, ui: &mut Ui, p: &Palette) {
        // Right to left: the bell, the render badge, the scheduler pill.
        let bell_count =
            self.workspace.bell() + usize::from(update_needs_you(&self.update_state()));
        let bell = chrome::icon_button(
            ui,
            p,
            if bell_count > 0 {
                icons::BELL_FILL
            } else {
                icons::BELL
            },
            &format!("Notifications, {bell_count} pending"),
        );
        if bell_count > 0 {
            chrome::badge(ui.painter(), bell.rect.min + vec2(20.0, 2.0), bell_count, p);
        }
        if bell.clicked() {
            self.chrome.bell_open = !self.chrome.bell_open;
        }
        ui.add_space(8.0);
        // The render badge: the CPU path is a stated mode, never silent (0054).
        let cpu = self.render.path.is_software();
        let label = match (self.chrome.compact, cpu) {
            (true, true) => "CPU rendering".to_owned(),
            (true, false) => "GPU".to_owned(),
            _ => self.render.label(),
        };
        let (bg, fg, icon_color) = if cpu {
            (p.danger_bg, p.danger, p.danger)
        } else {
            (p.additive, p.text, p.text)
        };
        let galley = ui
            .painter()
            .layout_no_wrap(label.clone(), theme::medium(12.0), fg);
        let bw = (galley.size().x + 38.0).min(260.0);
        let (brect, bresp) = ui.allocate_exact_size(vec2(bw, 32.0), Sense::hover());
        ui.painter().rect_filled(brect, 8.0, bg);
        glyph(
            ui.painter(),
            brect.left_center() + vec2(17.0, 0.0),
            icons::MEMORY,
            18.0,
            icon_color,
            0.0,
        );
        let clip = brect.shrink2(vec2(4.0, 0.0));
        ui.painter().with_clip_rect(clip).galley(
            pos2(
                brect.left() + 30.0,
                brect.center().y - galley.size().y * 0.5,
            ),
            galley,
            fg,
        );
        bresp.on_hover_text(self.render.facts.summary());
        ui.add_space(8.0);
        // The scheduler pill: "n of m workers" | "Scheduler running/paused".
        let (in_flight, action) = match self.workspace.active().and_then(|p| p.interaction.as_ref())
        {
            Some(i) => (
                i.host().admission().map_or(0, |a| a.gate().in_flight().0),
                i.play_action(),
            ),
            None => (
                0,
                PlayAction::Disabled("no project is open: add one from the guide".to_owned()),
            ),
        };
        let cap = self.settings.caps.work_items();
        let workers = format!("{in_flight} of {cap} workers");
        let wg = ui
            .painter()
            .layout_no_wrap(workers.clone(), theme::medium(14.0), p.text);
        let (wrect, wresp) = ui.allocate_exact_size(vec2(wg.size().x + 28.0, 36.0), Sense::hover());
        let (words, running, enabled, hint) = scheduler_words(&action);
        // LAY-8 short form, still naming the state (CMP-STAT-2).
        let words = match (self.chrome.compact, running) {
            (true, true) => "Running",
            (true, false) => "Paused",
            _ => words,
        };
        let sg = ui
            .painter()
            .layout_no_wrap(words.to_owned(), theme::medium(14.0), p.text);
        let (srect, sresp) = ui.allocate_exact_size(
            vec2(sg.size().x + 44.0, 36.0),
            if enabled {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        let whole = srect.union(wrect);
        ui.painter().rect_filled(whole, 18.0, p.additive);
        for (r, resp, left) in [(srect, &sresp, true), (wrect, &wresp, false)] {
            if resp.hovered() && enabled && left {
                let cr = if left {
                    egui::CornerRadius {
                        nw: 18,
                        sw: 18,
                        ne: 0,
                        se: 0,
                    }
                } else {
                    egui::CornerRadius {
                        nw: 0,
                        sw: 0,
                        ne: 18,
                        se: 18,
                    }
                };
                ui.painter().rect_filled(r, cr, p.chip_hover);
            }
        }
        ui.painter().rect_filled(
            Rect::from_center_size(pos2(srect.right(), srect.center().y), vec2(1.0, 24.0)),
            0.0,
            p.outline,
        );
        let fg = if enabled { p.text } else { p.text_dis };
        // CMP-STAT-2 (1.2): a 20 px pause icon while running, a play icon while
        // paused. The label names the state; the tooltip names the action.
        glyph(
            ui.painter(),
            srect.left_center() + vec2(20.0, 0.0),
            if running { icons::PAUSE } else { icons::PLAY },
            20.0,
            fg,
            0.0,
        );
        ui.painter().galley(
            pos2(srect.left() + 34.0, srect.center().y - sg.size().y * 0.5),
            sg,
            fg,
        );
        ui.painter().galley(
            pos2(wrect.left() + 14.0, wrect.center().y - wg.size().y * 0.5),
            wg,
            p.text,
        );
        sresp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, words));
        wresp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &workers));
        let sresp = sresp.on_hover_text(&hint).on_disabled_hover_text(&hint);
        if sresp.clicked()
            && let Some(i) = self
                .workspace
                .active_mut()
                .and_then(|p| p.interaction.as_mut())
        {
            i.play();
        }
        // CMP-NAV-7 (1.5): the readout shows the cap and opens nothing; the rail
        // is the only way into settings.
        wresp.on_hover_text(
            "Work items in flight in this project, of the concurrency cap. The cap is in \
             Settings, from the rail.",
        );
    }

    // ---------------------------------------------------------------- nav

    fn nav(&mut self, ui: &mut Ui, p: &Palette, t: f32) {
        let open = self.settings.guide_open;
        egui::Panel::left("nav")
            .exact_size(if open { size::NAV_FULL } else { size::NAV_MINI })
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(p.base)
                    .inner_margin(if open { 12 } else { 4 }),
            )
            .show(ui, |ui| {
                if open {
                    egui::ScrollArea::vertical()
                        .id_salt("nav_full")
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.nav_full(ui, p, t));
                } else {
                    self.nav_mini(ui, p);
                }
            });
    }

    fn nav_mini(&mut self, ui: &mut Ui, p: &Palette) {
        ui.spacing_mut().item_spacing.y = 0.0;
        let graph = self.chrome.nav == Nav::Graph;
        let entries = [
            (
                if graph {
                    icons::GRAPH_FILL
                } else {
                    icons::GRAPH
                },
                "Graph",
            ),
            (icons::SEARCH, "Search"),
            (icons::PROVIDERS, "Providers"),
            (icons::SETTINGS, "Settings"),
        ];
        for (icon, label) in entries {
            let current = match label {
                "Graph" => graph,
                "Search" => self.chrome.nav == Nav::Search,
                _ => false,
            };
            if chrome::mini_entry(ui, p, icon, label, current).clicked() {
                self.nav_to(label);
            }
        }
    }

    fn nav_to(&mut self, label: &str) {
        match label {
            "Graph" => self.chrome.nav = Nav::Graph,
            "Search" => {
                self.chrome.nav = Nav::Search;
                self.chrome.focus_search = true;
            }
            "Providers" => self.chrome.settings_tab = Some(SettingsTab::Providers),
            "Appearance" => self.chrome.settings_tab = Some(SettingsTab::Appearance),
            _ => self.chrome.settings_tab = Some(SettingsTab::General),
        }
    }

    fn nav_full(&mut self, ui: &mut Ui, p: &Palette, t: f32) {
        ui.spacing_mut().item_spacing.y = 2.0;
        chrome::guide_title(ui, p, "Projects");
        let mut switch_to = None;
        let mut close = None;
        let active = self.workspace.active_index();
        for (idx, proj) in self.workspace.projects().iter().enumerate() {
            let status = proj.status();
            let line = proj.state_line();
            let (rect, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
            let current = active == Some(idx);
            if current || resp.hovered() {
                ui.painter()
                    .rect_filled(rect, theme::radius::NAV_ENTRY, p.additive);
            }
            let tile =
                Rect::from_min_size(rect.left_center() + vec2(12.0, -14.0), vec2(28.0, 28.0));
            ui.painter().rect_filled(tile, 6.0, p.outline_opaque);
            ui.painter().text(
                tile.center(),
                egui::Align2::CENTER_CENTER,
                proj.letter(),
                theme::medium(13.0),
                p.text,
            );
            let text_left = tile.right() + 12.0;
            let text_right = rect.right() - 28.0;
            let clip = ui.painter().with_clip_rect(Rect::from_min_max(
                pos2(text_left, rect.top()),
                pos2(text_right, rect.bottom()),
            ));
            clip.text(
                pos2(text_left, rect.center().y - 9.0),
                egui::Align2::LEFT_CENTER,
                proj.name(),
                theme::medium(14.0),
                p.text,
            );
            let line_color = if proj.problem.is_some() {
                p.danger
            } else {
                p.text_2
            };
            clip.text(
                pos2(text_left, rect.center().y + 9.0),
                egui::Align2::LEFT_CENTER,
                &line,
                theme::regular(12.0),
                line_color,
            );
            if let Some(ind) = status.indicator() {
                let blink = status.blinks() && !self.settings.reduce_motion;
                chrome::dot(
                    ui.painter(),
                    pos2(rect.right() - 16.0, rect.center().y),
                    4.0,
                    p.indicator(ind),
                    blink,
                    t,
                );
                if blink {
                    self.chrome.animate(motion::BLINK_FRAME);
                }
            }
            let name = proj.name();
            let resp = resp.on_hover_text(format!(
                "{}\n{}: {}",
                proj.root.display(),
                name,
                status.words()
            ));
            resp.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    true,
                    current,
                    format!("{name}, {}. {line}", status.words()),
                )
            });
            if resp.clicked() {
                switch_to = Some(idx);
            }
            resp.context_menu(|ui| {
                if ui.button("Close project").clicked() {
                    close = Some(idx);
                    ui.close();
                }
            });
        }
        if chrome::guide_entry(ui, p, icons::ADD, "Add project", self.chrome.add_open).clicked() {
            if self.chrome.add_open {
                self.chrome.close_add_form();
            } else {
                self.chrome.add_open = true;
            }
            self.chrome.add_error = None;
        }
        if self.chrome.add_open {
            self.add_project_form(ui, p);
        }
        if let Some(i) = switch_to {
            self.workspace.switch_to(i);
            self.chrome.minimap = None;
        }
        if let Some(i) = close {
            match self.workspace.close(i) {
                Ok(_) => self.chrome.minimap = None,
                Err(e) => self.chrome.notice = Some((e, Instant::now())),
            }
        }
        ui.add_space(8.0);
        separator(ui, p);
        let graph = self.chrome.nav == Nav::Graph;
        if chrome::guide_entry(
            ui,
            p,
            if graph {
                icons::GRAPH_FILL
            } else {
                icons::GRAPH
            },
            "Graph",
            graph,
        )
        .clicked()
        {
            self.nav_to("Graph");
        }
        if chrome::guide_entry(
            ui,
            p,
            icons::SEARCH,
            "Search",
            self.chrome.nav == Nav::Search,
        )
        .clicked()
        {
            self.nav_to("Search");
        }
        ui.add_space(8.0);
        separator(ui, p);
        chrome::guide_title(ui, p, "Configuration");
        for (icon, label) in [
            (icons::PROVIDERS, "Providers"),
            (icons::DARK_MODE, "Appearance"),
            (icons::SETTINGS, "Settings"),
        ] {
            if chrome::guide_entry(ui, p, icon, label, false).clicked() {
                self.nav_to(label);
            }
        }
        ui.add_space(8.0);
        separator(ui, p);
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!(
                "kGraf {}. Local only, single user. No telemetry, no account.",
                env!("CARGO_PKG_VERSION")
            ))
            .small()
            .color(p.text_2),
        );
        ui.label(
            RichText::new("The graph is the only work surface.")
                .small()
                .color(p.text_dis),
        );
    }

    fn add_project_form(&mut self, ui: &mut Ui, p: &Palette) {
        ui.add_space(4.0);
        // The picker's answer, when it came (it runs off the UI thread).
        match self.chrome.picker.poll() {
            Some(PickerAnswer::Chosen(dir)) => self.add_project(&dir),
            Some(PickerAnswer::Nothing) => {
                self.chrome.add_error =
                    Some("No folder chosen. If no dialog opened, type the path below.".to_owned());
            }
            Some(PickerAnswer::Failed) => {
                self.chrome.add_error =
                    Some("The folder dialog failed; type the path below.".to_owned());
            }
            None => {}
        }
        let picking = self.chrome.picker.busy();
        let label = if picking {
            "Choosing a folder..."
        } else {
            "Choose a folder..."
        };
        if chrome::tonal_button(ui, p, label).clicked() && !picking {
            // The native dialog (rfd: AppKit, the Windows common dialog, or the
            // XDG desktop portal), awaited on a thread of its own so the window
            // keeps drawing and every project keeps polling. A missing
            // portal returns nothing; the path field below is the fallback.
            let dialog = rfd::AsyncFileDialog::new()
                .set_title("Add a project folder")
                .pick_folder();
            let (tx, rx) = std::sync::mpsc::channel();
            let wake = ui.ctx().clone();
            let spawned = std::thread::Builder::new()
                .name("folder-picker".into())
                .spawn(move || {
                    let dir = pollster::block_on(dialog).map(|h| h.path().to_path_buf());
                    let _ = tx.send(dir);
                    wake.request_repaint();
                });
            match spawned {
                Ok(_) => self.chrome.picker.start(rx),
                Err(e) => self.chrome.add_error = Some(format!("the folder dialog: {e}")),
            }
        }
        ui.add_space(4.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.chrome.add_path)
                .hint_text("or type a folder path (~ for home)")
                .desired_width(ui.available_width()),
        );
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (chrome::tonal_button(ui, p, "Open").clicked() || enter)
            && !self.chrome.add_path.trim().is_empty()
        {
            let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
            match resolve_typed_path(&self.chrome.add_path, home.as_deref()) {
                Ok(path) => self.add_project(&path),
                Err(e) => self.chrome.add_error = Some(e),
            }
        }
        if let Some(e) = &self.chrome.add_error {
            ui.label(RichText::new(e).small().color(p.danger));
        }
    }

    fn add_project(&mut self, dir: &Path) {
        match self.open_project(dir) {
            Ok(_) => {
                self.chrome.close_add_form();
                self.chrome.add_path.clear();
                self.chrome.add_error = None;
                self.chrome.minimap = None;
            }
            Err(e) => self.chrome.add_error = Some(e),
        }
    }

    // ---------------------------------------------------------------- secondary

    fn secondary(&mut self, ui: &mut Ui, p: &Palette) {
        let width = size::secondary_w(ui.ctx().content_rect().width()) + size::COLUMN_GAP;
        egui::Panel::right("secondary")
            .exact_size(width)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.base).inner_margin(egui::Margin {
                left: 0,
                right: 24,
                top: 12,
                bottom: 12,
            }))
            .show(ui, |ui| {
                let h = ui.available_height();
                let up = ((h - 12.0) * 0.6).max(120.0);
                let selected = self.selected();
                ui.allocate_ui(vec2(ui.available_width(), up), |ui| {
                    card(ui, p, |ui| self.up_next(ui, p, &selected));
                });
                ui.add_space(12.0);
                ui.allocate_ui(vec2(ui.available_width(), ui.available_height()), |ui| {
                    card(ui, p, |ui| self.live_activity(ui, p));
                });
            });
    }

    fn up_next(&mut self, ui: &mut Ui, p: &Palette, selected: &Selected) {
        card_head(ui, p, "Up next", None);
        let Some(proj) = self.workspace.active_mut() else {
            empty(ui, p, "Open a project to read its conversations here.");
            return;
        };
        let Some(i) = proj.interaction.as_mut() else {
            empty(
                ui,
                p,
                proj.problem
                    .as_deref()
                    .unwrap_or("This project cannot be shown."),
            );
            return;
        };
        i.on_selection(selected);
        let tab = i.text.tab();
        egui::Frame::new()
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if chrome::chip(ui, p, "Selection", tab == Tab::Selection, None).clicked() {
                        i.text.set_tab(Tab::Selection);
                    }
                    let n = i.search.results.turns.len() + i.search.results.briefs.len();
                    let label = if n > 0 {
                        format!("Search {n}")
                    } else {
                        "Search".to_owned()
                    };
                    if chrome::chip(ui, p, &label, tab == Tab::Search, None).clicked() {
                        i.text.set_tab(Tab::Search);
                    }
                });
            });
        // One rendering: when the feed is an agent's own session,
        // `i.text.show_session` draws the whole thing, transcript included
        // (design 0067: the reasoning line, children links, "Attach as
        // context", composer and stop/resume, plus the conversation itself
        // as its own selectable widget). The plain virtualized path below
        // (`i.text_surface`) is for every other feed (a work-node
        // conversation, an empty selection, search snippets) and is skipped
        // here, so the conversation is never drawn twice (SR "no second
        // place to look") and a session's own transcript is one rendering,
        // not a small box plus the big one below it.
        let is_agent_session = matches!(&i.text.feed, Feed::Conversation(c) if c.agent.is_some());
        //
        // `i.text.show_session` borrows only `i.text` (mutably) and `i.runs`
        // and `i.corpus` (read-only): disjoint fields of `Interaction`, so
        // this does not conflict with `i.box_and_host()`'s whole-`self`
        // borrow just below, because that borrow starts only after
        // `actions` (an owned value) is in hand and this one has ended.
        let actions = if is_agent_session {
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: 16,
                    right: 16,
                    top: 0,
                    bottom: 4,
                })
                .show(ui, |ui| {
                    i.text
                        .show_session(ui, &i.runs.runs, &i.corpus, Some(proj.root.as_path()))
                })
                .inner
        } else {
            crate::text_surface::SessionFrameActions::default()
        };
        if let Some(id) = &actions.select_child {
            let conv =
                crate::text_surface::conversation_for(Some(id), None, &i.runs.runs, &i.corpus);
            i.text.on_selection(conv);
        }
        if let Some(chip) = actions.attach {
            i.box_model.attach(chip);
        }
        let selected_worker = selected.agent.clone();
        if actions.send.is_some()
            || actions.stop.is_some()
            || actions.resume.is_some()
            || actions.redirect_to_orchestrator.is_some()
        {
            let (box_model, host) = i.box_and_host();
            if let Some((session, kind, mode, text)) = actions.send
                && let Err(e) = host.send_to_session(session, kind, mode, text)
            {
                box_model.say(Line::Error(format!("direct message not sent: {e}")));
            }
            if let Some(session) = actions.stop
                && let Err(e) = host.stop_session(session)
            {
                box_model.say(Line::Error(format!("stop not sent: {e}")));
            }
            if let Some(session) = actions.resume
                && let Err(e) = host.resume_session(session)
            {
                box_model.say(Line::Error(format!("resume not sent: {e}")));
            }
            if let Some(text) = actions.redirect_to_orchestrator {
                // The composer already refused the note locally; this
                // routes the same text through the normal orchestrator-box
                // send path (the same one the human composer uses), rather
                // than reinventing delivery.
                box_model.input = text;
                box_model.submit(host, selected_worker.as_deref());
            }
        }
        if let Some(err) = actions.local_error {
            i.box_model.say(Line::Error(err));
        }
        // Skipped for an agent's own session: `show_session` above already
        // drew its transcript (see that call's own comment).
        if !is_agent_session {
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: 16,
                    right: 8,
                    top: 0,
                    bottom: 8,
                })
                .show(ui, |ui| {
                    i.text_surface(ui, proj.galaxy.as_mut());
                });
        }
    }

    fn live_activity(&mut self, ui: &mut Ui, p: &Palette) {
        let ctx = ui.ctx().clone();
        let Some(proj) = self.workspace.active_mut() else {
            card_head(ui, p, "Live activity", Some(("Not live", false)));
            empty(
                ui,
                p,
                "Change-log events of the shown project appear here, newest first.",
            );
            return;
        };
        if proj.activity.is_none() && proj.problem.is_none() {
            let wake = ctx.clone();
            match ActivityWatch::spawn(proj.root.clone(), activity::POLL, move || {
                wake.request_repaint()
            }) {
                Ok(w) => proj.activity = Some(w),
                Err(e) => {
                    proj.activity_error = Some(format!("the activity tail could not start: {e}"))
                }
            }
        }
        // The accent means live: the tail runs and its last read succeeded.
        let live = proj.activity.is_some() && proj.activity_error.is_none();
        card_head(
            ui,
            p,
            "Live activity",
            Some(if live {
                ("Live", true)
            } else {
                ("Not live", false)
            }),
        );
        if let Some(e) = &proj.activity_error {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("   {e}")).small().color(p.text_2));
            return;
        }
        let rows = Arc::clone(&proj.activity_rows);
        if rows.is_empty() {
            empty(ui, p, "No change-log events yet.");
            return;
        }
        // Each row's slot, computed when the rows or the model change, not per
        // frame (a file row scanned every code node each frame).
        let generation = proj.galaxy.as_ref().map_or(0, ProjectView::generation);
        let key = (Arc::as_ptr(&rows) as usize, generation);
        if proj.activity_slots.0 != Some(key) {
            let slots = match proj.galaxy.as_ref().and_then(ProjectView::model) {
                Some(m) => activity::routes(&rows, m),
                None => vec![None; rows.len()],
            };
            proj.activity_slots = (Some(key), slots);
        }
        let slots = proj.activity_slots.1.clone();
        let mut select = None;
        let row_h = 30.0;
        egui::ScrollArea::vertical()
            .id_salt("live_activity")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for (r, slot) in rows[range.clone()].iter().zip(&slots[range]) {
                    let slot = *slot;
                    let sense = if slot.is_some() {
                        Sense::click()
                    } else {
                        Sense::hover()
                    };
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(ui.available_width(), row_h), sense);
                    if slot.is_some() && resp.hovered() {
                        ui.painter().rect_filled(rect, 0.0, p.additive_soft);
                    }
                    let painter = ui.painter().with_clip_rect(rect);
                    painter.text(
                        pos2(rect.left() + 16.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        activity::clock(r.ts_ms),
                        theme::regular(11.0),
                        p.text_3,
                    );
                    let av = Rect::from_center_size(
                        pos2(rect.left() + 90.0, rect.center().y),
                        vec2(24.0, 24.0), // CMP-ROW-3: 52 px time column, 24 px avatar
                    );
                    let letter = r.who.chars().find(|c| c.is_alphanumeric()).unwrap_or('?');
                    avatar(&painter, av, letter, p, None);
                    let mut job = egui::text::LayoutJob::default();
                    let fmt = |font| egui::TextFormat {
                        font_id: font,
                        color: p.text,
                        ..Default::default()
                    };
                    job.append(&r.who, 0.0, fmt(theme::medium(13.0)));
                    job.append(&format!("  {}", r.text), 0.0, fmt(theme::regular(13.0)));
                    job.wrap.max_width = rect.right() - (av.right() + 10.0) - 8.0;
                    job.wrap.max_rows = 1;
                    // Cut at the edge, not at a word: a long object is elided, the
                    // actor and the verb stay (an "indexer ..." row).
                    job.wrap.break_anywhere = true;
                    let galley = painter.layout_job(job);
                    painter.galley(
                        pos2(av.right() + 10.0, rect.center().y - galley.size().y * 0.5),
                        galley,
                        p.text,
                    );
                    let words = format!("{} {} {}", activity::clock(r.ts_ms), r.who, r.text);
                    resp.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, slot.is_some(), &words)
                    });
                    let resp = resp.on_hover_text(&r.text);
                    if slot.is_some() && resp.clicked() {
                        select = slot;
                    }
                }
            });
        if let Some(slot) = select
            && let Some(view) = proj.galaxy.as_mut().and_then(ProjectView::view_mut)
        {
            view.galaxy_mut().select(Some(slot));
            ctx.request_repaint();
        }
    }

    // ---------------------------------------------------------------- center

    fn center(&mut self, ui: &mut Ui, p: &Palette, frame: &eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.base).inner_margin(egui::Margin {
                left: 24,
                right: 24,
                top: 0,
                bottom: 12,
            }))
            .show(ui, |ui| {
                egui::Panel::top("chips")
                    .exact_size(size::CHIPBAR_H)
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| self.chips(ui, p));
                let max_below = (ui.available_height() - 220.0).max(120.0);
                egui::Panel::bottom("below")
                    .resizable(false)
                    .show_separator_line(false)
                    .max_size(max_below)
                    .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                        left: 0,
                        right: 0,
                        top: 12,
                        bottom: 0,
                    }))
                    // Sized to its content (LAY-7, LAY-9): the stage takes the rest,
                    // at least 220 points (LAY-3). The history scrolls on its own.
                    .show(ui, |ui| self.below(ui, p));
                let rect = ui.available_rect_before_wrap();
                self.stage(ui, p, frame, rect);
            });
    }

    fn chips(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(proj) = self.workspace.active_mut() else {
            return;
        };
        let (Some(i), root) = (proj.interaction.as_mut(), proj.root.clone()) else {
            return;
        };
        // Crate chips from the model, rebuilt when its generation moves.
        let generation = proj.galaxy.as_ref().map_or(0, ProjectView::generation);
        let stale =
            !matches!(&self.chrome.crates, Some((r, g, _)) if *r == root && *g == generation);
        if stale && let Some(model) = proj.galaxy.as_ref().and_then(ProjectView::model) {
            self.chrome.crates = Some((root.clone(), generation, crate_chips(model)));
        }
        let crates: &[(String, String)] = match &self.chrome.crates {
            Some((r, _, c)) if *r == root => c,
            _ => &[],
        };
        let query = i.search.query.trim().to_owned();
        let mut fit_all = false;
        egui::ScrollArea::horizontal()
            .id_salt("chipbar")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    if chrome::chip(ui, p, "All", query.is_empty(), None)
                        .on_hover_text("Clear the filter and fit the whole graph")
                        .clicked()
                    {
                        // CMP-CHIP-2: "All" clears the highlight and fits the stage.
                        i.search.query.clear();
                        fit_all = true;
                    }
                    for (label, q) in STATUS_CHIPS {
                        if chrome::chip(ui, p, label, query == q, None)
                            .on_hover_text(format!("Select on the graph: {q}"))
                            .clicked()
                        {
                            i.search.query = if query == q {
                                String::new()
                            } else {
                                q.to_owned()
                            };
                        }
                    }
                    for (label, q) in crates {
                        if chrome::chip(ui, p, label, query == *q, None)
                            .on_hover_text(format!("Select on the graph: {q}"))
                            .clicked()
                        {
                            i.search.query = if query == *q {
                                String::new()
                            } else {
                                q.clone()
                            };
                        }
                    }
                });
            });
        if fit_all && let Some(v) = proj.galaxy.as_mut().and_then(ProjectView::view_mut) {
            v.galaxy_mut().frame_all();
            ui.ctx().request_repaint();
        }
    }

    fn below(&mut self, ui: &mut Ui, p: &Palette) {
        let selected = self.selected();
        let flash = self.chrome.flash.clone();
        let active = self.workspace.active().map(|p| p.root.clone());
        let Some(proj) = self.workspace.active_mut() else {
            return;
        };
        let root = proj.root.clone();
        let name = proj.name();
        let failures = proj.failures();
        let run_json = self
            .chrome
            .run_json
            .get(&run_json_path(&root), || read_run_json(&root));
        // The project row.
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 20.0, p.raised);
            ui.painter()
                .circle_stroke(rect.center(), 19.5, Stroke::new(1.0, p.outline));
            chrome::kgraf_mark(ui.painter(), rect.shrink(9.0), p.text);
            ui.vertical(|ui| {
                // Page title: 700 18/26 (TYP-1, the type scale).
                ui.label(RichText::new(&name).font(theme::bold(18.0)).color(p.text));
                let branch = git_head_path(&root)
                    .and_then(|h| self.chrome.branches.get(&h, || git_branch(&root)));
                let path = match branch {
                    Some(b) => format!("{}  on branch {b}", root.display()),
                    None => root.display().to_string(),
                };
                ui.add(egui::Label::new(RichText::new(path).small().color(p.text_2)).truncate());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let model = run_json
                    .planner
                    .clone()
                    .or_else(|| run_json.routes.first().cloned());
                let (text, tip) = match (&model, &run_json.error) {
                    (Some(m), _) => (
                        format!("orchestrator: {m}"),
                        "The orchestrator's route and model, from .kgraf/run.json".to_owned(),
                    ),
                    (None, Some(e)) => ("orchestrator: not configured".to_owned(), e.clone()),
                    (None, None) => (
                        "orchestrator: no route".to_owned(),
                        ".kgraf/run.json names no route".to_owned(),
                    ),
                };
                let g = ui
                    .painter()
                    .layout_no_wrap(text, theme::medium(12.0), p.text_2);
                let (r, resp) =
                    ui.allocate_exact_size(vec2(g.size().x + 36.0, 28.0), Sense::hover());
                ui.painter().rect_stroke(
                    r,
                    14.0,
                    Stroke::new(1.0, p.outline),
                    egui::StrokeKind::Inside,
                );
                glyph(
                    ui.painter(),
                    r.left_center() + vec2(15.0, 0.0),
                    icons::BOT,
                    16.0,
                    p.text_2,
                    0.0,
                );
                ui.painter().galley(
                    pos2(r.left() + 28.0, r.center().y - g.size().y * 0.5),
                    g,
                    p.text_2,
                );
                resp.on_hover_text(tip);
            });
        });
        ui.add_space(12.0);
        let Some(i) = proj.interaction.as_mut() else {
            ui.label(RichText::new(proj.problem.clone().unwrap_or_default()).color(p.danger));
            return;
        };
        if let Some(e) = &i.runs.error {
            ui.label(
                RichText::new(format!("run records unreadable: {e}"))
                    .small()
                    .color(p.danger),
            );
        }
        // The attribution line: recomputed at most every 2 s, and only
        // on frames that happen anyway (no timer).
        let fresh = matches!(&self.chrome.attribution, Some((r, at, _)) if *r == root && at.elapsed() < Duration::from_secs(2));
        if !fresh {
            let run = match i.host().state() {
                BackendState::Running { run } => Some(run),
                _ => crate::run_watch::newest(&i.runs.runs).map(|r| r.run.to_string()),
            };
            let line = match run {
                Some(run) => match i.host().run_tokens(&kgraf_core::RunId::new(run.clone())) {
                    Some(tokens) => tokens.line(&run),
                    None => format!(
                        "Run {run}: tokens are attributed per provider call in the ledger; \
                         it cannot be read from this window yet"
                    ),
                },
                None => "No run yet. Every token of every provider call is attributed in the \
                         ledger."
                    .to_owned(),
            };
            self.chrome.attribution = Some((root.clone(), Instant::now(), line));
        }
        let attribution = self
            .chrome
            .attribution
            .as_ref()
            .map(|(_, _, l)| l.clone())
            .unwrap_or_default();
        let flash_key = flash.and_then(|(root, key, _)| (Some(root) == active).then_some(key));
        let flash_ask = flash_key
            .as_deref()
            .and_then(|k| k.strip_prefix("ask:"))
            .and_then(|id| id.parse().ok());
        let worker = selected
            .agent
            .as_deref()
            .and_then(|a| crate::run_view::worker_for_holder(&i.runs.runs, a));
        let opts = BoxOptions {
            attribution,
            flash_ask,
            focus_input: false,
        };
        // Pinned failure cards (3): each failure that needs the user,
        // before the approvals, with Dismiss where it can be acknowledged.
        let mut dismissed = None;
        for f in &failures {
            let flash = flash_key.as_deref() == Some(f.key.as_str());
            if failure_card(ui, p, f, flash) {
                dismissed = Some(f.key.clone());
            }
            ui.add_space(8.0);
        }
        let (model, host) = i.box_and_host();
        crate::orchestrator_box::show_styled(ui, model, host, worker.as_deref(), &opts);
        // Decision 0094: the plan waiting for review lights what its contracts
        // write on the graph (CMP-CHIP-2's selection on the stage), once per
        // change; it clears when the plan is answered.
        let highlight = model.perm.take_highlight();
        if let Some(paths) = highlight
            && let Some(view) = proj.galaxy.as_mut()
        {
            let slots = view
                .model()
                .map(|m| crate::permission_bar::slots_for_paths(m, &paths))
                .unwrap_or_default();
            if let Some(v) = view.view_mut() {
                v.galaxy_mut().select_set(&slots);
            }
        }
        if let Some(key) = dismissed
            && let Err(e) = proj.dismiss(&key)
        {
            self.chrome.notice = Some((
                format!("The acknowledgment was not saved: {e}"),
                Instant::now(),
            ));
        }
    }

    // ---------------------------------------------------------------- stage

    fn stage(&mut self, ui: &mut Ui, p: &Palette, frame: &eframe::Frame, rect: Rect) {
        let rs = frame.wgpu_render_state().cloned();
        ui.painter()
            .rect_filled(rect, theme::radius::STAGE, theme::STAGE_BG);
        let white = Color32::WHITE;
        let Some(rs) = rs else {
            center_text(
                ui,
                rect,
                "galaxy unavailable: no wgpu render state",
                p.danger,
            );
            return;
        };
        let budget = if self.render.path.is_software() {
            RenderBudget::Cpu
        } else {
            RenderBudget::Gpu
        };
        let vision = self.stage_vision();
        let notice = self
            .chrome
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(6))
            .map(|(n, _)| n.clone());
        let Some(proj) = self.workspace.active_mut() else {
            center_text(
                ui,
                rect,
                "No project open. Add one from the guide (the menu button, then Add \
                 project), or start kgraf-app with --project <DIR>.",
                white,
            );
            return;
        };
        if let Some(problem) = &proj.problem {
            center_text(
                ui,
                rect,
                &format!("{} cannot be shown: {problem}", proj.name()),
                white,
            );
            return;
        }
        if proj.galaxy.is_none() && proj.galaxy_error.is_none() {
            match ProjectView::open(proj.root.clone(), budget, ui.ctx()) {
                Ok(mut view) => {
                    // One palette (the stage semantic colors and literals the chrome
                    // reads too), and no stage HUD: the project row and the selection
                    // title say the same. The view still says its warnings.
                    view.set_palette(theme::galaxy_palette(vision));
                    view.set_hud(false);
                    proj.galaxy = Some(view);
                }
                Err(e) => proj.galaxy_error = Some(format!("starting the feed: {e}")),
            }
        }
        if let Some(err) = &proj.galaxy_error {
            center_text(ui, rect, &format!("galaxy unavailable: {err}"), white);
            return;
        }
        let Some(view) = proj.galaxy.as_mut() else {
            return;
        };
        // The galaxy's warnings go under the selection title drawn below.
        view.set_overlay_top(warnings_top(rect));
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            view.show(ui, &rs);
        });
        chrome::mask_corners(ui.painter(), rect, 12.0, p.base);
        let mut repaint = false;
        // The selection title, top left.
        let sel = view
            .view()
            .and_then(|v| v.galaxy().selection().map(|s| s.selected()));
        if let (Some(slot), Some(model)) = (sel, view.model()) {
            let describe = model.describe(slot);
            let name = model
                .agent_at(slot)
                .map(str::to_owned)
                .or_else(|| model.plan_at(slot).map(|n| n.key.clone()))
                .or_else(|| model.qualified_name(slot).map(str::to_owned))
                .unwrap_or_else(|| format!("node {slot}"));
            let top = Rect::from_min_size(rect.min, vec2(rect.width(), 84.0));
            gradient(
                ui.painter(),
                top,
                Color32::from_black_alpha(140),
                Color32::TRANSPARENT,
            );
            let title = SelectionTitle::on(rect);
            let av = title.avatar;
            let letter = name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?');
            avatar(ui.painter(), av, letter, &Palette::dark(), None);
            let clip = ui.painter().with_clip_rect(title.text);
            clip.text(
                av.right_top() + vec2(12.0, 2.0),
                egui::Align2::LEFT_TOP,
                &name,
                theme::medium(18.0),
                white,
            );
            // STG-3: the metadata says how to clear.
            let meta = match describe {
                Some(d) => format!("{d}. Esc clears"),
                None => "Esc clears".to_owned(),
            };
            clip.text(
                av.right_top() + vec2(12.0, 26.0),
                egui::Align2::LEFT_TOP,
                meta,
                theme::regular(13.0),
                white.gamma_multiply(0.7),
            );
            let clear_at = title.clear.center();
            let clear = stage_button(
                ui,
                clear_at,
                icons::CLOSE,
                "Clear selection (Esc)",
                true,
                true,
            );
            if clear.clicked()
                && let Some(v) = view.view_mut()
            {
                v.galaxy_mut().select(None);
                repaint = true;
            }
        }
        if let Some(n) = notice {
            let at = pos2(rect.center().x, rect.top() + 20.0);
            let g = ui.painter().layout_no_wrap(n, theme::regular(13.0), white);
            let r = Rect::from_center_size(at, g.size() + vec2(24.0, 12.0));
            ui.painter().rect_filled(r, 8.0, theme::stage_popup());
            ui.painter().galley(r.min + vec2(12.0, 6.0), g, white);
        }
        // The legend, top right.
        self.legend(ui, rect);
        // The bottom bar and its gradient.
        let chrome_rect = Rect::from_min_max(
            pos2(rect.left() + 12.0, rect.bottom() - size::STAGE_CHROME_H),
            pos2(rect.right() - 12.0, rect.bottom()),
        );
        let fade = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 110.0), rect.max);
        gradient(
            ui.painter(),
            fade,
            Color32::TRANSPARENT,
            Color32::from_black_alpha(184),
        );
        repaint |= self.stage_controls(ui, chrome_rect);
        if self.chrome.minimap_open {
            repaint |= self.minimap(ui, rect);
        }
        if repaint {
            ui.ctx().request_repaint();
        }
    }

    /// The legend (STG-7): top right, 220 points wide, labels wrapping inside.
    fn legend(&mut self, ui: &mut Ui, stage: Rect) {
        let white = Color32::WHITE;
        let dim = white.gamma_multiply(0.85);
        let open = self.chrome.legend_open;
        let w = if open { 220.0 } else { 104.0 };
        let items: [(&str, u8); 6] = [
            ("Code symbol, size is connections", 0),
            ("Agent, gray dot is working", 1),
            ("Plan item, the bar is progress", 2),
            ("Waiting, reason under the label", 3),
            ("Needs your input", 4),
            ("Selected or matched", 5),
        ];
        // What the graph does not draw yet, said plainly. The galaxy
        // takes its colors from the same palette as this legend
        // (`theme::galaxy_palette`, decision 0068), so colors are not listed.
        const NOTE: &str = "Not drawn on the graph yet: the progress bar on plan cards, \
                            waiting cards at half opacity, and the head of the waits-on \
                            arrow.";
        let text_w = w - 12.0 - 26.0 - 10.0 - 12.0;
        let galleys: Vec<_> = if open {
            items
                .iter()
                .map(|(label, _)| {
                    ui.painter()
                        .layout((*label).to_owned(), theme::regular(12.0), dim, text_w)
                })
                .collect()
        } else {
            Vec::new()
        };
        let note = open.then(|| {
            ui.painter().layout(
                NOTE.to_owned(),
                theme::regular(11.0),
                white.gamma_multiply(0.6),
                w - 24.0,
            )
        });
        let rows_h: f32 = galleys.iter().map(|g| g.size().y.max(18.0) + 4.0).sum();
        let h = match &note {
            Some(n) => 32.0 + 4.0 + rows_h + 4.0 + n.size().y + 10.0,
            None => 32.0,
        };
        let rect = Rect::from_min_size(
            pos2(stage.right() - 12.0 - w, stage.top() + 12.0),
            vec2(w, h),
        );
        ui.painter().rect_filled(rect, 12.0, theme::stage_popup());
        let head = Rect::from_min_size(rect.min, vec2(w, 32.0));
        let resp = ui.interact(head, ui.id().with("legend_head"), Sense::click());
        ui.painter().text(
            head.left_center() + vec2(12.0, 0.0),
            egui::Align2::LEFT_CENTER,
            "Legend",
            theme::medium(13.0),
            white,
        );
        glyph(
            ui.painter(),
            head.right_center() - vec2(16.0, 0.0),
            if open {
                icons::ARROW_UP
            } else {
                icons::ARROW_DOWN
            },
            20.0,
            white,
            0.0,
        );
        if resp.on_hover_text("Show or hide the legend").clicked() {
            self.chrome.legend_open = !open;
        }
        let Some(note) = note else {
            return;
        };
        let dark = Palette::dark();
        let gray = crate::theme::stage_indicator(ui.ctx(), Indicator::Neutral);
        let mut y = head.bottom() + 4.0;
        for ((_, kind), g) in items.iter().zip(galleys) {
            let row_h = g.size().y.max(18.0);
            let icon = Rect::from_min_size(
                pos2(rect.left() + 12.0, y + (row_h - 18.0) * 0.5),
                vec2(26.0, 18.0),
            );
            let painter = ui.painter();
            match kind {
                0 => {
                    // Stage palette: a node at rest.
                    for (x, r) in [(4.0, 1.4), (11.0, 2.2), (20.0, 3.4)] {
                        painter.circle_filled(
                            pos2(icon.left() + x, icon.center().y),
                            r,
                            theme::stage::node(),
                        );
                    }
                }
                1 => {
                    // STG-DRAW-7: the actor disk, a 1.2 point border in white at
                    // 60%, a letter, and a gray working dot with a black ring.
                    let c = icon.center() - vec2(2.0, 0.0);
                    painter.circle(
                        c,
                        8.0,
                        theme::stage::ACTOR,
                        Stroke::new(1.2, white.gamma_multiply(0.6)),
                    );
                    painter.text(
                        c,
                        egui::Align2::CENTER_CENTER,
                        "K",
                        theme::medium(9.0),
                        white,
                    );
                    let dot = c + vec2(6.0, 6.0);
                    painter.circle_filled(dot, 3.5, theme::STAGE_BG);
                    painter.circle_filled(dot, 2.5, gray);
                }
                2 => {
                    let card = Rect::from_min_size(icon.min + vec2(0.0, 1.0), vec2(24.0, 14.0));
                    painter.rect(
                        card,
                        3.0,
                        theme::stage::CARD,
                        Stroke::new(1.0, white.gamma_multiply(0.34)),
                        egui::StrokeKind::Inside,
                    );
                    let bar = Rect::from_min_size(
                        pos2(card.left() + 0.5, card.bottom() - 3.0),
                        vec2(23.0, 2.5),
                    );
                    painter.rect_filled(bar, 0.0, white.gamma_multiply(0.22));
                    painter.rect_filled(
                        Rect::from_min_size(bar.min, vec2(13.0, 2.5)),
                        0.0,
                        theme::PROGRESS.from,
                    );
                }
                3 => {
                    let card = Rect::from_min_size(icon.min + vec2(0.0, 1.0), vec2(15.0, 14.0));
                    painter.rect(
                        card,
                        3.0,
                        dark.raised.gamma_multiply(0.5),
                        Stroke::new(1.0, white.gamma_multiply(0.17)),
                        egui::StrokeKind::Inside,
                    );
                    let yc = icon.center().y;
                    painter.line_segment(
                        [pos2(icon.left() + 16.0, yc), pos2(icon.left() + 22.0, yc)],
                        Stroke::new(1.0, theme::stage::RELATION),
                    );
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            pos2(icon.left() + 21.0, yc - 2.7),
                            pos2(icon.left() + 25.5, yc),
                            pos2(icon.left() + 21.0, yc + 2.7),
                        ],
                        theme::stage::RELATION,
                        Stroke::NONE,
                    ));
                }
                4 => {
                    let pill = Rect::from_center_size(icon.center(), vec2(18.0, 16.0));
                    painter.rect_filled(
                        pill,
                        8.0,
                        crate::theme::stage_indicator(ui.ctx(), Indicator::NeedsInput),
                    );
                    painter.text(
                        pill.center(),
                        egui::Align2::CENTER_CENTER,
                        "2",
                        theme::medium(10.0),
                        theme::stage::PILL_ICON,
                    );
                }
                _ => {
                    let r = Rect::from_center_size(icon.center(), vec2(20.0, 12.0));
                    painter.rect_stroke(
                        r,
                        3.0,
                        Stroke::new(
                            1.8,
                            crate::theme::stage_indicator(ui.ctx(), Indicator::Selection),
                        ),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            painter.galley(
                pos2(icon.right() + 10.0, y + (row_h - g.size().y) * 0.5),
                g,
                dim,
            );
            y += row_h + 4.0;
        }
        ui.painter()
            .galley(pos2(rect.left() + 12.0, y + 4.0), note, white);
    }

    /// The bottom of the stage (STG-5, style guide 1.1): the timeline scrubber,
    /// and under it one right-aligned group of view controls: the legend and
    /// minimap toggles, zoom out, zoom in, fit. There are no playback controls.
    ///
    /// The scrubber is a placeholder at the live edge: replay (a view of the
    /// graph at a past time) is an open question in decision 0063 and not built.
    /// The bar is filled to its right end in `--progress` (STG-TL-2); its knob
    /// shows on hover (STG-TL-3). A click or a drag anywhere, and End, keep the
    /// view live (STG-TL-7: the right end is live); dragging back is refused
    /// with a tooltip that says why. Returns whether the camera moved.
    fn stage_controls(&mut self, ui: &mut Ui, bar: Rect) -> bool {
        const NOT_BUILT: &str = "Run history. Going back in time is not built yet (decision \
                                 0063 leaves it open), so the view stays live.";
        let track = Rect::from_min_max(
            pos2(bar.left(), bar.top() + 7.0),
            pos2(bar.right(), bar.top() + 10.0),
        );
        let hit = track.expand2(vec2(0.0, 6.0));
        let resp = ui.interact(hit, ui.id().with("scrubber"), Sense::click_and_drag());
        let hot = resp.hovered() || resp.dragged() || resp.has_focus();
        // STG-TL-1: 3 px at rest, 5 px on hover, drag or focus.
        let track = if hot {
            track.expand2(vec2(0.0, 1.0))
        } else {
            track
        };
        let stop = track.left() + track.width() * theme::PROGRESS.stop;
        ui.painter().rect_filled(
            Rect::from_min_max(track.min, pos2(stop, track.bottom())),
            egui::CornerRadius {
                nw: 2,
                sw: 2,
                ne: 0,
                se: 0,
            },
            theme::PROGRESS.from,
        );
        gradient_h(
            ui.painter(),
            Rect::from_min_max(pos2(stop, track.top()), track.max),
            theme::PROGRESS.from,
            theme::PROGRESS.to,
        );
        if hot {
            ui.painter()
                .circle_filled(pos2(track.right(), track.center().y), 6.5, theme::ACCENT);
        }
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "Run history, live")
        });
        resp.on_hover_text(NOT_BUILT);

        // The view controls, right-aligned, 2 px apart (40 point buttons).
        let y = bar.bottom() - 24.0;
        let mut moved = false;
        let mut rx = bar.right() - 20.0;
        let mut camera = |f: &mut dyn FnMut(&mut kgraf_galaxy::Galaxy)| {
            if let Some(v) = self
                .workspace
                .active_mut()
                .and_then(|p| p.galaxy.as_mut())
                .and_then(ProjectView::view_mut)
            {
                f(v.galaxy_mut());
            }
        };
        // INT-5 keys: `+` `-` `0` and `m`, only when no field has the keyboard
        // and no modifier is held (INT-4).
        let free = ui.ctx().memory(|m| m.focused().is_none())
            && ui.input(|i| !i.modifiers.command && !i.modifiers.alt && !i.modifiers.ctrl);
        let typed_now: Vec<String> = if free {
            ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect()
            })
        } else {
            Vec::new()
        };
        let typed = |s: &str| typed_now.iter().any(|t| t == s);
        if stage_button(
            ui,
            pos2(rx, y),
            icons::FIT,
            "Fit graph to view (0)",
            true,
            false,
        )
        .clicked()
            || typed("0")
        {
            camera(&mut |g| g.frame_all());
            moved = true;
        }
        rx -= 42.0;
        // STG-CAM-2: buttons and keys zoom by 1.4 x.
        if stage_button(ui, pos2(rx, y), icons::ADD, "Zoom in (+)", true, false).clicked()
            || typed("+")
            || typed("=")
        {
            camera(&mut |g| g.camera_mut().zoom_factor(1.0 / 1.4));
            moved = true;
        }
        rx -= 42.0;
        if stage_button(ui, pos2(rx, y), icons::REMOVE, "Zoom out (-)", true, false).clicked()
            || typed("-")
        {
            camera(&mut |g| g.camera_mut().zoom_factor(1.4));
            moved = true;
        }
        rx -= 42.0;
        let mm = self.chrome.minimap_open;
        if stage_button(
            ui,
            pos2(rx, y),
            if mm { icons::MAP_FILL } else { icons::MAP },
            if mm {
                "Hide the minimap (m)"
            } else {
                "Show the minimap (m)"
            },
            true,
            false,
        )
        .clicked()
            || typed("m")
        {
            self.chrome.minimap_open = !mm;
        }
        rx -= 42.0;
        let lg = self.chrome.legend_open;
        if stage_button(
            ui,
            pos2(rx, y),
            icons::LEGEND,
            if lg {
                "Hide the legend"
            } else {
                "Show the legend"
            },
            true,
            false,
        )
        .clicked()
        {
            self.chrome.legend_open = !lg;
        }
        moved
    }

    /// The minimap: a density image of the scene's nodes (x, y: the camera looks
    /// down -Z) with the view's center marked. A click moves the view there.
    fn minimap(&mut self, ui: &mut Ui, stage: Rect) -> bool {
        let Some(proj) = self.workspace.active_mut() else {
            return false;
        };
        let root = proj.root.clone();
        let Some(pv) = proj.galaxy.as_mut() else {
            return false;
        };
        let generation = pv.generation();
        let agents: Vec<u32> = pv
            .model()
            .map(|m| m.agents().map(|(_, slot)| slot).collect())
            .unwrap_or_default();
        let Some(view) = pv.view_mut() else {
            return false;
        };
        let (w, h) = (170usize, 104usize);
        let stale = !matches!(&self.chrome.minimap, Some(m) if m.root == root && m.generation == generation);
        if stale {
            let scene = view.galaxy().scene();
            let nodes = scene.nodes();
            let live = (0..nodes.len() as u32).filter(|&n| scene.is_alive(n));
            let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            for n in live.clone() {
                let [x, y, _] = nodes[n as usize].position;
                b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
            }
            if b[0] > b[2] {
                return false;
            }
            // Keep the aspect: pad the shorter side.
            let (bw, bh) = ((b[2] - b[0]).max(1.0), (b[3] - b[1]).max(1.0));
            let scale = (bw / w as f32).max(bh / h as f32) * 1.08;
            let cx = (b[0] + b[2]) * 0.5;
            let cy = (b[1] + b[3]) * 0.5;
            let bounds = [
                cx - scale * w as f32 * 0.5,
                cy - scale * h as f32 * 0.5,
                cx + scale * w as f32 * 0.5,
                cy + scale * h as f32 * 0.5,
            ];
            let mut counts = vec![0u32; w * h];
            for n in live {
                let [x, y, _] = nodes[n as usize].position;
                let px = ((x - bounds[0]) / scale) as usize;
                let py = ((bounds[3] - y) / scale) as usize;
                if px < w && py < h {
                    counts[py * w + px] += 1;
                }
            }
            // STG-8: every node a 1 px dot in white at 50%, over the
            // `--stage-bg` fill drawn under the texture.
            let dot = Color32::from_white_alpha(128);
            let pixels: Vec<Color32> = counts
                .iter()
                .map(|&c| if c == 0 { Color32::TRANSPARENT } else { dot })
                .collect();
            let image = egui::ColorImage::new([w, h], pixels);
            let texture = ui
                .ctx()
                .load_texture("minimap", image, egui::TextureOptions::LINEAR);
            self.chrome.minimap = Some(Minimap {
                root: root.clone(),
                generation,
                texture,
                bounds,
            });
        }
        let Some(mm) = &self.chrome.minimap else {
            return false;
        };
        let rect = Rect::from_min_size(
            pos2(stage.right() - 12.0 - 172.0, stage.bottom() - 66.0 - 106.0),
            vec2(172.0, 106.0),
        );
        ui.painter().rect_filled(rect, 8.0, theme::STAGE_BG);
        let inner = rect.shrink(1.0);
        ui.painter().image(
            mm.texture.id(),
            inner,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        ui.painter().rect_stroke(
            rect,
            8.0,
            Stroke::new(1.0, Color32::from_white_alpha(77)),
            egui::StrokeKind::Inside,
        );
        let b = mm.bounds;
        let to_px = |x: f32, y: f32| {
            pos2(
                inner.left() + (x - b[0]) / (b[2] - b[0]) * inner.width(),
                inner.top() + (b[3] - y) / (b[3] - b[1]) * inner.height(),
            )
        };
        let cam = view.galaxy().camera();
        let target = cam.target();
        let at = to_px(target.x, target.y);
        let half =
            (cam.distance() * 0.45 / (b[2] - b[0]) * inner.width()).clamp(4.0, inner.width());
        // STG-8: actors as 4 px dots in #EDEFF7 (drawn live: few, and they move).
        {
            let scene = view.galaxy().scene();
            for &slot in &agents {
                if let Some(n) = scene.nodes().get(slot as usize) {
                    let [x, y, _] = n.position;
                    let at = to_px(x, y);
                    if inner.contains(at) {
                        ui.painter().circle_filled(at, 2.0, theme::stage::LIGHT);
                    }
                }
            }
        }
        let marker = Rect::from_center_size(at, vec2(half * 1.6, half)).intersect(inner);
        // The viewport: a white 1.5 px frame with a white 8% fill.
        ui.painter()
            .rect_filled(marker, 2.0, Color32::from_white_alpha(20));
        ui.painter().rect_stroke(
            marker,
            2.0,
            Stroke::new(1.5, Color32::WHITE),
            egui::StrokeKind::Inside,
        );
        let resp = ui
            .interact(rect, ui.id().with("minimap"), Sense::click_and_drag())
            .on_hover_text("Minimap. Click or drag to move the view.");
        if (resp.clicked() || resp.dragged())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            // The world point under the pointer, and a pan that moves the pivot
            // there (the camera pans in its view plane; looking down -Z that is
            // x and y).
            let wx = b[0] + (pos.x - inner.left()) / inner.width() * (b[2] - b[0]);
            let wy = b[3] - (pos.y - inner.top()) / inner.height() * (b[3] - b[1]);
            let vh = view.last_rect().map_or(600.0, |r| r.height()) * ui.ctx().pixels_per_point();
            let cam = view.galaxy_mut().camera_mut();
            let per_px = cam.pixel_world_size_per_depth(vh) * cam.distance();
            if per_px > 0.0 {
                let (dx, dy) = (wx - cam.target().x, wy - cam.target().y);
                cam.pan(-dx / per_px, dy / per_px, vh);
            }
            return true;
        }
        false
    }

    // ---------------------------------------------------------------- menus

    fn bell_menu(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.chrome.bell_open {
            return;
        }
        let screen = ctx.content_rect();
        let mut jump = None;
        let mut install = None;
        let update_row = update_notice(&self.update_state());
        let area = egui::Area::new(egui::Id::new("bell_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(screen.right() - 8.0 - 480.0, 48.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(p.menu)
                    .corner_radius(theme::radius::MENU)
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 32,
                        spread: 0,
                        color: p.shadow_menu,
                    })
                    .show(ui, |ui| {
                        ui.set_width(480.0);
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(16, 12))
                            .show(ui, |ui| {
                                ui.label(RichText::new("Notifications").font(theme::regular(16.0)).color(p.text));
                            });
                        separator(ui, p);
                        let mut any = false;
                        if let Some((words, candidate)) = &update_row {
                            any = true;
                            egui::Frame::new()
                                .inner_margin(egui::Margin::symmetric(16, 12))
                                .show(ui, |ui| {
                                    ui.set_width(448.0);
                                    ui.label(RichText::new(words).font(theme::regular(14.0)).color(p.text));
                                    if let Some(c) = candidate
                                        && chrome::text_button(ui, p.primary, "Download and install").clicked()
                                    {
                                        install = Some(c.clone());
                                    }
                                });
                        }
                        for (pi, proj) in self.workspace.projects().iter().enumerate() {
                            // Failures first: each jumps to its pinned card.
                            for f in proj.failures() {
                                any = true;
                                let (rect, resp) =
                                    ui.allocate_exact_size(vec2(480.0, 64.0), Sense::click());
                                if resp.hovered() {
                                    ui.painter().rect_filled(rect, 0.0, p.additive);
                                }
                                let av = Rect::from_center_size(
                                    rect.left_center() + vec2(36.0, 0.0),
                                    vec2(40.0, 40.0),
                                );
                                avatar(
                                    ui.painter(),
                                    av,
                                    '!',
                                    p,
                                    Some(p.indicator(Indicator::NeedsInput)),
                                );
                                let clip = ui.painter().with_clip_rect(rect.shrink(4.0));
                                clip.text(
                                    av.right_center() + vec2(16.0, -9.0),
                                    egui::Align2::LEFT_CENTER,
                                    format!("Failed: {}", f.words),
                                    theme::regular(14.0),
                                    p.text,
                                );
                                clip.text(
                                    av.right_center() + vec2(16.0, 11.0),
                                    egui::Align2::LEFT_CENTER,
                                    format!("{}. Pinned below the graph.", proj.name()),
                                    theme::regular(12.0),
                                    p.text_2,
                                );
                                let words = format!("{} failed: {}", proj.name(), f.words);
                                resp.widget_info(|| {
                                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &words)
                                });
                                if resp.on_hover_text("Go to the pinned card").clicked() {
                                    jump = Some((pi, f.key.clone()));
                                }
                            }
                            let Some(i) = proj.interaction.as_ref() else { continue };
                            for ask in i.host().pending_asks() {
                                any = true;
                                let (rect, resp) = ui.allocate_exact_size(vec2(480.0, 64.0), Sense::click());
                                if resp.hovered() {
                                    ui.painter().rect_filled(rect, 0.0, p.additive);
                                }
                                ui.painter().circle_filled(
                                    rect.left_center() + vec2(8.0, 0.0),
                                    2.0,
                                    p.indicator(Indicator::Selection),
                                );
                                let av = Rect::from_center_size(rect.left_center() + vec2(36.0, 0.0), vec2(40.0, 40.0));
                                let letter = ask.worker.chars().find(|c| c.is_alphanumeric()).unwrap_or('W');
                                avatar(ui.painter(), av, letter, p, Some(p.indicator(Indicator::NeedsInput)));
                                let clip = ui.painter().with_clip_rect(rect.shrink(4.0));
                                clip.text(
                                    av.right_center() + vec2(16.0, -9.0),
                                    egui::Align2::LEFT_CENTER,
                                    format!("{} wants to run {}: {}", ask.worker, ask.tool, ask.question),
                                    theme::regular(14.0),
                                    p.text,
                                );
                                clip.text(
                                    av.right_center() + vec2(16.0, 11.0),
                                    egui::Align2::LEFT_CENTER,
                                    format!("{}. Approval pinned below the graph.", proj.name()),
                                    theme::regular(12.0),
                                    p.text_2,
                                );
                                let words = format!("{} wants to run {} in {}", ask.worker, ask.tool, proj.name());
                                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &words));
                                if resp.on_hover_text("Go to the pinned card").clicked() {
                                    jump = Some((pi, format!("ask:{}", ask.id)));
                                }
                            }
                        }
                        if !any {
                            egui::Frame::new()
                                .inner_margin(egui::Margin::symmetric(32, 32))
                                .show(ui, |ui| {
                                    ui.vertical_centered(|ui| {
                                        ui.label(icons::text(icons::BELL, 48.0).color(p.text_2));
                                        ui.label(RichText::new("Nothing needs you").font(theme::medium(16.0)).color(p.text));
                                        ui.label(
                                            RichText::new(
                                                "Approvals, questions and failures appear here, and pinned below the graph.",
                                            )
                                            .color(p.text_2),
                                        );
                                    });
                                });
                        }
                    });
            });
        if let Some(candidate) = install
            && let Some(u) = &self.update
        {
            u.accept_and_install(candidate);
        }
        if let Some((pi, ask)) = jump {
            self.workspace.switch_to(pi);
            let root = self.workspace.projects().get(pi).map(|p| p.root.clone());
            self.chrome.flash = root.map(|r| (r, ask, Instant::now()));
            self.chrome.bell_open = false;
        } else if ctx.input(|i| i.pointer.any_click())
            && !ctx
                .pointer_interact_pos()
                .is_some_and(|pos| area.response.rect.contains(pos) || pos.y < size::MASTHEAD_H)
        {
            self.chrome.bell_open = false;
        }
    }

    /// The settings dialog (CMP-DLG-1 to -6): a title and one line, the tab
    /// list on the left and the selected tab's rows on the right, Done at the
    /// bottom. Each tab is drawn from [`crate::settings_rows`], so every
    /// section, row and switch reads the same way.
    fn settings_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(tab) = self.chrome.settings_tab else {
            return;
        };
        let mut next_tab = tab;
        let modal = egui::Modal::new(egui::Id::new("settings"))
            // CMP-DLG-1: the scrim, and the menu fill with its shadow.
            .backdrop_color(p.scrim)
            .frame(
                egui::Frame::new()
                    .fill(p.menu)
                    .corner_radius(theme::radius::MENU)
                    .inner_margin(24)
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 32,
                        spread: 0,
                        color: p.shadow_menu,
                    }),
            )
            .show(ctx, |ui| {
                ui.set_width(760.0); // CMP-DLG-1
                ui.set_min_height(420.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        // CMP-DLG-2: the title in 700 20/28, the description in
                        // 400 14/20 `--text-2`.
                        ui.label(
                            RichText::new("Settings")
                                .font(theme::bold(20.0))
                                .color(p.text),
                        );
                        ui.label(
                            RichText::new("Configuration only. Work state stays on the graph.")
                                .size(14.0)
                                .color(p.text_2),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if chrome::icon_button(ui, p, icons::CLOSE, "Close (Esc)").clicked() {
                            next_tab = SettingsTab::General;
                            self.chrome.settings_tab = None;
                        }
                    });
                });
                separator(ui, p);
                ui.horizontal_top(|ui| {
                    // CMP-DLG-3: a 200 px tab list.
                    ui.allocate_ui(vec2(200.0, 440.0), |ui| {
                        ui.vertical(|ui| {
                            for t in SettingsTab::ALL {
                                if chrome::guide_entry(ui, p, t.icon(), t.label(), tab == t)
                                    .clicked()
                                {
                                    next_tab = t;
                                }
                            }
                        });
                    });
                    ui.add_space(16.0);
                    ui.vertical(|ui| {
                        ui.set_width(480.0);
                        // One height for every tab, so the dialog does not
                        // jump when the tab changes (CMP-DLG-1 fixes its size).
                        ui.set_min_height(440.0);
                        egui::ScrollArea::vertical()
                            .id_salt("settings_body")
                            .max_height(440.0)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width() - 12.0);
                                // CMP-SET-2: a slider wide enough to aim with,
                                // leaving room for its value.
                                ui.spacing_mut().slider_width = 300.0;
                                match tab {
                                    SettingsTab::General => self.settings_general(ui, p),
                                    SettingsTab::Runs => self.settings_runs(ui, p),
                                    SettingsTab::Permissions => self.settings_permissions(ui, p),
                                    SettingsTab::Appearance => self.settings_appearance(ui, p),
                                    SettingsTab::Providers => self.settings_providers(ui, p),
                                }
                                if let Some(err) = &self.store.last_error {
                                    ui.add_space(8.0);
                                    settings_rows::error(
                                        ui,
                                        p,
                                        &format!("Settings not saved: {err}"),
                                    );
                                }
                            });
                    });
                });
                separator(ui, p);
                // CMP-DLG-4: settings apply at once, so one Done and no Save.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if chrome::text_button(ui, p.primary, "Done").clicked() {
                        self.chrome.settings_tab = None;
                    }
                });
            });
        if modal.should_close() {
            self.chrome.settings_tab = None;
        } else if self.chrome.settings_tab.is_some() {
            self.chrome.settings_tab = Some(next_tab);
        }
    }

    /// General: what the app does when its window closes, and updates.
    fn settings_general(&mut self, ui: &mut Ui, p: &Palette) {
        settings_rows::heading(ui, p, "Window");
        let toggled = settings_rows::toggle(
            ui,
            p,
            "Background mode",
            "On: closing the window leaves kGraf in the tray, and scheduled runs keep going. \
             Off: closing the window quits.",
            &mut self.settings.background_mode,
        );
        if toggled {
            // Decision 0052's global switch, applied to every open project's
            // scheduler at once, not only at the next launch.
            for proj in self.workspace.projects() {
                if let Some(s) = &proj.scheduler {
                    s.set_background_mode(self.settings.background_mode);
                }
            }
        }
        if self.tray.is_none() {
            // TXT-6: a degraded state says why.
            settings_rows::indented(ui, |ui| {
                settings_rows::helper(ui, p, "No tray icon in this session, so closing quits.")
            });
        }
        settings_rows::divider(ui, p);
        self.settings_planning(ui, p);
        settings_rows::divider(ui, p);
        self.settings_updates(ui, p);
    }

    /// Planning (decision 0100): whether a small request skips the planning
    /// call, and whether its worker may start before the triage answer.
    /// Configuration only (decision 0017); a run started after the change
    /// takes it. A run file's `plan` and `early_start` outrank it.
    fn settings_planning(&mut self, ui: &mut Ui, p: &Palette) {
        settings_rows::heading(ui, p, "Planning");
        settings_rows::toggle(
            ui,
            p,
            "Run small requests without a plan",
            "Off by default. On: a request of one sentence that names one or two files runs \
             as one contract with no planning call; a request kGraf cannot size gets one \
             short triage call first. Off: every request is planned.",
            &mut self.settings.plan_auto,
        );
        ui.add_space(8.0);
        settings_rows::toggle(
            ui,
            p,
            "Start a worker before triage answers",
            "On by default. On: while the triage call runs, the one worker starts if it can \
             start at once; its work merges only after the answer, and a \"split\" answer \
             stops it. Only applies when small requests run without a plan.",
            &mut self.settings.early_start,
        );
        ui.add_space(8.0);
        settings_rows::toggle(
            ui,
            p,
            "Let a contract carry its own check",
            "Off by default. On: the planner may give a contract a check command of its own; \
             kGraf runs it only when the run's check tests nothing for that contract, and a \
             pass verifies it. A check that tests nothing, or that your permissions would not \
             run, is refused.",
            &mut self.settings.own_checks,
        );
        ui.add_space(8.0);
        settings_rows::toggle(
            ui,
            p,
            "Hold planned work that no test checks",
            "Off by default. On: a planned contract whose check, narrowed to its files, selects \
             no test stays unverified until the whole check passes on the merged result, as a \
             small request's does.",
            &mut self.settings.zero_test_plans,
        );
        ui.add_space(8.0);
        settings_rows::toggle(
            ui,
            p,
            "Lay out change requests for the prompt cache",
            "Off by default. On: a change to a finished run sends its plan in id order, and \
             marks it for the provider's prompt cache where the provider takes marks and a \
             read can still come. The run report shows each call kind's cache read share and \
             the write cost paid.",
            &mut self.settings.cache_layout,
        );
    }

    /// Runs: everything a run the app starts takes from Settings. A change
    /// applies to runs started after it (a host reopens when no run is
    /// active). A cap above what a model of an open project supports is
    /// lowered for that run, and the note under the slider says so before
    /// the run starts (the same sentence a run reports).
    fn settings_runs(&mut self, ui: &mut Ui, p: &Palette) {
        // CMP-SET-6: a fixed behavior, stated once for the whole tab.
        settings_rows::note(
            ui,
            p,
            "These apply to runs started after the change. The run limits also apply to \
             kgraf commands given no flag, and a flag on the command line outranks them.",
        );
        ui.add_space(12.0);
        let hosting = self
            .workspace
            .projects()
            .iter()
            .any(|p| p.interaction.as_ref().is_some_and(|i| i.hosting()));
        let attempts = self
            .workspace
            .projects()
            .iter()
            .filter_map(|p| p.interaction.as_ref().and_then(|i| i.max_attempts()))
            .max();
        // Decision 0067: the parallelism slider and its capacity breakdown,
        // the settings surface's only concurrency control. `breakdown` is
        // `None` only until the re-measure thread's first tick, or when no
        // thread was started; then no breakdown is shown, rather than a
        // number invented here.
        let breakdown = self.capacity_breakdown();
        crate::interaction::parallelism_controls(
            ui,
            p,
            &mut self.settings.parallelism,
            breakdown.as_ref(),
            hosting,
            attempts,
        );
        settings_rows::divider(ui, p);
        self.settings_compaction(ui, p);
        settings_rows::divider(ui, p);
        self.settings_run_limits(ui, p);
        settings_rows::divider(ui, p);
        self.settings_worktrees(ui, p);
    }

    /// The compaction point (decision 0057 addendum): a slider that follows
    /// the recommendation until moved (CMP-SET-3). Every run the app starts
    /// takes it (`Settings::run_limits`); a run file's `point` outranks it.
    fn settings_compaction(&mut self, ui: &mut Ui, p: &Palette) {
        settings_rows::label(ui, p, "Compaction point");
        settings_rows::description(
            ui,
            p,
            "The share of the effective window at which a worker's context is compacted. \
             A point set in the project's run.json outranks it.",
        );
        let slider = &mut self.settings.compaction;
        let mut percent = slider.value().percent();
        let changed = ui
            .add(
                egui::Slider::new(
                    &mut percent,
                    COMPACTION_MIN_PERCENT..=COMPACTION_MAX_PERCENT,
                )
                .suffix("%"),
            )
            .changed();
        if changed {
            // The slider's range equals CompactionPoint's range, so this only fails
            // if the two drift apart; then the move is refused visibly.
            match CompactionPoint::new(i64::from(percent)) {
                Ok(pt) => slider.set_by_user(pt),
                Err(e) => settings_rows::error(ui, p, &e.to_string()),
            }
        }
        let rec_text = match slider.recommendation() {
            Recommendation::Unmeasured(pt) => format!("Recommended: {}%, unmeasured", pt.percent()),
            Recommendation::Measured(pt) => format!("Recommended: {}% (measured)", pt.percent()),
        };
        settings_rows::helper(ui, p, &rec_text);
        if !slider.follows_recommendation()
            && chrome::text_button(ui, p.primary, "Return to recommendation").clicked()
        {
            slider.return_to_recommendation();
        }
    }

    /// The six global run limits as sliders, one row each (CMP-SET-1,
    /// CMP-SET-2; the default legend and its return action, CMP-SET-3).
    fn settings_run_limits(&mut self, ui: &mut Ui, p: &Palette) {
        let worker = self.settings.limits.worker_max_tokens.value() as u32;
        let planner = self.settings.limits.planner_max_tokens.value() as u32;
        let mut worker_notes = Vec::new();
        let mut planner_notes = Vec::new();
        for proj in self.workspace.projects() {
            let Some(host) = proj
                .interaction
                .as_ref()
                .and_then(|i| i.host().housekeeping())
                .and_then(|h| h.host())
            else {
                continue;
            };
            for (which, sentence) in host.output_clamp_notes(worker, planner) {
                let notes = if which == "worker" {
                    &mut worker_notes
                } else {
                    &mut planner_notes
                };
                let line = format!("{}: {sentence}", proj.name());
                if !notes.contains(&line) {
                    notes.push(line);
                }
            }
        }
        let limits = &mut self.settings.limits;
        crate::interaction::limit_slider_row(
            ui,
            p,
            "Model rounds per worker",
            "The rounds in one block. A worker that makes measured progress is given another \
             block of this size (see the next limit); one that does not ends at the block's \
             end with a final answer.",
            &mut limits.rounds,
            false,
            &[],
        );
        settings_rows::divider(ui, p);
        crate::interaction::limit_slider_row(
            ui,
            p,
            "Most round blocks per worker",
            "The most blocks of rounds one worker may be given while it keeps making \
             progress. 1 means the old fixed round limit: no extension.",
            &mut limits.max_checkpoints,
            false,
            &[],
        );
        settings_rows::divider(ui, p);
        crate::interaction::limit_slider_row(
            ui,
            p,
            "Worker reply cap (tokens)",
            "The most output tokens one worker reply may have.",
            &mut limits.worker_max_tokens,
            true,
            &worker_notes,
        );
        settings_rows::divider(ui, p);
        crate::interaction::limit_slider_row(
            ui,
            p,
            "Orchestrator reply cap (tokens)",
            "The most output tokens one orchestrator call may have: planning, re-grounding \
             and reviews. An answer cut off at it fails the run, naming this limit.",
            &mut limits.planner_max_tokens,
            true,
            &planner_notes,
        );
        settings_rows::divider(ui, p);
        crate::interaction::limit_slider_row(
            ui,
            p,
            "Context window assumed (tokens)",
            "The window assumed for models. A single worker uses at most 100,000 tokens of it.",
            &mut limits.window,
            true,
            &[],
        );
        settings_rows::divider(ui, p);
        crate::interaction::limit_slider_row(
            ui,
            p,
            "Answer wait (seconds)",
            "How long a worker's question waits for the orchestrator's model to answer it. \
             When the wait ends, the worker states an assumption and continues; an answer \
             that comes later reaches it at its next round. Also how long a connection \
             judgment may run.",
            &mut limits.answer_wait,
            false,
            &[],
        );
    }

    /// The worker-worktree switch (decision 0088): the effect and the fixed
    /// exception in plain words (TXT-5, TXT-7: no saving is claimed, only
    /// what is deleted). A change reaches every open project's host at once
    /// and applies to the runs built after it.
    fn settings_worktrees(&mut self, ui: &mut Ui, p: &Palette) {
        let toggled = settings_rows::toggle(
            ui,
            p,
            "Delete worker worktrees after use",
            "Each worker builds in its own worktree inside the project, with its own build \
             folder (a Rust target folder can be several gigabytes). On: a worktree and its \
             builds are deleted once nothing needs them. Off: every worktree stays, with its \
             builds, until you remove it. A worktree that a live worker, a stopped run, a \
             pending merge or a later part of the same work needs is never deleted.",
            &mut self.settings.delete_worker_worktrees,
        );
        if toggled {
            let retention = self.settings.worktree_retention();
            for proj in self.workspace.projects() {
                if let Some(i) = &proj.interaction
                    && let Some(h) = i.host().housekeeping()
                {
                    h.set_worktree_retention(retention);
                }
            }
        }
    }

    /// Permissions (decision 0094): what reading permissions.toml found, the
    /// Allow Bypass switch, the rule editor, the script-construct switches and
    /// the rule-conflict warning. Configuration only (CMP-DLG-6): the mode
    /// itself is chosen in the orchestrator box, the one place it is set;
    /// Bypass is offered there only with Allow Bypass on, and its first use
    /// shows a warning. Turning Allow Bypass off while in Bypass returns the
    /// mode to Manual at once.
    fn settings_permissions(&mut self, ui: &mut Ui, p: &Palette) {
        settings_rows::description(
            ui,
            p,
            "The permission mode is chosen in the orchestrator box. Allow Bypass, the rules \
             and the shell-script switches are kept in the user-level permissions.toml, \
             outside every project.",
        );
        let Some(perm) = self.permissions.clone() else {
            settings_rows::helper(ui, p, "No permission settings were loaded in this window.");
            settings_rows::divider(ui, p);
            self.settings_rule_conflicts(ui, p);
            settings_rows::divider(ui, p);
            self.settings_install_registries(ui, p);
            return;
        };
        crate::permission_bar::draw_load_result(ui, &perm, p);
        settings_rows::divider(ui, p);
        let settings = perm.control.settings();
        let about = "Off: the orchestrator box does not offer Bypass. Bypass grants every \
                     request without asking, except your deny and ask rules and deleting \
                     system paths, and gives no protection against prompt injection.";
        if settings.disable_bypass {
            // TXT-6: a disabled control says why and what to do.
            let mut off = false;
            ui.add_enabled(
                false,
                egui::Checkbox::new(
                    &mut off,
                    RichText::new("Allow Bypass mode")
                        .font(theme::medium(14.0))
                        .color(p.text),
                ),
            );
            settings_rows::indented(ui, |ui| {
                settings_rows::description(
                    ui,
                    p,
                    "Bypass is disabled in permissions.toml (disable_bypass = true). Remove \
                     that line to offer it.",
                );
            });
        } else {
            let mut allow = settings.allow_bypass;
            if settings_rows::toggle(ui, p, "Allow Bypass mode", about, &mut allow) {
                perm.control.set_allow_bypass(allow);
                // Through the UI's save, so refused entries stay in the file.
                if perm.file.is_some()
                    && let Err(e) = perm.save()
                {
                    self.chrome.notice = Some((
                        format!("The permission setting was not saved: {e}"),
                        Instant::now(),
                    ));
                }
            }
        }
        crate::permission_bar::draw_rule_editor(ui, &perm, p);
        settings_rows::divider(ui, p);
        for line in crate::permission_bar::draw_script_constructs(ui, &perm, p) {
            if let crate::orchestrator_box::Line::Error(e) = line {
                self.chrome.notice = Some((e, Instant::now()));
            }
        }
        settings_rows::divider(ui, p);
        self.settings_rule_conflicts(ui, p);
        settings_rows::divider(ui, p);
        self.settings_install_registries(ui, p);
    }

    /// The hosts each package manager's install may reach when kGraf runs it
    /// in its sandbox (decision 0087, amended 2026-10-07): one host per line,
    /// `*.domain` for every name under a domain, `host:port` for another
    /// port than 443 and 80. A list that does not parse is shown with the
    /// reason and not applied (CMP-FLD-4); "Default" puts the built-in list
    /// back. Kept in settings.toml, read when an install is about to run.
    fn settings_install_registries(&mut self, ui: &mut Ui, p: &Palette) {
        use kgraf_host::deps::{Ecosystem, InstallHosts};
        settings_rows::heading(ui, p, "Install registries");
        settings_rows::description(
            ui,
            p,
            "When you allow it, kGraf runs a project's missing install (npm, pip, Go, Maven, \
             Gradle, .NET, Composer, Bundler) in its sandbox. On macOS and Linux the install \
             reaches only these hosts, one per line, and cannot read your registry logins, so \
             a private registry that needs one stays unreachable. On Windows the sandbox \
             confines neither the network nor reads: the install runs with the network open, \
             and its scripts can read any file you can.",
        );
        for eco in Ecosystem::ALL {
            let current = || {
                self.settings
                    .install_registries
                    .for_ecosystem(eco)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let initial = current();
            let (text, error) = self
                .chrome
                .install_registry_text
                .entry(eco)
                .or_insert_with(|| (initial, None));
            ui.add_space(6.0);
            settings_rows::label(ui, p, eco.name());
            let edited = ui
                .add(
                    egui::TextEdit::multiline(text)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                )
                .changed();
            let mut reset = false;
            ui.horizontal(|ui| {
                reset = chrome::text_button(ui, p.primary, "Default").clicked();
            });
            if reset {
                *text = InstallHosts::default_hosts(eco).join("\n");
            }
            if edited || reset {
                let entries: Vec<String> = text.lines().map(str::to_owned).collect();
                match InstallHosts::parse_list(&entries) {
                    Ok(rules) => {
                        *error = None;
                        self.settings.install_registries.set(eco, rules);
                    }
                    Err(e) => *error = Some(e),
                }
            }
            if let Some(e) = error {
                settings_rows::error(ui, p, e);
            }
        }
    }

    /// The rule-conflict warning: a run setting kept in settings.toml with the
    /// run limits, shown with the rules it is about.
    fn settings_rule_conflicts(&mut self, ui: &mut Ui, p: &Palette) {
        settings_rows::toggle(
            ui,
            p,
            "Warn about rule conflicts",
            "Off by default. On: at the start of a run, kGraf says when your verify or gate \
             command runs a program your deny or ask rules block for workers. The command \
             still runs.",
            &mut self.settings.limits.warn_rule_conflicts,
        );
    }

    /// The update-check controls. Off by default and said so plainly (decision
    /// 0051's egress rule: a GitHub Releases query is not a configured model
    /// provider or MCP server, so it is opt-in, not a standing destination);
    /// "Check now" runs regardless of the switch, since the user just asked.
    fn settings_updates(&mut self, ui: &mut Ui, p: &Palette) {
        settings_rows::heading(ui, p, "Updates");
        settings_rows::toggle(
            ui,
            p,
            "Check for updates on start",
            "Off by default. On: kGraf reads the GitHub Releases of Marceux-Croix/kGraf when \
             it starts. This is the only network request the setting causes.",
            &mut self.settings.check_for_updates,
        );
        ui.add_space(8.0);
        settings_rows::toggle(
            ui,
            p,
            "Include prereleases",
            "Offer a prerelease too, not only a full release: at start and for Check now.",
            &mut self.settings.allow_prerelease_updates,
        );
        ui.add_space(8.0);
        if chrome::tonal_button(ui, p, "Check now")
            .on_hover_text("Check GitHub Releases now, whether or not the setting is on.")
            .clicked()
            && let Some(u) = &self.update
        {
            u.check_now(self.settings.allow_prerelease_updates);
        }
        match self.update_state() {
            None | Some(crate::update_bg::UpdateState::Idle) => {}
            Some(UpdateState::Checking) => settings_rows::helper(ui, p, "Checking..."),
            Some(UpdateState::UpToDate) => settings_rows::helper(ui, p, "kGraf is up to date."),
            Some(UpdateState::Available(candidate)) => {
                ui.colored_label(
                    p.primary,
                    format!("kGraf {} is available.", candidate.version),
                );
                if chrome::text_button(ui, p.primary, "Download and install").clicked()
                    && let Some(u) = &self.update
                {
                    u.accept_and_install(candidate);
                }
            }
            Some(UpdateState::Downloading) => settings_rows::helper(ui, p, "Downloading..."),
            Some(UpdateState::Ready { words, .. }) => {
                ui.colored_label(p.text, words);
            }
            Some(UpdateState::Failed(message)) => {
                settings_rows::error(ui, p, &format!("Update check failed: {message}"));
            }
        }
    }

    /// Appearance: the theme and color vision as single-choice lists
    /// (CMP-SET-5), reduced motion, and the graphics adapter.
    fn settings_appearance(&mut self, ui: &mut Ui, p: &Palette) {
        settings_rows::heading(ui, p, "Theme");
        for (a, icon, label) in [
            (Appearance::Device, icons::AUTO_THEME, "Use device theme"),
            (Appearance::Dark, icons::DARK_MODE, "Dark theme"),
            (Appearance::Light, icons::LIGHT_MODE, "Light theme"),
        ] {
            if chrome::guide_entry(ui, p, icon, label, self.settings.appearance == a).clicked() {
                self.settings.appearance = a;
            }
        }
        settings_rows::description(
            ui,
            p,
            "The graph stays on its near-black stage in every theme.",
        );
        settings_rows::divider(ui, p);
        settings_rows::heading(ui, p, "Color vision");
        for v in kgraf_core::palette::Vision::ALL {
            if chrome::guide_entry(
                ui,
                p,
                icons::VISIBILITY,
                v.label(),
                self.settings.color_vision == v,
            )
            .clicked()
            {
                self.settings.color_vision = v;
            }
        }
        settings_rows::description(
            ui,
            p,
            "Remaps the five meaning colors (progress, danger, needs input, selection, \
             neutral) for the chosen kind of color blindness. This is kGraf's own setting, \
             not part of the style guide's one palette.",
        );
        settings_rows::divider(ui, p);
        settings_rows::toggle(
            ui,
            p,
            "Reduce motion",
            "No blinking or wiggling indicators.",
            &mut self.settings.reduce_motion,
        );
        settings_rows::divider(ui, p);
        settings_rows::heading(ui, p, "Graphics");
        settings_rows::toggle(
            ui,
            p,
            "Prefer high-performance GPU",
            "Discrete GPU first. Takes effect at the next launch.",
            &mut self.settings.prefer_high_performance_gpu,
        );
        ui.add_space(8.0);
        settings_rows::label(ui, p, "Current adapter");
        // TXT-6 and COL-7: CPU rendering is a degraded state, in `--danger`.
        let color = if self.render.path.is_software() {
            p.danger
        } else {
            p.text_2
        };
        ui.label(RichText::new(self.render.label()).size(13.0).color(color));
        settings_rows::description(
            ui,
            p,
            "On a CPU adapter the galaxy draws without glow, with edges only for a \
             selection, and the top bar says so. Start with --force-software to try it.",
        );
    }

    /// Providers: each route of the active project, read-only (CMP-SET-7).
    fn settings_providers(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(proj) = self.workspace.active() else {
            settings_rows::description(ui, p, "Open a project to see its provider routes.");
            return;
        };
        let root = proj.root.clone();
        let rj = self
            .chrome
            .run_json
            .get(&run_json_path(&root), || read_run_json(&root));
        settings_rows::heading(
            ui,
            p,
            &format!("Routes of {} (.kgraf/run.json)", proj.name()),
        );
        if let Some(e) = &rj.error {
            settings_rows::error(ui, p, e);
        }
        // (route, role and kind, generative): the reasoning line applies to a
        // generative route only.
        let mut all: Vec<(String, String, bool)> = rj
            .routes
            .iter()
            .map(|r| (r.clone(), role_line("worker", r), true))
            .collect();
        if let Some(pl) = &rj.planner {
            all.insert(0, (pl.clone(), role_line("orchestrator", pl), true));
        }
        // Decision 0089: routes of other kinds, shown with their kind and the
        // tool that reaches them.
        all.extend(rj.model_routes.iter().map(|spec| {
            let (route, line) = model_route_row(spec);
            (route, line, false)
        }));
        for (route, role, generative) in all {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                let letter = route.chars().find(|c| c.is_alphanumeric()).unwrap_or('?');
                avatar(
                    ui.painter(),
                    rect,
                    letter,
                    p,
                    Some(p.indicator(Indicator::Neutral)),
                );
                ui.vertical(|ui| {
                    settings_rows::label(ui, p, &route);
                    settings_rows::helper(ui, p, &role);
                    if generative {
                        settings_rows::helper(ui, p, &reasoning_line(&route));
                    }
                });
            })
            .response
            .on_hover_text(
                "Gray: reachability is not checked from this dialog. `kgraf providers` checks it.",
            );
        }
        settings_rows::divider(ui, p);
        settings_rows::description(
            ui,
            p,
            "Every token is attributed to a provider call. kGraf reports spend; it does not \
             claim savings.",
        );
    }
}

// ---------------------------------------------------------------- helpers

/// The role line under a generative route: its role and its kind (decision
/// 0089). A worker or planner route that names another kind is flagged, since
/// a run refuses it.
fn role_line(role: &str, route: &str) -> String {
    match kgraf_core::split_kind_prefix(route) {
        Ok((_, rest)) if rest.len() == route.len() => {
            format!("{role}, {}", kgraf_core::ModelKind::GENERATIVE)
        }
        Ok((kind, _)) => format!(
            "{role}, {kind}: refused; a {role} route takes no kind prefix (declare other kinds \
             in model_routes)"
        ),
        Err(e) => format!("{role}, {e}"),
    }
}

/// One `model_routes` row: the spec as written, and its kind with the tool
/// that reaches it, or why the spec is not usable.
fn model_route_row(spec: &str) -> (String, String) {
    let line = match kgraf_core::split_kind_prefix(spec) {
        Ok((kind, _)) if kind.is_generative() => {
            "no kind: write kind=route:model; generative routes go in routes".to_owned()
        }
        Ok((kind, _)) => match kind.spec() {
            Ok(spec) => format!("{kind}, reached through the {} tool", spec.tools.join(", ")),
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    };
    (spec.to_owned(), line)
}

/// The reasoning-visibility line under one route (Settings > Providers,
/// design 0067 point 9). `route` is `id` or `id:model`, as
/// `run.json` writes it; a route this shell has not built a provider for
/// (an unknown id) says so rather than guessing.
///
/// **Declared only, for now.** The catalog's declared value and its
/// `Confidence` are shown. The *observed* half of the design's
/// declared-vs-observed pair needs a per-route ledger query (reasoning tokens,
/// reasoning text length, rounds) that the shell does not run yet
/// (design 0067; `kgraf-provider/src/accounting.rs` owns the ledger
/// statistics `T_r` and `tok_r`; reasoning-specific per-route stats are not
/// among them). Feeding `session_panel::observed_visibility` real numbers
/// once that query exists is a small follow-up; showing a fabricated
/// "observed" value here instead would be exactly the silent-degradation
/// failure the fail-loud rule exists to stop, so this states plainly that it is not
/// wired rather than showing a number nobody measured.
fn reasoning_line(route: &str) -> String {
    let id = route.split_once(':').map_or(route, |(id, _)| id);
    match kgraf_fleet::entry(id) {
        Ok(e) => format!(
            "Reasoning: {} ({}; declared only -- no live observation is wired to this \
             dialog yet)",
            session_panel::label(e.reasoning_visibility),
            e.confidence.as_str(),
        ),
        Err(_) => "Reasoning: unknown route (not in the catalog)".to_owned(),
    }
}

/// A pinned failure card (CMP-CARD-1, -2, -5): the additive fill, a 24 point
/// avatar with the needs-input dot, the failure in words, with the failed thing
/// itself in the danger color (COL-7), and Dismiss where the failure can be
/// acknowledged. Dismiss acts at once: it hides a notice, it changes no work, so
/// CMP-CARD-3's confirm step does not apply. Returns whether Dismiss was
/// pressed.
fn failure_card(ui: &mut Ui, p: &Palette, f: &crate::projects::Failure, flash: bool) -> bool {
    let (fill, stroke) = if flash {
        (
            p.suggested,
            Stroke::new(2.0, p.indicator(Indicator::Selection)),
        )
    } else {
        (p.additive, Stroke::NONE)
    };
    let mut dismissed = false;
    let resp = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(theme::radius::CARD)
        .inner_margin(egui::Margin {
            left: 14,
            right: 14,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                avatar(
                    ui.painter(),
                    rect,
                    '!',
                    p,
                    Some(p.indicator(Indicator::NeedsInput)),
                );
                ui.label(
                    RichText::new("Failed")
                        .font(theme::medium(13.0))
                        .color(p.danger),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Pinned by kGraf").small().color(p.text_2));
                    ui.label(icons::text(icons::PIN, 14.0).color(p.text_2));
                });
            });
            ui.add(egui::Label::new(RichText::new(&f.words).color(p.text)).wrap());
            if f.dismissable {
                ui.add_space(8.0);
                if chrome::tonal_button(ui, p, "Dismiss").clicked() {
                    dismissed = true;
                }
            } else {
                ui.label(
                    RichText::new("Clears when it is fixed.")
                        .small()
                        .color(p.text_2),
                );
            }
        })
        .response;
    if flash {
        ui.scroll_to_rect(resp.rect, Some(egui::Align::Center));
    }
    dismissed
}

/// A card: a panel (CMP-PNL-1: 1 px outline, radius 12, base fill).
fn card(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui)) {
    let rect = ui.available_rect_before_wrap();
    ui.painter().rect(
        rect,
        theme::radius::CARD,
        p.base,
        Stroke::new(1.0, p.outline),
        egui::StrokeKind::Inside,
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(1.0)), |ui| {
        ui.set_clip_rect(rect.shrink(1.0));
        add(ui);
    });
    ui.advance_cursor_after_rect(rect);
}

/// A card's 48 point head: a 16/22 title and, for the live card, the
/// "Live" state with the accent dot (CMP-ROW-5).
fn card_head(ui: &mut Ui, p: &Palette, title: &str, state: Option<(&str, bool)>) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), size::CARD_HEAD_H),
        Sense::hover(),
    );
    ui.painter().text(
        rect.left_center() + vec2(16.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        theme::regular(16.0),
        p.text,
    );
    if let Some((s, live)) = state {
        let g = ui
            .painter()
            .layout_no_wrap(s.to_owned(), theme::medium(12.0), p.text_2);
        let x = rect.right() - 16.0 - g.size().x;
        // CMP-ROW-5: the accent dot only while the tail is live; gray otherwise.
        let dot = if live { theme::ACCENT } else { p.text_dis };
        ui.painter()
            .circle_filled(pos2(x - 10.0, rect.center().y), 4.0, dot);
        ui.painter()
            .galley(pos2(x, rect.center().y - g.size().y * 0.5), g, p.text_2);
    }
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, p.outline),
    );
}

fn empty(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.add(egui::Label::new(RichText::new(text).color(p.text_2)).wrap());
    });
}

fn separator(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0, p.outline),
    );
}

fn center_text(ui: &mut Ui, rect: Rect, text: &str, color: Color32) {
    let g = ui.painter().layout(
        text.to_owned(),
        theme::regular(14.0),
        color,
        (rect.width() - 48.0).max(80.0),
    );
    ui.painter()
        .galley(rect.center() - g.size() * 0.5, g, color);
}

/// A 40 point round button on the stage: white on the stage, hover rgba(255,255,255,.1),
/// disabled at 30% with its reason as the tooltip. `popup` gives it the stage
/// popup fill (the clear-selection button).
/// Where the stage's selection title draws (top left): the avatar, the clipped text
/// block and the Clear button. One place, so the galaxy's warnings can be placed
/// under all three ([`warnings_top`]).
struct SelectionTitle {
    avatar: Rect,
    text: Rect,
    clear: Rect,
}

impl SelectionTitle {
    fn on(stage: Rect) -> Self {
        let avatar = Rect::from_min_size(
            stage.min + vec2(12.0, size::SELECTION_TITLE_TOP),
            vec2(40.0, 40.0),
        );
        let text_w = (stage.width() - 360.0).max(120.0);
        let text = Rect::from_min_size(
            avatar.right_top() + vec2(12.0, 0.0),
            vec2(text_w, size::SELECTION_TITLE_H),
        );
        let clear = Rect::from_center_size(
            pos2(avatar.right() + 24.0 + text_w, avatar.center().y),
            vec2(size::ICON_BUTTON, size::ICON_BUTTON),
        );
        Self {
            avatar,
            text,
            clear,
        }
    }

    /// The lowest point any part reaches.
    fn bottom(&self) -> f32 {
        self.avatar
            .bottom()
            .max(self.text.bottom())
            .max(self.clear.bottom())
    }
}

/// Where the galaxy's warnings start, in points below the stage's top edge: 8 points
/// under the lowest part of the selection title, so the two never overlap.
fn warnings_top(stage: Rect) -> f32 {
    SelectionTitle::on(stage).bottom() - stage.top() + 8.0
}

/// Gives every open project view the stage palette for `vision` when it differs from
/// the one last given (`last`); a view opened later takes it at open. Returns whether
/// it re-applied.
pub(crate) fn sync_stage_vision<'a>(
    last: &mut Option<kgraf_core::palette::Vision>,
    vision: kgraf_core::palette::Vision,
    views: impl Iterator<Item = &'a mut ProjectView>,
) -> bool {
    if *last == Some(vision) {
        return false;
    }
    let palette = theme::galaxy_palette(vision);
    for v in views {
        v.set_palette(palette);
    }
    *last = Some(vision);
    true
}

/// The color vision `sync_stage_vision` should apply to every open project's
/// stage: whatever Settings > Appearance holds (decision 0070), the same
/// setting [`crate::theme::current_vision`] applies to the page's chrome, so
/// a color-blind mode covers the graph too, not only the shell around it. A
/// free function, not a method, so a test can exercise it (and the
/// `sync_stage_vision` it feeds) without building a full `ShellApp`.
pub(crate) fn stage_vision_from(
    settings: &crate::settings::Settings,
) -> kgraf_core::palette::Vision {
    settings.color_vision
}

impl ShellApp {
    /// The color vision the stage is drawn for. See [`stage_vision_from`].
    fn stage_vision(&self) -> kgraf_core::palette::Vision {
        stage_vision_from(&self.settings)
    }
}

fn stage_button(
    ui: &mut Ui,
    center: egui::Pos2,
    icon: char,
    tip: &str,
    enabled: bool,
    popup: bool,
) -> egui::Response {
    let rect = Rect::from_center_size(center, vec2(size::ICON_BUTTON, size::ICON_BUTTON));
    let id = ui.id().with((
        "stage_button",
        icon as u32,
        (center.x as i32, center.y as i32),
    ));
    let resp = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if popup {
        ui.painter()
            .circle_filled(center, 20.0, theme::stage_popup());
    }
    if enabled && resp.hovered() {
        ui.painter()
            .circle_filled(center, 20.0, Color32::from_white_alpha(26));
    }
    let color = if enabled {
        Color32::WHITE
    } else {
        Color32::WHITE.gamma_multiply(0.3)
    };
    glyph(ui.painter(), center, icon, 24.0, color, 0.0);
    let label = tip.to_owned();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &label));
    // A disabled button still says why on hover (its reason is the tip).
    resp.on_hover_text(tip)
}

/// A vertical gradient from `top` to `bottom` over `rect`.
fn gradient(painter: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

/// A horizontal gradient from `left` to `right` over `rect`.
fn gradient_h(painter: &egui::Painter, rect: Rect, left: Color32, right: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    /// A changed color vision reaches every open project view; an unchanged one
    /// touches none.
    #[test]
    fn a_vision_change_recolors_every_open_stage() {
        use kgraf_core::palette::Vision;
        use kgraf_galaxy::{Palette, ProjectView, RenderBudget};
        let ctx = egui::Context::default();
        let dirs = [
            tempfile::tempdir().expect("a"),
            tempfile::tempdir().expect("b"),
        ];
        let mut views: Vec<ProjectView> = dirs
            .iter()
            .map(|d| {
                ProjectView::open(d.path().to_path_buf(), RenderBudget::Cpu, &ctx).expect("open")
            })
            .collect();
        // Stand-in for a palette from another vision mode (only one exists yet).
        let other = Palette {
            pill: egui::Color32::from_rgb(1, 2, 3),
            ..Palette::default()
        };
        for v in &mut views {
            v.set_palette(other);
        }
        let mut last = None;
        assert!(super::sync_stage_vision(
            &mut last,
            Vision::Standard,
            views.iter_mut()
        ));
        let want = super::theme::galaxy_palette(Vision::Standard);
        assert!(views.iter().all(|v| *v.palette() == want), "every view");
        for v in &mut views {
            v.set_palette(other);
        }
        assert!(!super::sync_stage_vision(
            &mut last,
            Vision::Standard,
            views.iter_mut()
        ));
        assert!(
            views.iter().all(|v| *v.palette() == other),
            "unchanged: untouched"
        );
    }

    /// A color vision chosen in Settings > Appearance reaches every open
    /// project's stage: `stage_vision_from` reads `Settings::color_vision`
    /// (decision 0070), and feeding its result to `sync_stage_vision` on a
    /// change recolors the views to that mode's galaxy palette, not just the
    /// standard one `ShellApp::new` would otherwise leave in place.
    #[test]
    fn a_changed_color_vision_setting_reaches_the_galaxy_through_stage_vision() {
        use kgraf_core::palette::Vision;
        use kgraf_galaxy::{ProjectView, RenderBudget};
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut views: Vec<ProjectView> = vec![
            ProjectView::open(dir.path().to_path_buf(), RenderBudget::Cpu, &ctx).expect("open"),
        ];
        let mut settings = crate::settings::Settings::default();
        assert_eq!(super::stage_vision_from(&settings), Vision::Standard);

        let mut last = None;
        assert!(super::sync_stage_vision(
            &mut last,
            super::stage_vision_from(&settings),
            views.iter_mut()
        ));
        let standard_palette = *views[0].palette();
        assert_eq!(
            standard_palette,
            super::theme::galaxy_palette(Vision::Standard)
        );

        // The user picks Protanopia in Settings > Appearance.
        settings.color_vision = Vision::Protanopia;
        assert_eq!(super::stage_vision_from(&settings), Vision::Protanopia);
        assert!(super::sync_stage_vision(
            &mut last,
            super::stage_vision_from(&settings),
            views.iter_mut()
        ));
        let protan_palette = *views[0].palette();
        assert_eq!(
            protan_palette,
            super::theme::galaxy_palette(Vision::Protanopia)
        );
        assert_ne!(
            protan_palette, standard_palette,
            "the setting change must actually reach the view's palette"
        );
    }

    /// The galaxy's warnings start below every part of the selection title as the
    /// layout draws it (avatar, text block, Clear button), on narrow and wide stages
    /// and wherever the stage sits in the window.
    #[test]
    fn the_warnings_start_below_the_selection_title() {
        use egui::{Rect, pos2, vec2};
        for (x, y, w) in [
            (0.0, 0.0, 300.0),
            (240.0, 72.0, 1_200.0),
            (80.0, 120.0, 3_000.0),
        ] {
            let stage = Rect::from_min_size(pos2(x, y), vec2(w, 800.0));
            let title = super::SelectionTitle::on(stage);
            let warnings = stage.top() + super::warnings_top(stage);
            assert!(
                title.bottom() < warnings,
                "{} >= {warnings} on {stage:?}",
                title.bottom()
            );
        }
    }

    /// A picker answer that comes after its form closed is discarded; a
    /// later form does not receive it (low item).
    #[test]
    fn a_late_picker_answer_is_discarded() {
        let mut chrome = super::ChromeState {
            add_open: true,
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        chrome.picker.start(rx);
        chrome.close_add_form();
        chrome.add_open = true;
        assert!(chrome.picker.busy(), "the old dialog is still up");
        assert_eq!(chrome.picker.poll(), None);
        tx.send(Some(std::path::PathBuf::from("/late")))
            .expect("send");
        assert_eq!(chrome.picker.poll(), None, "the late answer is dropped");
        assert!(!chrome.picker.busy());
        // A picker asked by the open form is answered.
        let (tx, rx) = std::sync::mpsc::channel();
        chrome.picker.start(rx);
        tx.send(Some(std::path::PathBuf::from("/now")))
            .expect("send");
        assert_eq!(
            chrome.picker.poll(),
            Some(super::PickerAnswer::Chosen("/now".into()))
        );
        // A picker thread that died answers Failed.
        let (tx, rx) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
        chrome.picker.start(rx);
        drop(tx);
        assert_eq!(chrome.picker.poll(), Some(super::PickerAnswer::Failed));
    }

    use super::*;

    /// An offered update reaches the bell: it counts, says the version, and
    /// carries the candidate the button accepts. A finished check says nothing.
    #[test]
    fn an_offered_update_is_a_bell_notice_with_a_button() {
        let candidate = crate::update_check::UpdateCandidate {
            version: semver::Version::new(9, 9, 9),
            tag_name: "v9.9.9".into(),
            asset_name: "a".into(),
            download_url: "u".into(),
            checksum_name: "c".into(),
            checksum_url: "cu".into(),
        };
        let offered = Some(UpdateState::Available(candidate.clone()));
        assert!(update_needs_you(&offered));
        let (words, button) = update_notice(&offered).expect("a notice");
        assert!(words.contains("9.9.9"), "{words}");
        assert_eq!(button, Some(candidate));
        for quiet in [
            None,
            Some(UpdateState::Idle),
            Some(UpdateState::Checking),
            Some(UpdateState::UpToDate),
        ] {
            assert!(!update_needs_you(&quiet));
            assert!(update_notice(&quiet).is_none());
        }
        let (words, button) =
            update_notice(&Some(UpdateState::Failed("offline".into()))).expect("a failure is said");
        assert!(words.contains("offline") && button.is_none(), "{words}");
        assert!(update_notice(&Some(UpdateState::Downloading)).is_some());
    }

    /// A refused credential reaches the window as one line,
    /// naming the kind; a scrubber that refused `lm-studio` says so.
    #[test]
    fn a_refused_credential_becomes_one_notice_line() {
        assert_eq!(refusal_notice(Vec::new()), None);
        let s = kgraf_secrets::Scrubber::new();
        s.register_exact_secret("provider-api-key", "lm-studio");
        s.register_exact_secret("mcp-header-env", "changeme");
        let said = refusal_notice(s.take_refusal_notices()).expect("a notice");
        assert!(said.contains("provider-api-key"), "{said}");
        assert!(said.contains("and 1 more"), "{said}");
        assert!(
            !said.contains("lm-studio") && !said.contains("changeme"),
            "{said}"
        );
    }

    #[test]
    fn the_branch_is_read_from_head_and_from_a_worktree_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
        std::fs::write(
            dir.path().join(".git/HEAD"),
            "ref: refs/heads/kgraf/run-0927\n",
        )
        .expect("write");
        assert_eq!(git_branch(dir.path()).as_deref(), Some("kgraf/run-0927"));
        let wt = tempfile::tempdir().expect("tempdir");
        let gitdir = dir.path().join(".git/worktrees/w");
        std::fs::create_dir_all(&gitdir).expect("mkdir");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature\n").expect("write");
        std::fs::write(
            wt.path().join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("write");
        assert_eq!(git_branch(wt.path()).as_deref(), Some("feature"));
        std::fs::write(dir.path().join(".git/HEAD"), "0123abcd\n").expect("write");
        assert_eq!(git_branch(dir.path()), None, "detached");
    }

    /// Decision 0089: the model list shows each route's kind, and a spec a run
    /// would refuse says why.
    #[test]
    fn the_model_list_shows_each_routes_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".kgraf")).expect("mkdir");
        std::fs::write(
            run_json_path(dir.path()),
            r#"{"routes": ["ollama:llama3"], "planner": "anthropic:m",
                "model_routes": ["embedding=openai:text-embedding-3-small",
                                 "typed-decision=systemone:jev-latest"]}"#,
        )
        .expect("write");
        let rj = read_run_json(dir.path());
        assert_eq!(rj.model_routes.len(), 2);
        assert_eq!(role_line("worker", &rj.routes[0]), "worker, generative");
        assert_eq!(
            model_route_row(&rj.model_routes[0]).1,
            "embedding, reached through the embed tool"
        );
        assert_eq!(
            model_route_row(&rj.model_routes[1]).1,
            "typed-decision, reached through the decide tool"
        );
        assert!(role_line("worker", "rerank=llama-cpp:r").contains("refused"));
        assert!(model_route_row("openai:e").1.starts_with("no kind"));
        assert!(
            model_route_row("world=openai:e")
                .1
                .contains("unknown model kind")
        );
    }

    #[test]
    fn typed_paths_expand_home_and_refuse_relative() {
        let home = Path::new("/home/u");
        assert_eq!(
            resolve_typed_path("~", Some(home)),
            Ok(PathBuf::from("/home/u"))
        );
        assert_eq!(
            resolve_typed_path(" ~/src/p ", Some(home)),
            Ok(PathBuf::from("/home/u/src/p"))
        );
        if cfg!(windows) {
            assert_eq!(
                resolve_typed_path(r" ~\src\p ", Some(home)),
                Ok(PathBuf::from("/home/u/src/p"))
            );
        }
        // An absolute path is platform-specific: `/abs/p` has no drive on Windows.
        let abs = std::env::temp_dir().join("abs-p");
        assert_eq!(
            resolve_typed_path(&abs.to_string_lossy(), Some(home)),
            Ok(abs.clone())
        );
        let err = resolve_typed_path("src/p", Some(home)).expect_err("relative");
        assert!(err.contains("relative path") && err.contains('~'), "{err}");
        assert!(resolve_typed_path("~/p", None).is_err());
    }

    /// A cached branch follows `.git/HEAD` when the file changes,
    /// and a missing file becomes a value once it appears.
    #[test]
    fn a_file_cache_rereads_a_changed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
        let head = git_head_path(dir.path()).expect("head path");
        std::fs::write(&head, "ref: refs/heads/one\n").expect("write");
        let mut cache = FileCache::new(Duration::ZERO);
        let read = |c: &mut FileCache<Option<String>>| c.get(&head, || git_branch(dir.path()));
        assert_eq!(read(&mut cache).as_deref(), Some("one"));
        // A later write moves the modification time.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&head, "ref: refs/heads/two\n").expect("write");
        assert_eq!(read(&mut cache).as_deref(), Some("two"), "reread");
        // Unchanged: not read again.
        let mut loads = 0;
        let _ = cache.get(&head, || {
            loads += 1;
            None
        });
        assert_eq!(loads, 0);
        // run.json that appears later is read then.
        let mut rj = FileCache::new(Duration::ZERO);
        let path = run_json_path(dir.path());
        assert!(rj.get(&path, || read_run_json(dir.path())).error.is_some());
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, r#"{"routes": ["kimi:k2"]}"#).expect("write");
        assert_eq!(
            rj.get(&path, || read_run_json(dir.path())).routes,
            ["kimi:k2"]
        );
    }

    /// INT-3: each Escape closes one layer, the top one first, until none is
    /// left. The layers are applied as the window applies them.
    #[test]
    fn escape_closes_one_layer_per_press() {
        let mut open = Layers {
            dialog: true,
            menu: true,
            selection: true,
            filter: true,
            search: true,
        };
        let mut closed = Vec::new();
        while let Some(layer) = escape_layer(open) {
            closed.push(layer);
            match layer {
                Layer::Dialog => open.dialog = false,
                Layer::Menu => open.menu = false,
                Layer::Selection => open.selection = false,
                // One query field: clearing it ends the filter and the search.
                Layer::Filter | Layer::Search => {
                    open.filter = false;
                    open.search = false;
                }
            }
        }
        assert_eq!(
            closed,
            [Layer::Dialog, Layer::Menu, Layer::Selection, Layer::Filter]
        );
        assert_eq!(escape_layer(Layers::default()), None);
        let typed = Layers {
            search: true,
            ..Layers::default()
        };
        assert_eq!(escape_layer(typed), Some(Layer::Search));
        assert!(is_chip_query("status:blocked") && is_chip_query("file:crates/x/"));
        assert!(!is_chip_query("alpha"));
    }

    #[test]
    fn the_scheduler_control_says_what_play_would_do() {
        let (w, _, on, _) = scheduler_words(&PlayAction::Stop);
        assert_eq!((w, on), ("Scheduler running", true));
        let (w, _, on, hint) = scheduler_words(&PlayAction::Resume("r1".into()));
        assert_eq!((w, on), ("Scheduler paused", true));
        assert!(hint.contains("r1"));
        let (_, _, on, hint) = scheduler_words(&PlayAction::Disabled("why".into()));
        assert!(!on);
        assert_eq!(hint, "why");
    }

    /// Settings > Providers states reasoning visibility as a catalog fact,
    /// split on `route` or `route:model`, and says plainly
    /// when a route is not in the catalog rather than guessing.
    #[test]
    fn the_providers_settings_line_names_the_declared_reasoning_visibility() {
        let line = reasoning_line("anthropic");
        assert!(line.contains("none"), "{line}");
        assert!(line.contains("verified"), "{line}");
        let line = reasoning_line("kimi:kimi-k3");
        assert!(
            line.contains("full reasoning"),
            "{line}: kimi is declared full"
        );
        let line = reasoning_line("zai:glm-x");
        assert!(
            line.contains("unknown"),
            "{line}: an unverified route declares unknown, not a guess"
        );
        let line = reasoning_line("not-a-real-route");
        assert!(line.contains("unknown route"), "{line}");
    }
}

#[cfg(test)]
#[path = "settings_dialog_tests.rs"]
mod settings_dialog_tests;
