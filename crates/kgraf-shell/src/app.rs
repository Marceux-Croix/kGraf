//! The eframe application: its lifecycle (measurement, screenshots, close to
//! tray, quitting with hosted runs) and the open projects. The window's layout,
//! in the Stage style (`docs/STYLE_GUIDE.md`), is drawn by `layout.rs`:
//! - masthead: guide button, kGraf mark and branch, project, search (0032), the
//!   scheduler control, the workers readout, the render badge (0054), the bell;
//! - guide: the mini rail or the sidebar with the project switcher (0066);
//! - center: filter chips, the galaxy on the near-black stage with its legend, minimap
//!   and bottom bar, then the project row, pinned cards and the orchestrator
//!   composer;
//! - right: "Up next", the one text surface (0034), and "Live activity" (0063);
//! - settings: a configuration-only dialog (0017).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use crate::adapter::{AdapterFacts, RenderPath, SoftwareReason, classify};
use crate::diag;
use crate::interaction::Selected;
use crate::layout::ChromeState;
use crate::metrics::FrameClock;
use crate::projects::{Opener, ProjectParts, Workspace};
use crate::resources::{self, ResourceSample, mib};
use crate::settings::Settings;
use crate::surface::DropCounters;
use crate::tray::{Tray, TrayCommand};

/// How long `--measure` stays idle before sampling "idle after reclaim". Apple's Metal
/// driver holds a few hundred MB after rendering and reclaims it after about 1.3 s idle
/// (a plain Swift Metal program shows the same), so the report gives both figures.
const IDLE_RECLAIM_WAIT: Duration = Duration::from_secs(3);

/// Give up on `--measure` if no frame reaches the screen this long after the window.
const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(15);

/// A question turned into a folder-name-safe slug:
/// lower-cased alphanumerics, runs of anything else collapsed to one `-`, cut
/// to 40 characters, `"question"` when nothing alphanumeric survives.
fn project_slug(question: &str) -> String {
    let mut out = String::new();
    for c in question.chars().take(60) {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let capped: String = trimmed.chars().take(40).collect();
    let capped = capped.trim_matches('-');
    if capped.is_empty() {
        "question".to_owned()
    } else {
        capped.to_owned()
    }
}

/// The first of `slug`, `slug-1`, `slug-2`, ... not already a folder under
/// `base`. `None` past 1000 tries (a pathological case, never a real project
/// name), so the caller reports rather than looping forever.
fn first_free_name(base: &std::path::Path, slug: &str) -> Option<String> {
    (0u32..1000)
        .map(|n| {
            if n == 0 {
                slug.to_owned()
            } else {
                format!("{slug}-{n}")
            }
        })
        .find(|name| !base.join(name).exists())
}

/// The adapter the renderer is actually using, read back from eframe after creation.
#[derive(Debug, Clone)]
pub struct RenderStatus {
    pub facts: AdapterFacts,
    pub path: RenderPath,
}

impl RenderStatus {
    pub fn from_render_state(state: &egui_wgpu::RenderState) -> Self {
        let facts = AdapterFacts::from_info(&state.adapter.get_info(), true);
        let path = classify(&facts);
        Self { facts, path }
    }

    /// Text for the always-visible indicator.
    pub fn label(&self) -> String {
        match self.path {
            RenderPath::Hardware => format!("GPU: {} ({})", self.facts.name, self.facts.backend),
            RenderPath::Software(SoftwareReason::KnownRasterizer(r)) => {
                format!("CPU rendering: {r} ({})", self.facts.name)
            }
            RenderPath::Software(SoftwareReason::CpuDeviceType) => {
                format!("CPU rendering: {} (CPU device)", self.facts.name)
            }
        }
    }
}

/// Measurement mode for the 0054 confirmation numbers.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeasureOptions {
    /// Exit and print a report this long after the first frame.
    pub run_for: Option<Duration>,
    /// Repaint every frame instead of on change, to measure frame time.
    pub continuous: bool,
}

/// `--measure` state machine; transitions run in `App::logic`.
///
/// ```text
///   WaitingForFirstFrame --first presented frame--> Active
///   WaitingForFirstFrame --deadline---------------> Done (report says no frame)
///   Active --run_for elapsed--> Idle (sampler thread started, no repaints requested)
///   Idle   --sampler result arrives--> Done (print, quit)
/// ```
///
/// The idle sample is taken on a separate thread while the UI thread sleeps. Measured:
/// a sample taken in the wake-up frame's `logic` already reads the re-inflated
/// footprint (494 MB against 89 MB a quarter second earlier), because eframe touches
/// the surface before `logic` runs.
#[derive(Debug)]
enum MeasureState {
    WaitingForFirstFrame,
    Active {
        until: Instant,
    },
    Idle {
        active_end: Option<ResourceSample>,
        sampler: Receiver<(Option<ResourceSample>, Duration)>,
    },
    Done,
}

/// Where settings are persisted, and what was last written.
#[derive(Debug)]
pub struct SettingsStore {
    pub path: PathBuf,
    saved: Settings,
    /// This window's own project list at its last write (or load): what its
    /// own adds and closes are measured against.
    own_projects: Vec<PathBuf>,
    pub(crate) last_error: Option<String>,
    /// After a failed write: when to try again even without a new change.
    retry_at: Option<Instant>,
}

/// How long after a failed settings write it is tried again, unchanged: often
/// enough that a transient failure heals, rare enough not to log every frame.
const SETTINGS_RETRY: Duration = Duration::from_secs(5);

/// The project list to write (1): the file's current list with only
/// this window's own changes applied. `own_before` is this window's list at its
/// last write, `own_now` its list now: what it added is added (at the end, in
/// its order), what it closed is removed, and every other project in the file,
/// another window's included, stays. An unrelated change adds and closes
/// nothing, so it can never drop another window's project. Pure.
pub fn merge_projects(
    own_before: &[PathBuf],
    own_now: &[PathBuf],
    file: &[PathBuf],
) -> Vec<PathBuf> {
    let closed: Vec<&PathBuf> = own_before.iter().filter(|p| !own_now.contains(p)).collect();
    let mut out: Vec<PathBuf> = file
        .iter()
        .filter(|p| !closed.contains(p))
        .cloned()
        .collect();
    for p in own_now.iter().filter(|p| !own_before.contains(p)) {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

impl SettingsStore {
    /// `saved` is what the file holds now (as loaded).
    pub fn new(path: PathBuf, saved: Settings) -> Self {
        Self {
            path,
            own_projects: saved.projects.clone(),
            saved,
            last_error: None,
            retry_at: None,
        }
    }

    /// The advisory lock around the read-modify-write, in a sibling file so the
    /// settings file itself can be replaced by rename while it is held.
    fn lock(&self) -> Result<kgraf_core::flock::HeldLock, String> {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".lock");
        let lock = PathBuf::from(name);
        if let Some(dir) = lock.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .map_err(|e| format!("{}: {e}", lock.display()))?;
        file.lock()
            .map_err(|e| format!("locking {}: {e}", lock.display()))?;
        Ok(kgraf_core::flock::HeldLock::new(file))
    }

    /// Writes `current` when it differs from what was last saved. Under an
    /// advisory lock, the file is read again and only this window's own
    /// project adds and closes are applied to its list ([`merge_projects`]), so
    /// another window's projects are never dropped. A failed write is logged and
    /// shown in the settings surface, and retried on the next change
    /// or after [`SETTINGS_RETRY`], never every frame. The own list advances only
    /// when a write lands (final check item 2): a project added during a failed
    /// write is still this window's add at the retry, so it is not lost.
    pub fn persist_if_changed(&mut self, current: &Settings) {
        let retry_due = self.retry_at.is_some_and(|t| Instant::now() >= t);
        if *current == self.saved && !retry_due {
            return;
        }
        let result = self.lock().and_then(|_held| {
            let file = match Settings::load(&self.path) {
                Ok(s) => s.projects,
                // An unreadable file is not merged into; the error says so and
                // this window's own list is written.
                Err(e) => {
                    log::warn!("settings: the file could not be read for the merge: {e}");
                    current.projects.clone()
                }
            };
            let mut write = current.clone();
            write.projects = merge_projects(&self.own_projects, &current.projects, &file);
            if write
                .active_project
                .as_ref()
                .is_some_and(|a| !write.projects.contains(a))
            {
                write.active_project = None;
            }
            write.save(&self.path).map_err(|e| e.to_string())
        });
        match result {
            Ok(()) => {
                self.last_error = None;
                self.retry_at = None;
                self.own_projects = current.projects.clone();
            }
            Err(e) => {
                log::error!("{e}");
                self.last_error = Some(e);
                self.retry_at = Some(Instant::now() + SETTINGS_RETRY);
            }
        }
        self.saved = current.clone();
    }
}

/// Wait before retrying after a dropped frame. Short, because it bounds the error in
/// "cold start to first frame"; nonzero, so our own requests never spin on a window
/// that wgpu sees as not visible while winit has not reported it occluded.
///
/// Measured on macOS 27: 171 to 407 Occluded drops still occur in the ~40 to 60 ms
/// before the first presented frame with this throttle in place, so those repaints
/// come from eframe/egui while the window is being shown, not from this app.
pub const DROPPED_FRAME_RETRY: Duration = Duration::from_millis(8);

/// What the frame asks of the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepaintRequest {
    None,
    Now,
    After(Duration),
}

/// Whether and when this frame should ask for another one. Pure.
///
/// Never while the window is occluded: a covered window in continuous mode would
/// otherwise spin a core on frames the compositor discards. egui-winit requests a
/// repaint when the window becomes visible again. After a dropped frame (for example
/// the still-hidden window at startup, which winit does not report as occluded), wait
/// `DROPPED_FRAME_RETRY` instead of spinning.
pub fn repaint_request(
    continuous: bool,
    first_frame_presented: bool,
    occluded: bool,
    measure_idle: bool,
    previous_frame_dropped: bool,
) -> RepaintRequest {
    // Repaint is wanted in continuous mode, or to confirm the first frame.
    let wanted = continuous || !first_frame_presented;
    if occluded || measure_idle || !wanted {
        RepaintRequest::None
    } else if previous_frame_dropped {
        RepaintRequest::After(DROPPED_FRAME_RETRY)
    } else {
        RepaintRequest::Now
    }
}

/// Cmd+Q (Ctrl+Q elsewhere) as seen by egui. Pure over the input state.
pub fn quit_shortcut_pressed(input: &egui::InputState) -> bool {
    input.modifiers.command && input.key_pressed(egui::Key::Q)
}

/// `--screenshot`: wait this long after the first frame (the graph loads and the
/// camera settles), then capture the window, write the PNG and quit.
#[derive(Debug, Clone)]
pub struct ScreenshotOptions {
    pub path: PathBuf,
    pub after: Duration,
}

#[derive(Debug)]
enum ScreenshotState {
    Waiting,
    Requested(Instant),
    Done,
}

pub struct ShellApp {
    pub(crate) settings: Settings,
    pub(crate) store: SettingsStore,
    pub(crate) render: RenderStatus,
    pub(crate) tray: Option<Tray>,
    clock: FrameClock,
    drops: Arc<DropCounters>,
    measure: MeasureOptions,
    measure_state: MeasureState,
    refresh_mhz: Option<u32>,
    quitting: bool,
    /// The open projects (decision 0066). Each keeps its own host, scheduler,
    /// graph and runs; one is shown.
    pub(crate) workspace: Workspace,
    /// Builds a project's host and scheduler when the user adds one. `None` in
    /// headless tests, where adding a project opens it without a host.
    pub(crate) opener: Option<Opener>,
    /// Chrome state: which overlays and menus are open (not persisted, except
    /// the guide's open state, which is configuration).
    pub(crate) chrome: ChromeState,
    /// Hosted runs being stopped because the window is quitting.
    shutdown: Option<Receiver<bool>>,
    /// When the user last gave input (attendance needs recent input).
    last_input: Option<Instant>,
    /// Frames whose input carried user input, and frames with none: `--measure`
    /// reports them apart, so "no repaint while idle" can be read off the second.
    frames_with_input: u64,
    frames_without_input: u64,
    /// Of those frames, the ones with the pointer over the window: hover
    /// alone can wake egui, so a run meant to show the app's own frames reports
    /// that the pointer stayed outside (measure with it outside).
    frames_pointer_inside: u64,
    screenshot: Option<(ScreenshotOptions, ScreenshotState)>,
    /// The screenshot's waker thread is running.
    screenshot_waker: bool,
    /// When the capture was last asked for.
    screenshot_sent: Option<Instant>,
    /// The appearance and color vision mode last applied to the context
    /// (decision 0070); re-applied whenever either moves, so a color-blind
    /// mode change takes effect with no restart.
    applied_appearance: Option<(crate::settings::Appearance, kgraf_core::palette::Vision)>,
    /// The animation clock's zero.
    pub(crate) started: Instant,
    /// The shared, process-wide machine gate the parallelism slider drives
    /// (decision 0067). The real app
    /// passes `kgraf_orchestrator::process_machine_gate(...)`, the same
    /// singleton every open project's `Host` shares; a caller that never
    /// opens a `Host` (a future headless test constructing a full
    /// `ShellApp`) passes its own fresh `MachineGate::new(...)` instead, so
    /// it never touches the process-wide one.
    pub(crate) machine_gate: Arc<kgraf_orchestrator::MachineGate>,
    /// The re-measure thread (`crate::capacity_bg`), when one was started.
    /// Spawned by the caller before `ShellApp::new` (never inside it: a
    /// headless test creating a `ShellApp` must never start a background
    /// thread, the same rule `AppParts`'s own doc comment states for the
    /// scheduler), and kept alive here so it stops when the app closes.
    pub(crate) capacity: Option<crate::capacity_bg::CapacityHandle>,
    /// The parallelism value last pushed to `machine_gate` (idempotency
    /// guard, same pattern `Interaction::applied_caps` uses): avoids taking
    /// `MachineGate`'s locks every frame when nothing changed.
    applied_parallelism: Option<kgraf_core::ParallelismLimits>,
    /// The update-check thread, when one was started.
    /// Spawned by the caller before `ShellApp::new`, same rule `capacity`'s own doc
    /// comment states (a headless test creating a `ShellApp` must never start a
    /// background thread).
    pub(crate) update: Option<crate::update_bg::UpdateHandle>,
    /// The compaction recommendation's background read (`crate::compaction_measure`).
    /// `None` until [`ShellApp::enable_compaction_measurement`]: a headless test
    /// creating a `ShellApp` must never start a background thread.
    compaction_measure: Option<crate::compaction_measure::Measurement>,
    /// The user's live permission control (decision 0094).
    pub(crate) permissions: Option<crate::permission_bar::PermissionUi>,
}

/// Everything `ShellApp` needs at creation.
pub struct AppParts {
    pub settings: Settings,
    pub store: SettingsStore,
    pub render: RenderStatus,
    pub tray: Option<Tray>,
    pub clock: FrameClock,
    pub drops: Arc<DropCounters>,
    pub measure: MeasureOptions,
    /// The projects already opened by the caller (the `--project` folder and the
    /// ones the settings file restores). Spawning their schedulers is the
    /// caller's job, since a headless test creating a `ShellApp` must never start
    /// a background thread.
    pub workspace: Workspace,
    /// Builds a project the user adds from the switcher.
    pub opener: Option<Opener>,
    pub screenshot: Option<ScreenshotOptions>,
    /// The shared machine gate (decision 0067): see
    /// [`ShellApp::machine_gate`]'s doc comment.
    pub machine_gate: Arc<kgraf_orchestrator::MachineGate>,
    /// The re-measure thread, already spawned by the caller (see
    /// [`ShellApp::capacity`]'s doc comment for why `ShellApp::new` never
    /// spawns it itself).
    pub capacity: Option<crate::capacity_bg::CapacityHandle>,
    /// The update-check thread, already spawned by the caller (see
    /// [`ShellApp::update`]'s doc comment).
    pub update: Option<crate::update_bg::UpdateHandle>,
    /// The user's live permission control (decision 0094), for the settings
    /// surface's Allow Bypass switch. `None` in tests that build no control.
    pub permissions: Option<crate::permission_bar::PermissionUi>,
}

impl ShellApp {
    pub fn new(parts: AppParts) -> Self {
        let chrome = ChromeState::default();
        Self {
            settings: parts.settings,
            store: parts.store,
            render: parts.render,
            tray: parts.tray,
            clock: parts.clock,
            drops: parts.drops,
            measure: parts.measure,
            measure_state: MeasureState::WaitingForFirstFrame,
            refresh_mhz: None,
            quitting: false,
            workspace: parts.workspace,
            opener: parts.opener,
            chrome,
            shutdown: None,
            last_input: None,
            frames_with_input: 0,
            frames_without_input: 0,
            frames_pointer_inside: 0,
            screenshot: parts.screenshot.map(|s| (s, ScreenshotState::Waiting)),
            screenshot_waker: false,
            screenshot_sent: None,
            applied_appearance: None,
            started: Instant::now(),
            machine_gate: parts.machine_gate,
            capacity: parts.capacity,
            applied_parallelism: None,
            update: parts.update,
            permissions: parts.permissions,
            compaction_measure: None,
        }
    }

    /// Starts following the compaction point measured from stored runs (decision
    /// 0057): from the next logic pass the slider's recommendation is read from
    /// every open project's run traces, off the UI thread.
    pub fn enable_compaction_measurement(&mut self) {
        self.compaction_measure
            .get_or_insert_with(crate::compaction_measure::Measurement::default);
    }

    /// Applies a finished measurement to the slider: a slider the user has not
    /// moved follows it, a moved one keeps its value.
    pub(crate) fn sync_compaction_recommendation(&mut self) {
        let Some(measure) = self.compaction_measure.as_mut() else {
            return;
        };
        let workspace = &self.workspace;
        let found = measure.poll(Instant::now(), || workspace.roots());
        if let Some(recommendation) = found {
            self.settings
                .compaction
                .update_recommendation(recommendation);
        }
    }

    /// The update-check thread's current state, for the Settings panel.
    pub fn update_state(&self) -> Option<crate::update_bg::UpdateState> {
        self.update.as_ref().map(|u| u.state())
    }

    /// The shared machine gate the parallelism slider drives.
    pub fn machine_gate(&self) -> &Arc<kgraf_orchestrator::MachineGate> {
        &self.machine_gate
    }

    /// The capacity breakdown the re-measure thread last accepted, if the
    /// thread was started (`None` otherwise, and until its first reading).
    pub fn capacity_breakdown(&self) -> Option<kgraf_capacity::Capacity> {
        self.capacity.as_ref().and_then(|c| c.latest())
    }

    /// Pushes the parallelism slider's effective value to `machine_gate`
    /// when it has changed (decision 0067: when
    /// the user moves the slider, the shell calls `gate.set_limits`), and
    /// pulls the re-measure thread's own computed maximum into the slider.
    ///
    /// **The slider's maximum must never come from `machine_gate.limits()`.**
    /// `MachineGate::limits().max_agents` is the *effective* limit: the
    /// user's own choice (or a following slider's last pushed value),
    /// already in force. The slider's own maximum is a different number
    /// entirely: `kgraf-capacity`'s *computed* capacity
    /// (`CapacityHandle::latest`), which changes only when the machine or
    /// the routes do. An earlier version of this method read the gate's
    /// `max_agents` back into `update_max`; that would cause a bug:
    /// if the user sets the slider to 3, the gate
    /// then reports an effective limit of 3, and the *next* frame would
    /// call `update_max(3)`, collapsing the slider's own maximum to 3 and
    /// leaving no way to raise it again. See [`apply_parallelism`] (the pure
    /// core this delegates to, and its own tests) for the fix and the
    /// regression test.
    pub(crate) fn sync_parallelism(&mut self) {
        let computed = self
            .capacity_breakdown()
            .map(|c| (c.max_agents, c.max_writers, c.exec_slots));
        apply_parallelism(
            &mut self.settings.parallelism,
            &self.machine_gate,
            computed,
            &mut self.applied_parallelism,
        );
    }

    /// Runs every gate writer once: the capacity thread's sources, the
    /// process-wide `MachineGate` (decision 0067: the slider drives the one
    /// gate every open project shares), and each project's own `Admission`,
    /// which needs the derived `Limits` applied to its own `AdmissionGate`.
    /// Every open project's local ceiling tracks the slider's effective
    /// value (`max_writers`, the free-disk bound `kgraf-capacity` measures,
    /// stands in for the retired `max_work_items` control); the shared
    /// `MachineGate` keeps several projects together within it.
    pub(crate) fn sync_gates(&mut self) {
        // Sources first, so a project opened this frame is visible to the
        // next re-measure tick.
        self.sync_capacity_sources();
        self.sync_compaction_recommendation();
        self.sync_parallelism();
        let max_writers = self.machine_gate().limits().max_writers.max(1) as usize;
        let effective_agents = self.settings.parallelism.effective().max(1) as usize;
        let limits = kgraf_orchestrator::Limits::new(max_writers, effective_agents)
            .expect("max_writers and the slider's effective value are both at least 1");
        for proj in self.workspace.projects_mut() {
            if let Some(i) = proj.interaction.as_mut() {
                i.sync_parallelism(limits);
            }
        }
    }

    /// Refreshes what the re-measure thread reads for the writer bound
    /// (every open project's root) and the route bound (every open
    /// project's housekeeping handle, decision 0067). Cheap: this only replaces a
    /// `Vec` behind a lock the background thread reads once per tick: safe
    /// to call every frame.
    pub(crate) fn sync_capacity_sources(&mut self) {
        let Some(capacity) = &self.capacity else {
            return;
        };
        let mut roots = Vec::new();
        let mut projects: Vec<Arc<dyn crate::orchestrator_host::RunHousekeeping>> = Vec::new();
        for proj in self.workspace.projects() {
            roots.push(proj.root.clone());
            if let Some(i) = &proj.interaction
                && let Some(h) = i.host().housekeeping()
            {
                projects.push(h);
            }
        }
        capacity.set_volumes(roots);
        capacity.set_projects(projects);
    }

    /// The open projects (tests, the report).
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Shows the schedule boards' line on the tray: one project's line as is,
    /// several as `name: line`, joined. The schedulers wake the UI when a board
    /// changes, so this runs on those frames, with no timer;
    /// `Tray::update_schedule_summary` skips the OS menu rebuild when the text is
    /// the same.
    fn refresh_tray_schedules(&mut self) {
        let Some(tray) = &mut self.tray else {
            return;
        };
        let lines: Vec<(String, String)> = self
            .workspace
            .projects()
            .iter()
            .filter_map(|p| {
                p.scheduler
                    .as_ref()
                    .map(|s| (p.name(), s.board().snapshot().line()))
            })
            .collect();
        let line = match lines.as_slice() {
            [] => return,
            [(_, one)] => one.clone(),
            many => many
                .iter()
                .map(|(n, l)| format!("{n}: {l}"))
                .collect::<Vec<_>>()
                .join("; "),
        };
        if let Err(e) = tray.update_schedule_summary(&line) {
            log::error!("could not update the tray's schedule summary: {e}");
        }
    }

    /// What is selected on the shown project's graph.
    pub(crate) fn selected(&self) -> Selected {
        let found = || {
            let view = self.workspace.active()?.galaxy.as_ref()?;
            let slot = view.view()?.galaxy().selection()?.selected();
            let model = view.model()?;
            Some(Selected {
                slot: Some(slot),
                agent: model.agent_at(slot).map(str::to_owned),
                work: model.plan_at(slot).map(|n| n.key.clone()),
            })
        };
        found().unwrap_or_default()
    }

    /// Opens a folder from the switcher: canonical, a directory, not open yet.
    /// Shows it. Returns why not, when refused.
    pub fn open_project(&mut self, path: &std::path::Path) -> Result<usize, String> {
        let root = std::fs::canonicalize(path)
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| format!("{} is not a folder", path.display()))?;
        let background = self.settings.background_mode;
        let worktrees = self.settings.worktree_retention();
        let opener = &self.opener;
        let i = self.workspace.open(root.clone(), || match opener {
            Some(open) => open(&root, background, worktrees),
            None => ProjectParts::default(),
        });
        self.workspace.switch_to(i);
        self.sync_project_settings();
        Ok(i)
    }

    /// Picks up every open project's
    /// accepted "start a new project" offer (`BoxModel::take_new_project_request`)
    /// and opens one new project per question, seeded with it, through
    /// [`Self::open_new_seeded_project`]. Called once a frame, right after
    /// `Workspace::tick`.
    pub(crate) fn take_new_project_offers(&mut self) {
        let requests: Vec<(usize, String, Option<String>)> = self
            .workspace
            .projects_mut()
            .iter_mut()
            .enumerate()
            .filter_map(|(idx, p)| {
                let (question, graph_id) = p
                    .interaction
                    .as_mut()?
                    .box_model
                    .take_new_project_request()?;
                Some((idx, question, graph_id))
            })
            .collect();
        for (origin, question, graph_id) in requests {
            match self.open_new_seeded_project(&question) {
                Ok(new_i) => {
                    let new_name = self.workspace.projects()[new_i].name();
                    self.record_offer_resolution(
                        origin,
                        graph_id.as_deref(),
                        &question,
                        kgraf_orchestrator::OfferResolution::Accepted {
                            new_project: new_name,
                        },
                    );
                }
                Err(e) => {
                    self.chrome.notice =
                        Some((format!("new project not started: {e}"), Instant::now()));
                }
            }
        }
    }

    /// Records an offer's resolution on the *origin* project's graph, at index `origin` in the
    /// workspace: the new project this same call may just have opened is a
    /// different project's graph entirely, and never the one the offer was
    /// made on. A no-op when the offer has no graph item (its own write
    /// failed earlier) or the origin project cannot be reached any more.
    fn record_offer_resolution(
        &mut self,
        origin: usize,
        graph_id: Option<&str>,
        question: &str,
        resolution: kgraf_orchestrator::OfferResolution,
    ) {
        let Some(graph_id) = graph_id else { return };
        let Some(host) = self
            .workspace
            .projects()
            .get(origin)
            .and_then(|p| p.interaction.as_ref())
            .and_then(|i| i.host().project_host())
        else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let _ = host.with_graph(|g| {
            kgraf_orchestrator::record_offer_resolved(g, now, graph_id, question, resolution)
        });
    }

    /// A fresh folder under kGraf's own data directory, named from `question`
    /// (a slug, made unique against what is already there): the offer names
    /// no folder of its own, unlike "Add project"'s picker.
    fn new_project_folder(question: &str) -> Result<PathBuf, String> {
        let dirs = directories::ProjectDirs::from("", "", crate::app_dirs::APP_NAME)
            .ok_or_else(|| "no home directory to place a new project under".to_owned())?;
        let base = dirs.data_dir().join("projects");
        first_free_name(&base, &project_slug(question))
            .map(|name| base.join(name))
            .ok_or_else(|| "could not find a free project folder name".to_owned())
    }

    /// Opens a new project seeded with `question` as its starting instruction,
    /// through the same path "Add project" uses (`crate::projects::open_seeded_project`,
    /// `Workspace::open` underneath): accepting
    /// creates the project through the same path 'Add project' uses".
    pub fn open_new_seeded_project(&mut self, question: &str) -> Result<usize, String> {
        let root = Self::new_project_folder(question)?;
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let background = self.settings.background_mode;
        let worktrees = self.settings.worktree_retention();
        let opener = &self.opener;
        let i = crate::projects::open_seeded_project(
            &mut self.workspace,
            root.clone(),
            || match opener {
                Some(open) => open(&root, background, worktrees),
                None => ProjectParts::default(),
            },
            question,
        )
        .map_err(|e| e.to_string())?;
        self.workspace.switch_to(i);
        self.sync_project_settings();
        Ok(i)
    }

    /// The settings file's list of open projects follows the workspace.
    pub(crate) fn sync_project_settings(&mut self) {
        self.settings.projects = self.workspace.roots();
        self.settings.active_project = self.workspace.active().map(|p| p.root.clone());
    }

    /// Cmd+Q (Ctrl+Q elsewhere) quits even with background mode on: it is the
    /// standard macOS "quit the app" intent, while closing the window only hides it
    /// to the tray (0052). On macOS winit's default app menu owns Cmd+Q and calls
    /// `terminate:`, which ends the event loop without a CloseRequested, so
    /// `close_to_tray` never sees it. This handler covers Ctrl+Q elsewhere and any
    /// case where the key reaches egui.
    fn handle_quit_shortcut(&mut self, ctx: &egui::Context) {
        if ctx.input(quit_shortcut_pressed) {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };
        for cmd in tray.poll() {
            self.quitting |= apply_tray_command(ctx, cmd);
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        let intercepted = close_to_tray(
            ctx,
            self.quitting,
            self.settings.background_mode,
            self.tray.is_some(),
        );
        let workspace = &mut self.workspace;
        let quit_now = quit_step(ctx, intercepted, &mut self.shutdown, || {
            // Every project's hosted run is stopped; the window quits when all
            // have answered (or timed out).
            let pending: Vec<Receiver<bool>> = workspace
                .projects_mut()
                .iter_mut()
                .filter_map(|p| p.interaction.as_mut())
                .filter_map(|i| i.host_mut().start_shutdown())
                .collect();
            join_all(pending)
        });
        if quit_now {
            self.quitting = true;
        }
    }

    /// `in_ui`: the request and the capture event belong to a `ui` pass (a
    /// viewport command sent from `logic` is not acted on); `logic` only keeps
    /// the frames coming and enforces the timeout.
    fn handle_screenshot(&mut self, ctx: &egui::Context, in_ui: bool) {
        let Some((opts, state)) = &mut self.screenshot else {
            return;
        };
        if !in_ui {
            // A waker thread: a window that is not painted gets no `ui` pass and
            // no repaint of its own, so without it the deadline below would never
            // be checked. It ends with the process (the capture quits the app).
            if !self.screenshot_waker {
                self.screenshot_waker = true;
                let wake = ctx.clone();
                let _ = std::thread::Builder::new()
                    .name("screenshot-waker".into())
                    .spawn(move || {
                        loop {
                            std::thread::sleep(Duration::from_millis(250));
                            wake.request_repaint();
                        }
                    });
            }
            let deadline = opts.after + FIRST_FRAME_DEADLINE;
            let late = match state {
                ScreenshotState::Requested(since) => since.elapsed() > FIRST_FRAME_DEADLINE,
                ScreenshotState::Waiting => self.clock.process_start().elapsed() > deadline,
                ScreenshotState::Done => false,
            };
            if late {
                diag::print_err(
                    "kgraf-app: --screenshot: no capture arrived; the window was not \
                     painted (covered, minimized, or the display asleep)\n",
                );
                *state = ScreenshotState::Done;
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if !matches!(state, ScreenshotState::Done) {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            return;
        }
        match state {
            ScreenshotState::Waiting => match self.clock.first_frame_presented() {
                Some(first) if self.clock.process_start().elapsed() >= first + opts.after => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                    *state = ScreenshotState::Requested(Instant::now());
                    self.screenshot_sent = Some(Instant::now());
                    ctx.request_repaint();
                }
                _ => ctx.request_repaint_after(Duration::from_millis(100)),
            },
            ScreenshotState::Requested(_) => {
                let image = ctx.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Screenshot { image, .. } => Some(image.clone()),
                        _ => None,
                    })
                });
                match image {
                    Some(image) => {
                        let [w, h] = image.size;
                        let png = crate::png::encode_png_rect(w as u32, h as u32, image.as_raw());
                        match std::fs::write(&opts.path, png) {
                            Ok(()) => diag::print_out(&format!(
                                "kgraf-app: screenshot {w}x{h} written to {}\n",
                                opts.path.display()
                            )),
                            Err(e) => diag::print_err(&format!(
                                "kgraf-app: --screenshot {}: {e}\n",
                                opts.path.display()
                            )),
                        }
                        *state = ScreenshotState::Done;
                        self.quitting = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    // A window that is not painted (covered, minimized, the
                    // display asleep) never answers; say so and quit.
                    None => {
                        // A request sent on a frame that was not painted is lost
                        // (seen on macOS); ask again every half second until the
                        // deadline in `logic`.
                        let due = self
                            .screenshot_sent
                            .is_none_or(|at| at.elapsed() > Duration::from_millis(500));
                        if due {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                                Default::default(),
                            ));
                            self.screenshot_sent = Some(Instant::now());
                        }
                        ctx.request_repaint_after(Duration::from_millis(50));
                    }
                }
            }
            ScreenshotState::Done => {}
        }
    }

    fn handle_measure(&mut self, ctx: &egui::Context) {
        let Some(run_for) = self.measure.run_for else {
            return;
        };
        let now = Instant::now();
        let state = std::mem::replace(&mut self.measure_state, MeasureState::Done);
        self.measure_state = match state {
            MeasureState::WaitingForFirstFrame => {
                if self.clock.first_frame_presented().is_some() {
                    ctx.request_repaint_after(run_for);
                    MeasureState::Active {
                        until: now + run_for,
                    }
                } else if self.clock.process_start().elapsed() > run_for + FIRST_FRAME_DEADLINE {
                    diag::print_err("kgraf-app: --measure: no frame reached the screen\n");
                    self.finish_measure(ctx, None, None, None);
                    MeasureState::Done
                } else {
                    ctx.request_repaint_after(Duration::from_millis(250));
                    MeasureState::WaitingForFirstFrame
                }
            }
            MeasureState::Active { until } if now >= until => {
                let (tx, rx) = channel();
                let wake = ctx.clone();
                std::thread::spawn(move || {
                    let start = Instant::now();
                    std::thread::sleep(IDLE_RECLAIM_WAIT);
                    // A closed receiver means the app already quit; nothing to report.
                    if tx.send((resources::sample(), start.elapsed())).is_ok() {
                        wake.request_repaint();
                    }
                });
                MeasureState::Idle {
                    active_end: resources::sample(),
                    sampler: rx,
                }
            }
            MeasureState::Active { until } => {
                ctx.request_repaint_after(until - now);
                MeasureState::Active { until }
            }
            MeasureState::Idle {
                active_end,
                sampler,
            } => match sampler.try_recv() {
                Ok((idle, elapsed)) => {
                    let idle_cpu = match (active_end, idle) {
                        (Some(a), Some(b)) => Some(
                            b.cpu_time.saturating_sub(a.cpu_time).as_secs_f64()
                                / elapsed.as_secs_f64(),
                        ),
                        _ => None,
                    };
                    self.finish_measure(ctx, active_end, idle, idle_cpu);
                    MeasureState::Done
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => MeasureState::Idle {
                    active_end,
                    sampler,
                },
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    diag::print_err("kgraf-app: --measure: idle sampler thread died\n");
                    self.finish_measure(ctx, active_end, None, None);
                    MeasureState::Done
                }
            },
            MeasureState::Done => MeasureState::Done,
        };
    }

    fn finish_measure(
        &mut self,
        ctx: &egui::Context,
        active_end: Option<ResourceSample>,
        idle: Option<ResourceSample>,
        idle_cpu_fraction: Option<f64>,
    ) {
        diag::print_out(&format!(
            "{}\n",
            self.report(active_end, idle, idle_cpu_fraction)
        ));
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Plain-text measurement report, one `key: value` per line.
    fn report(
        &self,
        active_end: Option<ResourceSample>,
        idle: Option<ResourceSample>,
        idle_cpu_fraction: Option<f64>,
    ) -> String {
        let ms = |d: Option<Duration>| {
            d.map_or("n/a".to_owned(), |d| {
                format!("{:.1}", d.as_secs_f64() * 1000.0)
            })
        };
        let na = || "n/a".to_owned();
        let drops = self.drops.snapshot();
        let mut lines = vec![
            format!("adapter: {}", self.render.facts.summary()),
            format!("render_path: {:?}", self.render.path),
            format!(
                "display_refresh_hz: {}",
                self.refresh_mhz
                    .map_or_else(na, |m| format!("{:.2}", f64::from(m) / 1000.0))
            ),
            format!("continuous: {}", self.measure.continuous),
            format!("first_ui_pass_ms: {}", ms(self.clock.first_ui_start())),
            format!(
                "first_frame_presented_ms: {}",
                ms(self.clock.first_frame_presented())
            ),
            format!("presented_frames: {}", self.clock.presented()),
            format!(
                "dropped_frames_settled: {} ({} before the first presented frame)",
                self.clock.dropped(),
                self.clock.dropped_before_first()
            ),
            format!(
                "dropped_frames: {} (timeout {}, occluded {}, outdated {}, lost {}, other {})",
                drops.total(),
                drops.timeout,
                drops.occluded,
                drops.outdated,
                drops.lost,
                drops.other,
            ),
            format!("extra_passes: {}", self.clock.extra_passes()),
            // UI frames after the first presented frame, split by whether the
            // frame's input carried user input (pointer, key, text, scroll): the
            // frames without input are the app's own.
            format!(
                "ui_frames_with_input (after the first frame): {}",
                self.frames_with_input
            ),
            format!(
                "ui_frames_without_input (after the first frame): {}",
                self.frames_without_input
            ),
            format!(
                "ui_frames_pointer_inside (after the first frame): {}",
                self.frames_pointer_inside
            ),
        ];
        lines.push(match self.clock.interval_summary() {
            Some(s) => format!(
                "frame_interval_ms (presented only): mean {:.2} p50 {:.2} p95 {:.2} max {:.2} (n={})",
                s.mean_ms, s.p50_ms, s.p95_ms, s.max_ms, s.count
            ),
            None => "frame_interval_ms (presented only): n/a".to_owned(),
        });
        lines.push(match self.clock.cpu_summary() {
            Some(s) => format!(
                "frame_cpu_ms (presented only): mean {:.3} p50 {:.3} p95 {:.3} max {:.3} (n={})",
                s.mean_ms, s.p50_ms, s.p95_ms, s.max_ms, s.count
            ),
            None => "frame_cpu_ms (presented only): n/a".to_owned(),
        });
        // Metal holds memory after rendering and reclaims it when idle (see
        // IDLE_RECLAIM_WAIT): active peak is the lifetime peak at the end of the active
        // phase; idle after reclaim is sampled before any further paint.
        lines.push(format!(
            "footprint_active_peak_mib: {}",
            active_end.map_or_else(na, |s| mib(s.lifetime_peak))
        ));
        lines.push(format!(
            "footprint_active_end_mib: {}",
            active_end.map_or_else(na, |s| mib(s.phys_footprint))
        ));
        lines.push(format!(
            "footprint_idle_after_reclaim_mib: {} (after {} s idle)",
            idle.map_or_else(na, |s| mib(s.phys_footprint)),
            IDLE_RECLAIM_WAIT.as_secs()
        ));
        lines.push(format!(
            "idle_cpu_percent: {}",
            idle_cpu_fraction.map_or_else(na, |f| format!("{:.2}", f * 100.0))
        ));
        lines.push(format!(
            "open_projects: {}",
            self.workspace.projects().len()
        ));
        match self.workspace.active().and_then(|p| p.galaxy.as_ref()) {
            Some(g) => {
                let t = g.timings();
                let scene = g.view().map(|v| v.galaxy().scene());
                lines.push(format!("galaxy_project: {}", g.root().display()));
                lines.push(format!(
                    "galaxy_scene: {}",
                    scene.map_or_else(
                        || g.unindexed().unwrap_or("not loaded").to_owned(),
                        |s| format!(
                            "{} nodes, {} edges",
                            s.live_node_count(),
                            s.live_edge_count()
                        )
                    )
                ));
                lines.push(format!("galaxy_snapshot_read_ms: {}", ms(t.snapshot_read)));
                lines.push(format!("galaxy_scene_build_ms: {}", ms(t.scene_build)));
            }
            None => lines.push("galaxy_project: none".to_owned()),
        }
        lines.join("\n")
    }
}

/// One receiver that answers when every one of `pending` has (with several
/// projects): `true` only if every run stopped in time. `None` when nothing is
/// pending.
fn join_all(pending: Vec<Receiver<bool>>) -> Option<Receiver<bool>> {
    if pending.is_empty() {
        return None;
    }
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new()
        .name("runs-shutdown".into())
        .spawn(move || {
            // Every receiver is waited on, even after one says "not in time".
            let mut all = true;
            for r in pending {
                all &= r.recv().unwrap_or(false);
            }
            let _ = tx.send(all);
        });
    // A thread that cannot start leaves `rx` disconnected, which quits at once.
    if let Err(e) = spawned {
        log::error!("could not wait for the runs to stop: {e}");
    }
    Some(rx)
}

/// The pure core of [`ShellApp::sync_parallelism`] (decision 0067): kept free of `ShellApp` and
/// `CapacityHandle` so the collapsing-maximum bug has
/// a direct, fast regression test, with no real background thread or
/// machine measurement involved.
///
/// **The one writer of the gate's limits.** The re-measure thread only
/// publishes its reading (`capacity_bg`'s module doc); this function writes
/// `max_agents` from the slider (the user's pinned value, or the measured
/// maximum while it follows) and the writer and execution limits from the
/// measurement. With two writers, a reading accepted by the thread overwrote
/// a pinned value, and this function, seeing its own value unchanged, never
/// wrote it back.
///
/// `computed` is `kgraf-capacity`'s own measured `(max_agents, max_writers,
/// exec_slots)` (`CapacityHandle::latest`), `None` before the first reading.
/// Its `max_agents` drives [`crate::settings::ParallelismSlider::update_max`]
/// **only**; `gate`'s own `limits().max_agents` (the *effective* limit
/// already in force) is never fed back into the slider's maximum. Feeding it
/// back is the bug this exists to prevent: a user who lowers the slider
/// would see the gate's next-reported effective limit as the new maximum,
/// collapsing the slider's own ceiling to whatever they last chose and
/// leaving no way to raise it again. Before the first reading, the writer
/// and execution limits already in the gate are kept.
fn apply_parallelism(
    slider: &mut crate::settings::ParallelismSlider,
    gate: &kgraf_orchestrator::MachineGate,
    computed: Option<(u32, u32, u32)>,
    applied: &mut Option<kgraf_core::ParallelismLimits>,
) {
    if let Some((max, _, _)) = computed {
        slider.update_max(max);
    }
    let current = gate.limits();
    let (writers, exec) = computed
        .map(|(_, w, e)| (w, e))
        .unwrap_or((current.max_writers, current.exec_slots));
    let want = kgraf_core::ParallelismLimits::derive(slider.effective(), writers, exec);
    if *applied == Some(want) && current == want {
        return;
    }
    gate.set_limits(want);
    *applied = Some(want);
}

/// Turns a tray command into viewport commands. Returns `true` when the app is quitting.
fn apply_tray_command(ctx: &egui::Context, cmd: TrayCommand) -> bool {
    match cmd {
        TrayCommand::Show => {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            false
        }
        TrayCommand::Quit => {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            true
        }
    }
}

/// Close-to-tray (0052): with background mode on and a tray present, a window close
/// is canceled and the window hidden, so the process stays alive for schedules.
/// Returns `true` when it intercepted the close.
fn close_to_tray(
    ctx: &egui::Context,
    quitting: bool,
    background_mode: bool,
    tray_present: bool,
) -> bool {
    let close_requested = ctx.input(|i| i.viewport().close_requested());
    let intercept = close_requested && !quitting && background_mode && tray_present;
    if intercept {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
    intercept
}

/// How long after the last input a person counts as at the window.
pub const ATTENTION_WINDOW: Duration = Duration::from_secs(60);

/// Whether an event is the user's own input (pointer, key, text, scroll).
pub fn is_user_input(e: &egui::Event) -> bool {
    matches!(
        e,
        egui::Event::PointerMoved(_)
            | egui::Event::PointerButton { .. }
            | egui::Event::MouseWheel { .. }
            | egui::Event::Key { .. }
            | egui::Event::Text(_)
            | egui::Event::Touch { .. }
            | egui::Event::Zoom(_)
    )
}

/// Until when a person counts as at the window: the last input plus
/// [`ATTENTION_WINDOW`], while the window is showing and focused. Pure.
pub fn attended_until(showing: bool, last_input: Option<Instant>) -> Option<Instant> {
    last_input
        .filter(|_| showing)
        .map(|at| at + ATTENTION_WINDOW)
}

/// How often a closing window checks whether its hosted run has stopped.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Quitting with a hosted run: the window hides at once and the process
/// exits when the run has stopped (or the stop wait timed out), without blocking
/// the UI thread. A close that was not intercepted to the tray asks `start` for a
/// shutdown receiver; while one is pending, further closes are cancelled and the
/// frame polls it. Returns `true` when the app should quit now.
fn quit_step(
    ctx: &egui::Context,
    intercepted: bool,
    pending: &mut Option<Receiver<bool>>,
    start: impl FnOnce() -> Option<Receiver<bool>>,
) -> bool {
    let close_requested = ctx.input(|i| i.viewport().close_requested());
    if let Some(rx) = pending {
        if close_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        return match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(SHUTDOWN_POLL);
                false
            }
            // Stopped, timed out, or the thread is gone: quit.
            _ => {
                *pending = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                true
            }
        };
    }
    if close_requested
        && !intercepted
        && let Some(rx) = start()
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        *pending = Some(rx);
        ctx.request_repaint_after(SHUTDOWN_POLL);
    }
    false
}

impl eframe::App for ShellApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.logic_pass(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.ui_pass(ui, frame);
    }
}

impl ShellApp {
    /// Everything that runs on every wake, shown or not. Apart from
    /// `eframe::App::logic` so a test can drive it without a `Frame`.
    fn logic_pass(&mut self, ctx: &egui::Context) {
        self.handle_tray(ctx);
        self.handle_quit_shortcut(ctx);
        self.handle_close(ctx);
        self.handle_measure(ctx);
        // In `logic`, which runs on every wake: a covered window gets no `ui`
        // pass, and the capture must still time out and say so.
        self.handle_screenshot(ctx, false);
        // The gate writers run here and not in `ui`: a hidden or covered
        // window gets no `ui` pass, and the re-measure thread's readings and
        // the slider's pin must still reach the gates.
        self.sync_gates();
        // No timer repaint: the scheduler wakes the UI when its board
        // changes, and a hidden window still runs `logic` on that wake.
        self.refresh_tray_schedules();
        // A scheduled run's ask reaches the box only while a person is at the
        // window: visible, focused, and with input in the last
        // `ATTENTION_WINDOW`; otherwise it is answered unattended. A person
        // at the window attends every open project's box: the bell and the
        // project dots route them to it.
        let now = Instant::now();
        let (showing, input) = ctx.input(|i| {
            let v = i.viewport();
            let showing = v.focused.unwrap_or(false)
                && !v.occluded.unwrap_or(false)
                && !v.minimized.unwrap_or(false);
            (showing, i.events.iter().any(is_user_input))
        });
        if input {
            self.last_input = Some(now);
        }
        let until = attended_until(showing, self.last_input);
        for p in self.workspace.projects() {
            if let Some(i) = p.interaction.as_ref() {
                i.set_attended_until(until);
            }
        }
    }

    fn ui_pass(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // egui may run several passes per frame (a discarded sizing pass), each calling
        // `ui`. Only the first pass of a frame is a frame; later ones are counted apart
        // so they do not show up as near-zero frame intervals.
        if ui.ctx().current_pass_index() == 0 {
            // Counted after the first presented frame: before it, the startup
            // loop asks for frames until one reaches the screen.
            if self.clock.first_frame_presented().is_some() {
                if ui.input(|i| i.events.iter().any(is_user_input)) {
                    self.frames_with_input += 1;
                } else {
                    self.frames_without_input += 1;
                }
                if ui.input(|i| i.pointer.hover_pos().is_some()) {
                    self.frames_pointer_inside += 1;
                }
            }
            self.clock.frame_start(
                Instant::now(),
                frame.info().cpu_usage,
                self.drops.snapshot().total(),
            );
        } else {
            self.clock.extra_pass();
        }
        if self.refresh_mhz.is_none() {
            self.refresh_mhz = frame
                .winit_window()
                .and_then(|w| w.current_monitor())
                .and_then(|m| m.refresh_rate_millihertz());
        }
        let wanted = (self.settings.appearance, self.settings.color_vision);
        if self.applied_appearance != Some(wanted) {
            if self.applied_appearance.is_none() {
                crate::theme::install(ui.ctx(), wanted.0, wanted.1);
            } else {
                crate::theme::apply_appearance(ui.ctx(), wanted.0, wanted.1);
            }
            self.applied_appearance = Some(wanted);
        }
        // The capture arrives as an input event of a `ui` pass; `logic` does the
        // waiting and the timeout.
        self.handle_screenshot(ui.ctx(), true);
        self.frame_ui(ui, frame);
        let occluded = ui.input(|i| i.viewport().occluded.unwrap_or(false));
        let measure_idle = matches!(
            self.measure_state,
            MeasureState::Idle { .. } | MeasureState::Done
        );
        // Until a frame is confirmed presented, keep asking: the next frame is what
        // confirms it (see FrameClock).
        match repaint_request(
            self.measure.continuous,
            self.clock.first_frame_presented().is_some(),
            occluded,
            measure_idle,
            self.clock.previous_frame_was_dropped(),
        ) {
            RepaintRequest::None => {}
            RepaintRequest::Now => ui.ctx().request_repaint(),
            RepaintRequest::After(d) => ui.ctx().request_repaint_after(d),
        }
        // Save only once the pointer is released, so a slider drag writes one file.
        if !ui.input(|i| i.pointer.any_down()) {
            self.store.persist_if_changed(&self.settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ParallelismSlider;
    use egui::{RawInput, ViewportCommand, ViewportEvent, ViewportId, ViewportInfo};
    use kgraf_orchestrator::MachineGate;

    /// A question becomes a folder-name-safe slug.
    #[test]
    fn a_question_becomes_a_bounded_lower_case_slug() {
        assert_eq!(project_slug("Design a Mobile App!"), "design-a-mobile-app");
        assert_eq!(
            project_slug("   "),
            "question",
            "nothing alphanumeric survives"
        );
        let long = "a".repeat(100);
        assert!(project_slug(&long).len() <= 40);
    }

    /// `first_free_name` never collides with a folder that already exists,
    /// and returns the slug itself when nothing does yet.
    #[test]
    fn first_free_name_never_collides_with_an_existing_folder() {
        let dir = tempfile::tempdir().expect("dir");
        let base = dir.path();
        assert_eq!(
            first_free_name(base, "a-question").as_deref(),
            Some("a-question")
        );
        std::fs::create_dir_all(base.join("a-question")).expect("mkdir");
        assert_eq!(
            first_free_name(base, "a-question").as_deref(),
            Some("a-question-1"),
            "the bare slug is taken, so the next free one is used"
        );
        std::fs::create_dir_all(base.join("a-question-1")).expect("mkdir");
        assert_eq!(
            first_free_name(base, "a-question").as_deref(),
            Some("a-question-2")
        );
    }

    /// The slider's maximum must never come
    /// from the gate's own effective limit. Machine maximum 16, the user
    /// pins the slider at 3 (the gate's effective limit becomes 3), several
    /// frames pass (each a call to `apply_parallelism` with the same
    /// `computed_max`): the slider's own maximum must still read 16, and
    /// raising it to 10 must still work.
    #[test]
    fn lowering_the_slider_never_collapses_its_own_maximum() {
        let mut slider = ParallelismSlider::new(16);
        let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(16, 16, 16));
        let mut applied = None;

        apply_parallelism(&mut slider, &gate, Some((16, 16, 16)), &mut applied);
        assert_eq!(gate.limits().max_agents, 16);
        assert_eq!(slider.max(), 16);

        slider.set_by_user(3);
        apply_parallelism(&mut slider, &gate, Some((16, 16, 16)), &mut applied);
        assert_eq!(
            gate.limits().max_agents,
            3,
            "the gate's effective limit follows the user's own choice"
        );

        // Several more frames: the bug this test guards against fed the
        // gate's now-3 effective limit back into `update_max`, collapsing
        // the slider's maximum to 3 here.
        for _ in 0..5 {
            apply_parallelism(&mut slider, &gate, Some((16, 16, 16)), &mut applied);
        }
        assert_eq!(
            slider.max(),
            16,
            "the slider's maximum stays the computed capacity, not the gate's clamped effective limit"
        );

        slider.set_by_user(10);
        apply_parallelism(&mut slider, &gate, Some((16, 16, 16)), &mut applied);
        assert_eq!(
            gate.limits().max_agents,
            10,
            "raising the slider back up still works"
        );
        assert_eq!(slider.max(), 16);
    }

    /// A rising computed maximum (a machine or route measurement improving)
    /// still reaches a *following* slider (never pinned): `effective()`
    /// tracks it, and the gate is pushed to match.
    #[test]
    fn a_rising_computed_maximum_reaches_a_following_slider() {
        let mut slider = ParallelismSlider::new(4);
        let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(4, 4, 4));
        let mut applied = None;
        apply_parallelism(&mut slider, &gate, Some((4, 4, 4)), &mut applied);
        assert_eq!(gate.limits().max_agents, 4);

        apply_parallelism(&mut slider, &gate, Some((20, 4, 4)), &mut applied);
        assert_eq!(slider.max(), 20);
        assert_eq!(
            gate.limits().max_agents,
            20,
            "a following slider pushes the new maximum through"
        );
    }

    /// A pinned slider of 3, and the re-measure thread accepts a reading of
    /// 12: after one frame the gate reads 3, not 12, and the writer and
    /// execution limits come from the reading. The thread holds no gate, so
    /// the frame is the only writer.
    #[test]
    fn a_pinned_slider_wins_over_a_new_measurement_after_one_frame() {
        let mut slider = ParallelismSlider::new(8);
        let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(8, 8, 8));
        let mut applied = None;
        apply_parallelism(&mut slider, &gate, Some((8, 8, 8)), &mut applied);
        slider.set_by_user(3);
        apply_parallelism(&mut slider, &gate, Some((8, 8, 8)), &mut applied);
        assert_eq!(gate.limits().max_agents, 3);
        // The thread accepted 12 (writers 6, exec 5); one frame.
        apply_parallelism(&mut slider, &gate, Some((12, 6, 5)), &mut applied);
        let limits = gate.limits();
        assert_eq!(limits.max_agents, 3, "the pin holds");
        assert_eq!((limits.max_writers, limits.exec_slots), (6, 5));
        assert_eq!(slider.max(), 12, "the measurement is the slider's maximum");
        // Something else wrote the gate (it must not, but if it did): the
        // next frame puts the pin back.
        gate.set_limits(kgraf_core::ParallelismLimits::derive(12, 6, 5));
        apply_parallelism(&mut slider, &gate, Some((12, 6, 5)), &mut applied);
        assert_eq!(gate.limits().max_agents, 3, "the frame restores the pin");
    }

    /// Runs one logic pass and returns the root viewport commands egui hands to the
    /// windowing layer. This exercises egui's real command path, not a restatement of
    /// our own flags. The OS-level effect (window hidden, process alive) was
    /// not automated; see the crate report.
    fn root_commands(
        close_requested: bool,
        f: impl FnOnce(&egui::Context),
    ) -> Vec<ViewportCommand> {
        let ctx = egui::Context::default();
        let mut input = RawInput::default();
        let mut info = ViewportInfo::default();
        if close_requested {
            info.events.push(ViewportEvent::Close);
        }
        input.viewports.insert(ViewportId::ROOT, info);
        let out = ctx.run_logic(&input, f);
        out.viewport_commands
            .get(&ViewportId::ROOT)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn close_with_background_mode_and_tray_hides_instead_of_quitting() {
        let mut intercepted = false;
        let cmds = root_commands(true, |ctx| {
            intercepted = close_to_tray(ctx, false, true, true)
        });
        assert!(intercepted);
        assert!(cmds.contains(&ViewportCommand::CancelClose), "{cmds:?}");
        assert!(cmds.contains(&ViewportCommand::Visible(false)), "{cmds:?}");
    }

    /// A person counts as at the window only while it is showing and focused
    /// and for `ATTENTION_WINDOW` after their last input; a repaint or a timer
    /// is not input.
    #[test]
    fn attendance_needs_a_showing_window_and_recent_input() {
        let now = Instant::now();
        assert_eq!(attended_until(true, None), None, "no input yet: nobody");
        assert_eq!(attended_until(false, Some(now)), None, "hidden: nobody");
        assert_eq!(
            attended_until(true, Some(now)),
            Some(now + ATTENTION_WINDOW)
        );
        assert!(is_user_input(&egui::Event::Text("a".into())));
        assert!(is_user_input(&egui::Event::PointerMoved(egui::pos2(
            1.0, 1.0
        ))));
        assert!(!is_user_input(&egui::Event::WindowFocused(true)));
        assert!(is_user_input(&egui::Event::Zoom(1.1)), "a pinch is input");
    }

    /// Quitting with a hosted run hides the window at once and quits only when
    /// the run's stop answers; nothing blocks the frame.
    #[test]
    fn quitting_with_a_run_hides_at_once_and_quits_when_it_has_stopped() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut pending = None;
        let mut started = false;
        let cmds = root_commands(true, |ctx| {
            assert!(!quit_step(ctx, false, &mut pending, || {
                started = true;
                Some(rx)
            }));
        });
        assert!(started);
        assert!(cmds.contains(&ViewportCommand::CancelClose), "{cmds:?}");
        assert!(cmds.contains(&ViewportCommand::Visible(false)), "{cmds:?}");
        assert!(pending.is_some());
        // Still stopping: no quit, no close.
        let cmds = root_commands(false, |ctx| {
            assert!(!quit_step(ctx, false, &mut pending, || None));
        });
        assert!(!cmds.contains(&ViewportCommand::Close), "{cmds:?}");
        tx.send(true).expect("send");
        let cmds = root_commands(false, |ctx| {
            assert!(quit_step(ctx, false, &mut pending, || None));
        });
        assert!(cmds.contains(&ViewportCommand::Close), "{cmds:?}");
        assert!(pending.is_none());
        // With no run hosted, a close is an ordinary close.
        let cmds = root_commands(true, |ctx| {
            assert!(!quit_step(ctx, false, &mut pending, || None));
        });
        assert!(!cmds.contains(&ViewportCommand::CancelClose), "{cmds:?}");
    }

    #[test]
    fn close_without_tray_or_background_mode_is_a_real_close() {
        for (bg, tray) in [(false, true), (true, false), (false, false)] {
            let cmds = root_commands(true, |ctx| assert!(!close_to_tray(ctx, false, bg, tray)));
            assert!(!cmds.contains(&ViewportCommand::CancelClose), "{cmds:?}");
        }
    }

    #[test]
    fn quit_from_tray_is_not_intercepted() {
        let cmds = root_commands(true, |ctx| assert!(!close_to_tray(ctx, true, true, true)));
        assert!(!cmds.contains(&ViewportCommand::CancelClose), "{cmds:?}");
    }

    #[test]
    fn no_close_request_sends_nothing() {
        let cmds = root_commands(false, |ctx| assert!(!close_to_tray(ctx, false, true, true)));
        assert!(cmds.is_empty(), "{cmds:?}");
    }

    #[test]
    fn tray_show_reopens_and_quit_closes() {
        let cmds = root_commands(false, |ctx| {
            assert!(!apply_tray_command(ctx, TrayCommand::Show))
        });
        assert!(cmds.contains(&ViewportCommand::Visible(true)), "{cmds:?}");
        assert!(cmds.contains(&ViewportCommand::Focus), "{cmds:?}");

        let cmds = root_commands(false, |ctx| {
            assert!(apply_tray_command(ctx, TrayCommand::Quit))
        });
        assert!(cmds.contains(&ViewportCommand::Close), "{cmds:?}");
    }

    #[test]
    fn repaint_policy() {
        use RepaintRequest::{After, Now};
        let r = repaint_request;
        // Continuous mode repaints unless covered.
        assert_eq!(r(true, true, false, false, false), Now);
        assert_eq!(
            r(true, true, true, false, false),
            RepaintRequest::None,
            "occluded"
        );
        // Before the first presented frame, ask for the confirming frame.
        assert_eq!(r(false, false, false, false, false), Now);
        assert_eq!(r(false, false, true, false, false), RepaintRequest::None);
        // After a drop, retry later instead of spinning.
        assert_eq!(
            r(false, false, false, false, true),
            After(DROPPED_FRAME_RETRY)
        );
        assert_eq!(
            r(true, true, false, false, true),
            After(DROPPED_FRAME_RETRY)
        );
        // Idle after the first frame: no repaints.
        assert_eq!(r(false, true, false, false, false), RepaintRequest::None);
        // The measurement's idle phase overrides continuous mode.
        assert_eq!(r(true, true, false, true, false), RepaintRequest::None);
    }

    /// Drives one real egui pass with continuous mode, covered and not, and reads
    /// the repaint delay egui hands back to the integration.
    #[test]
    fn occluded_window_requests_no_repaint_through_egui() {
        let run = |occluded: bool| {
            let ctx = egui::Context::default();
            let input = || {
                let mut input = RawInput::default();
                let info = ViewportInfo {
                    occluded: Some(occluded),
                    ..Default::default()
                };
                input.viewports.insert(ViewportId::ROOT, info);
                input
            };
            let pass = |input| {
                let mut out = ctx.run_ui(input, |ui| {
                    let occ = ui.input(|i| i.viewport().occluded.unwrap_or(false));
                    if repaint_request(true, true, occ, false, false) == RepaintRequest::Now {
                        ui.ctx().request_repaint();
                    }
                });
                // The font atlas upload is not applied in a test; egui asserts on that.
                out.textures_delta.clear();
                out.viewport_output
                    .get(&ViewportId::ROOT)
                    .map(|v| v.repaint_delay)
            };
            // egui asks for a second frame after its first pass on its own; measure a
            // steady-state pass.
            let _ = pass(input());
            let _ = pass(input());
            pass(input())
        };
        assert_eq!(run(false), Some(Duration::ZERO), "visible: repaint now");
        assert_ne!(
            run(true),
            Some(Duration::ZERO),
            "occluded: no immediate repaint"
        );
    }

    fn input_with_key(key: egui::Key, modifiers: egui::Modifiers) -> RawInput {
        let mut input = RawInput::default();
        input.events.push(egui::Event::ModifiersChanged(modifiers));
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        });
        input
    }

    #[test]
    fn command_q_is_quit_and_plain_q_is_not() {
        let check = |input: RawInput| {
            let ctx = egui::Context::default();
            let mut pressed = false;
            let mut out = ctx.run_ui(input, |ui| pressed = ui.input(quit_shortcut_pressed));
            out.textures_delta.clear();
            pressed
        };
        assert!(check(input_with_key(
            egui::Key::Q,
            egui::Modifiers::COMMAND
        )));
        assert!(!check(input_with_key(egui::Key::Q, egui::Modifiers::NONE)));
        assert!(!check(input_with_key(
            egui::Key::W,
            egui::Modifiers::COMMAND
        )));
    }

    /// A color vision chosen in one settings write
    /// round-trips through `persist_if_changed` and a fresh load, the same
    /// path `SettingsStore` takes on a real save, not just `Settings::save`
    /// directly.
    #[test]
    fn a_persisted_color_vision_survives_a_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let base = Settings::default();
        base.save(&path).expect("save");
        let mut store = SettingsStore::new(path.clone(), base.clone());

        let mut chosen = Settings {
            color_vision: kgraf_core::palette::Vision::Protanopia,
            ..base.clone()
        };
        store.persist_if_changed(&chosen);
        assert!(store.last_error.is_none(), "{:?}", store.last_error);
        assert_eq!(
            Settings::load(&path).expect("load").color_vision,
            kgraf_core::palette::Vision::Protanopia
        );

        // An unrelated change afterward keeps the mode.
        chosen.reduce_motion = !chosen.reduce_motion;
        store.persist_if_changed(&chosen);
        assert_eq!(
            Settings::load(&path).expect("load").color_vision,
            kgraf_core::palette::Vision::Protanopia
        );
    }

    /// An unrelated change in
    /// one window never drops the other window's project, and a close is kept.
    #[test]
    fn two_windows_keep_each_others_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let p = |s: &str| PathBuf::from(s);
        let base = Settings {
            projects: vec![p("/a")],
            ..Settings::default()
        };
        base.save(&path).expect("save");
        let disk = || Settings::load(&path).expect("load").projects;
        let mut one = SettingsStore::new(path.clone(), base.clone());
        let mut two = SettingsStore::new(path.clone(), base.clone());
        // Round 1: window one opens /b, window two opens /c.
        let mut s1 = Settings {
            projects: vec![p("/a"), p("/b")],
            ..base.clone()
        };
        one.persist_if_changed(&s1);
        let mut s2 = Settings {
            projects: vec![p("/a"), p("/c")],
            ..base.clone()
        };
        two.persist_if_changed(&s2);
        assert_eq!(disk(), [p("/a"), p("/b"), p("/c")]);
        // Round 2: window two toggles the guide; /b stays.
        s2.guide_open = !s2.guide_open;
        two.persist_if_changed(&s2);
        assert_eq!(disk(), [p("/a"), p("/b"), p("/c")], "/b kept");
        // Round 3: window one changes the theme; /c stays.
        s1.reduce_motion = !s1.reduce_motion;
        one.persist_if_changed(&s1);
        assert_eq!(disk(), [p("/a"), p("/b"), p("/c")], "/c kept");
        // Round 4: window one closes /a; the close is kept, and window two's
        // unrelated write does not bring it back.
        s1.projects = vec![p("/b")];
        one.persist_if_changed(&s1);
        assert_eq!(disk(), [p("/b"), p("/c")]);
        s2.guide_open = !s2.guide_open;
        two.persist_if_changed(&s2);
        assert_eq!(disk(), [p("/b"), p("/c")]);
        assert!(
            std::fs::read_dir(dir.path())
                .expect("dir")
                .filter_map(Result::ok)
                .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")),
            "no temporary file left"
        );
    }

    /// Final check item 2: a write that fails does not advance this window's
    /// own list, so the project it added is written by the next write (on a
    /// new change or at the retry), and another window's project stays. The
    /// failure is real: the folder is made read-only, so the temporary file
    /// cannot be created.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_keeps_the_added_project_for_the_retry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("cfg");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let path = sub.join("settings.toml");
        let p = |s: &str| PathBuf::from(s);
        let base = Settings {
            projects: vec![p("/a")],
            ..Settings::default()
        };
        base.save(&path).expect("save");
        let disk = || Settings::load(&path).expect("load").projects;
        let mut one = SettingsStore::new(path.clone(), base.clone());
        // The lock file exists before the folder turns read-only.
        drop(one.lock().expect("lock"));
        let mode = |m: u32| {
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(m)).expect("chmod")
        };
        mode(0o555);
        let mut s1 = Settings {
            projects: vec![p("/a"), p("/b")],
            ..base.clone()
        };
        one.persist_if_changed(&s1);
        mode(0o755);
        assert!(one.last_error.is_some(), "the write failed");
        assert_eq!(disk(), [p("/a")]);
        // Meanwhile another window adds /c.
        let mut two = SettingsStore::new(path.clone(), base.clone());
        two.persist_if_changed(&Settings {
            projects: vec![p("/a"), p("/c")],
            ..base.clone()
        });
        // Nothing changed in window one yet: no write before the retry time.
        one.persist_if_changed(&s1);
        assert_eq!(disk(), [p("/a"), p("/c")]);
        // At the retry, /b lands and /c stays.
        one.retry_at = Some(Instant::now());
        one.persist_if_changed(&s1);
        assert!(one.last_error.is_none());
        assert_eq!(disk(), [p("/a"), p("/c"), p("/b")]);
        // A later unrelated change keeps both.
        s1.guide_open = !s1.guide_open;
        one.persist_if_changed(&s1);
        assert_eq!(disk(), [p("/a"), p("/c"), p("/b")]);
    }

    #[test]
    fn settings_store_writes_only_on_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let mut store = SettingsStore::new(path.clone(), Settings::default());
        let mut s = Settings::default();
        store.persist_if_changed(&s);
        assert!(!path.exists(), "unchanged settings are not written");

        s.compaction
            .set_by_user(crate::settings::CompactionPoint::new(42).expect("valid"));
        store.persist_if_changed(&s);
        assert_eq!(Settings::load(&path).expect("load"), s);
        assert!(store.last_error.is_none());
    }

    #[test]
    fn indicator_marks_cpu_rendering() {
        let facts = AdapterFacts {
            name: "llvmpipe (LLVM 17.0.6, 256 bits)".to_owned(),
            driver: "llvmpipe".to_owned(),
            driver_info: String::new(),
            backend: "vulkan".to_owned(),
            device_kind: crate::adapter::DeviceKind::Cpu,
            surface_compatible: true,
        };
        let status = RenderStatus {
            path: classify(&facts),
            facts,
        };
        let label = status.label();
        assert!(label.starts_with("CPU rendering: llvmpipe"), "{label}");
    }

    /// A shell with no window, no tray and no thread, holding a fixed
    /// capacity reading.
    fn headless_app(dir: &std::path::Path, capacity: kgraf_capacity::Capacity) -> ShellApp {
        let settings = Settings::default();
        ShellApp::new(AppParts {
            store: SettingsStore::new(dir.join("settings.json"), settings.clone()),
            settings,
            render: RenderStatus {
                facts: AdapterFacts {
                    name: "test".into(),
                    driver: String::new(),
                    driver_info: String::new(),
                    backend: "test".into(),
                    device_kind: crate::adapter::DeviceKind::Cpu,
                    surface_compatible: true,
                },
                path: RenderPath::Hardware,
            },
            tray: None,
            clock: FrameClock::new(Instant::now()),
            drops: Arc::new(DropCounters::default()),
            measure: MeasureOptions::default(),
            workspace: Workspace::default(),
            opener: None,
            screenshot: None,
            machine_gate: MachineGate::new(kgraf_core::ParallelismLimits::derive(1, 1, 1)),
            capacity: Some(crate::capacity_bg::CapacityHandle::fixed(capacity)),
            update: None,
            permissions: None,
        })
    }

    /// The gate writers run in `logic`, which a hidden or covered window still
    /// gets, and not in `ui`: one `logic` pass with a fresh capacity reading
    /// moves the process-wide gate, with no `ui` pass at all.
    #[test]
    fn a_logic_pass_alone_applies_the_capacity_reading_to_the_gate() {
        let dir = tempfile::tempdir().expect("dir");
        let mut app = headless_app(
            dir.path(),
            kgraf_capacity::Capacity {
                max_agents: 12,
                max_writers: 6,
                exec_slots: 5,
                bounds: Vec::new(),
            },
        );
        let before = app.machine_gate().limits();
        assert_eq!(before.max_writers, 1);
        let ctx = egui::Context::default();
        let _ = ctx.run_logic(&RawInput::default(), |ctx| app.logic_pass(ctx));
        let after = app.machine_gate().limits();
        assert_eq!(after.max_writers, 6, "the writer bound was applied");
        assert_eq!(after.exec_slots, 5, "the exec bound was applied");
    }
}
