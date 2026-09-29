//! The project switcher (decision 0066): several projects open in one window, each
//! with its own host, scheduler, graph and runs. Switching changes which one is
//! shown; runs in the others keep going.
//!
//! The switcher is navigation, not a work surface. Each project is
//! one row in the guide: a letter tile, its name, one line of state and a status
//! dot. The dot and the collapsed hamburger's color are derived here from real
//! state, never stored:
//!
//! - **needs input** (yellow, COL-3): approvals pending on the project's desk;
//!   questions to the user (the runtime has no `ask_user_question` yet, so that
//!   count is zero until it does); an unfinished run that is blocked on the user or
//!   holds a gate or contract for them; and failures (a failure needs
//!   input, so it falls under yellow): a gate of an unfinished run that
//!   failed, the last run hosted here that failed, run records that cannot be
//!   read, or a project that cannot be shown;
//! - **working** (blinking gray): a run hosted in the window is starting or
//!   running, the project's admission has work in flight, or a live process holds
//!   one of its run leases;
//! - **new results** (cyan): the project's results (verified contracts and
//!   completed runs, from its run records) grew since the user last viewed it;
//! - **idle**: none of those, and no dot.
//!
//! **One priority everywhere: yellow, then cyan, then gray** (CMP-BDG-3 and CMP-BDG-4; decision
//! 0066). The dot has no orange state: orange stays on the failed item itself (its text, its node).
//! One pure function, [`dot_status`], decides both a project row's dot and the collapsed hamburger,
//! which applies it to every project's flags combined ([`any_flags`], [`hamburger_status`]); a
//! change of order is a change there only. A project that works and has new results is cyan in its
//! row and in the hamburger.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kgraf_galaxy::ProjectView;
use kgraf_orchestrator::{RunStatus, StoredRun};

use crate::activity::{ActivityRow, ActivityUpdate, ActivityWatch};
use crate::interaction::Interaction;
use crate::orchestrator_box::{BackendState, BoxBackend, Line};
use crate::orchestrator_host::HostParts;
use crate::presume_bg::PresumeWatcher;
use crate::scheduler_bg::SchedulerHandle;
use crate::theme::Indicator;

/// A project's state as a dot color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectStatus {
    #[default]
    Idle,
    Working,
    NewResults,
    NeedsInput,
}

impl ProjectStatus {
    /// The dot's indicator color, `None` for no dot.
    pub fn indicator(self) -> Option<Indicator> {
        match self {
            Self::Idle => None,
            Self::NewResults => Some(Indicator::Selection),
            Self::Working => Some(Indicator::Neutral),
            Self::NeedsInput => Some(Indicator::NeedsInput),
        }
    }

    /// Whether the dot blinks (working only).
    pub fn blinks(self) -> bool {
        self == Self::Working
    }

    /// The words, for tooltips and accessibility.
    pub fn words(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::NewResults => "new results since you last looked",
            Self::Working => "working",
            Self::NeedsInput => "needs your input",
        }
    }
}

/// What a project's state is derived from. Every field is read from live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Signals {
    /// Tool approvals pending on the project's desk.
    pub approvals: usize,
    /// Questions to the user pending (none exist in the runtime yet).
    pub questions: usize,
    /// Runs blocked on the user, and gates or contracts held for them.
    pub held: usize,
    /// Failures: failed gates of unfinished runs, a failed last run, unreadable
    /// run records, a project that cannot be shown.
    pub failures: usize,
    /// Something runs for the project (see the module doc).
    pub working: bool,
    /// Verified contracts plus completed runs, over its run records.
    pub results: u64,
}

impl Signals {
    /// The items that need the user: the bell counts these. Pure.
    pub fn needs(&self) -> usize {
        self.approvals + self.questions + self.held + self.failures
    }

    /// The kinds in words, for tooltips: "1 failed run, 2 questions". Pure.
    pub fn needs_words(&self) -> String {
        let kind = |n: usize, one: &str, many: &str| match n {
            0 => None,
            1 => Some(format!("1 {one}")),
            n => Some(format!("{n} {many}")),
        };
        [
            kind(self.failures, "failure", "failures"),
            kind(self.approvals, "approval", "approvals"),
            kind(self.questions, "question", "questions"),
            kind(self.held, "held run", "held runs"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// A project's three independent flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    pub needs_input: bool,
    pub working: bool,
    pub new_results: bool,
}

impl Flags {
    /// The flags from the signals and the results seen at the last view. Pure.
    pub fn of(s: &Signals, seen_results: u64) -> Self {
        Self {
            needs_input: s.needs() > 0,
            working: s.working,
            new_results: s.results > seen_results,
        }
    }
}

/// The one priority (CMP-BDG-3): yellow (needs input, failures included), else
/// cyan (new results), else gray (working), else no dot. A project row's dot is
/// this of the project's flags; the collapsed hamburger is this of every
/// project's flags combined. Pure.
pub fn dot_status(f: Flags) -> ProjectStatus {
    if f.needs_input {
        ProjectStatus::NeedsInput
    } else if f.new_results {
        ProjectStatus::NewResults
    } else if f.working {
        ProjectStatus::Working
    } else {
        ProjectStatus::Idle
    }
}

/// The flags of several projects combined: each is set when any project's is.
/// Pure.
pub fn any_flags(projects: impl IntoIterator<Item = Flags>) -> Flags {
    projects.into_iter().fold(Flags::default(), |a, f| Flags {
        needs_input: a.needs_input || f.needs_input,
        working: a.working || f.working,
        new_results: a.new_results || f.new_results,
    })
}

/// The collapsed hamburger's state: [`dot_status`] of every project's flags
/// combined. Pure.
pub fn hamburger_status(projects: impl IntoIterator<Item = Flags>) -> ProjectStatus {
    dot_status(any_flags(projects))
}

/// `names` as English: "A", "A and B", "A, B and C". Pure.
pub fn name_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The collapsed hamburger's tooltip (CMP-BDG-4): the projects in the aggregate's
/// state, by name, with the kinds that need input: "Show projects. Flume: 1
/// failed run; A and B are working". `projects` is (name, flags, signals). Pure.
pub fn hamburger_words(projects: &[(String, Flags, Signals)]) -> String {
    let status = hamburger_status(projects.iter().map(|(_, f, _)| *f));
    let names = |keep: &dyn Fn(&Flags) -> bool| {
        projects
            .iter()
            .filter(|(_, f, _)| keep(f))
            .map(|(n, _, _)| n.clone())
            .collect::<Vec<_>>()
    };
    let plural = |n: &[String], one: &str, many: &str| {
        format!("{} {}", name_list(n), if n.len() == 1 { one } else { many })
    };
    let what = match status {
        ProjectStatus::Idle => return "Show projects".to_owned(),
        ProjectStatus::NeedsInput => projects
            .iter()
            .filter(|(_, f, _)| f.needs_input)
            .map(|(n, _, s)| format!("{n}: {}", s.needs_words()))
            .collect::<Vec<_>>()
            .join("; "),
        ProjectStatus::NewResults => plural(
            &names(&|f| f.new_results),
            "has new results",
            "have new results",
        ),
        ProjectStatus::Working => plural(&names(&|f| f.working), "is working", "are working"),
    };
    format!("Show projects. {what}")
}
/// How the hamburger icon looks this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HamburgerLook {
    /// The icon's color meaning; `None` is the plain text color.
    pub indicator: Option<Indicator>,
    pub blink: bool,
    pub wiggle: bool,
}

/// The hamburger's look. With the sidebar open the icon is static and plain (the
/// rows show each project's dot). Only needs-input wiggles, and not while the
/// pointer is on it or under reduced motion; only working blinks. Pure.
pub fn hamburger_look(
    aggregate: ProjectStatus,
    guide_open: bool,
    hovered: bool,
    reduce_motion: bool,
) -> HamburgerLook {
    if guide_open {
        return HamburgerLook {
            indicator: None,
            blink: false,
            wiggle: false,
        };
    }
    HamburgerLook {
        indicator: aggregate.indicator(),
        blink: aggregate == ProjectStatus::Working && !reduce_motion,
        wiggle: aggregate == ProjectStatus::NeedsInput && !reduce_motion && !hovered,
    }
}

/// The wiggle's rotation in degrees at `t` seconds into its 4.6 s cycle: the
/// attention wiggle of MOT-4 (8, -6, 4 and -1 degrees over the first 600 ms,
/// then rest), linear between keyframes. Pure.
pub fn wiggle_degrees(t: f32) -> f32 {
    use crate::theme::motion::{WIGGLE_DEGREES, WIGGLE_PERIOD_S};
    let t = t.rem_euclid(WIGGLE_PERIOD_S) / WIGGLE_PERIOD_S;
    let s = WIGGLE_DEGREES / 8.0;
    let keys = [
        (0.0, 0.0),
        (0.03, 8.0 * s),
        (0.06, -6.0 * s),
        (0.09, 4.0 * s),
        (0.12, -s),
        (0.13, 0.0),
    ];
    for w in keys.windows(2) {
        let ((t0, a0), (t1, a1)) = (w[0], w[1]);
        if t <= t1 {
            return a0 + (a1 - a0) * ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
        }
    }
    0.0
}

/// The blink's opacity at `t` seconds: 1 to 0.3 and back over 1.6 s. Pure.
pub fn blink_alpha(t: f32) -> f32 {
    let phase = (t / crate::theme::motion::BLINK_PERIOD_S) * std::f32::consts::TAU;
    0.65 + 0.35 * phase.cos()
}

/// Results a project's run records hold: verified contracts plus completed runs.
/// It only grows while records are kept, so a growth since the last view is a new
/// result. Pure.
pub fn results_count(runs: &[StoredRun]) -> u64 {
    runs.iter()
        .filter_map(|r| r.record.as_ref().ok())
        .map(|r| r.verified_contracts.len() as u64 + u64::from(r.status == RunStatus::Completed))
        .sum()
}

/// `path` in canonical form, or as given when it cannot be resolved (a folder
/// that is gone stays listed with its reason; it is never dropped).
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// What a project's run records say needs the user (a failure needs input, so it is
/// yellow like a block).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordFlags {
    /// Failed gates of unfinished runs.
    pub failed: usize,
    /// Unfinished runs blocked on the user ("nothing can progress without the
    /// user"), plus gates and contracts held for the user.
    pub held: usize,
}

/// [`RecordFlags`] over the stored runs marked for resume (finished runs are
/// history, not state). Pure.
pub fn record_flags(runs: &[StoredRun]) -> RecordFlags {
    use kgraf_orchestrator::{ContractStatus, GateStatus};
    let mut f = RecordFlags::default();
    for r in runs.iter().filter(|r| r.active) {
        let Ok(rec) = &r.record else { continue };
        for g in &rec.coordination.gates {
            match g.status {
                GateStatus::Failed { .. } => f.failed += 1,
                GateStatus::Held { .. } => f.held += 1,
                _ => {}
            }
        }
        f.held += usize::from(matches!(rec.status, RunStatus::Blocked { .. }));
        f.held += rec
            .open_contracts
            .iter()
            .filter(|c| matches!(c.status, ContractStatus::Held { .. }))
            .count();
    }
    f
}

/// One failure that needs the user (it is needs input), shown as a
/// pinned failure card with a Dismiss action where it can be acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Stable identity: acknowledging it records this key.
    pub key: String,
    /// What failed, in words (TXT-2).
    pub words: String,
    /// A condition (a missing folder, unreadable records) clears only when it is
    /// fixed; an event (a run or a gate that failed) can be dismissed.
    pub dismissable: bool,
}

/// Most acknowledgments kept per project; the oldest leave first.
pub const MAX_ACKED: usize = 500;

/// Where a project's acknowledged failures are kept.
pub fn acked_path(root: &Path) -> PathBuf {
    root.join(kgraf_graph::snapshot::STATE_DIR)
        .join("acknowledged.json")
}

/// The acknowledged failure keys of `root`; none when the file is absent or
/// unreadable (said in the log: an acknowledgment lost shows the card again,
/// which is the safe side).
pub fn load_acked(root: &Path) -> Vec<String> {
    let path = acked_path(root);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            log::warn!("{}: {e}; acknowledged failures show again", path.display());
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

fn save_acked(root: &Path, acked: &[String]) -> Result<(), String> {
    let path = acked_path(root);
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(acked).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Every failure of a project that is not acknowledged: the project cannot be
/// shown, its run records cannot be read, the last run hosted in its box failed
/// (`box_run`: key, words, one key per attempt), a scheduled run failed (`scheduled`: key, words),
/// or a gate of an unfinished run failed. A new run of the box, or of the
/// schedule, supersedes the earlier failure (its source forgets it); a gate's
/// key names the failure's time, so a new failure of the same gate shows
/// again. Pure.
pub fn collect_failures(
    problem: Option<&str>,
    unreadable: Option<&str>,
    box_run: Option<(String, String)>,
    scheduled: &[(String, String)],
    runs: &[StoredRun],
    acked: &[String],
) -> Vec<Failure> {
    use kgraf_orchestrator::GateStatus;
    let mut out = Vec::new();
    if let Some(p) = problem {
        out.push(Failure {
            key: "problem".into(),
            words: p.to_owned(),
            dismissable: false,
        });
    }
    if let Some(e) = unreadable {
        out.push(Failure {
            key: "records".into(),
            words: format!("the run records cannot be read: {e}"),
            dismissable: false,
        });
    }
    // (key, words), keyed per attempt (`HostFailure::key`).
    if let Some((key, words)) = box_run {
        out.push(Failure {
            key,
            words,
            dismissable: true,
        });
    }
    for (key, words) in scheduled {
        out.push(Failure {
            key: key.clone(),
            words: words.clone(),
            dismissable: true,
        });
    }
    for r in runs.iter().filter(|r| r.active) {
        let Ok(rec) = &r.record else { continue };
        for g in &rec.coordination.gates {
            if let GateStatus::Failed {
                at_ms, remediation, ..
            } = &g.status
            {
                out.push(Failure {
                    key: format!("gate:{}/{}@{at_ms}", r.run, g.id),
                    words: format!("gate {} of run {} failed: {remediation}", g.id, r.run),
                    dismissable: true,
                });
            }
        }
    }
    out.retain(|f| !(f.dismissable && acked.contains(&f.key)));
    out
}

/// What opening a project gives the window: its host and scheduler (built by the
/// caller, who knows about the tray), and anything to say about the opening.
#[derive(Default)]
pub struct ProjectParts {
    pub host: Option<HostParts>,
    pub scheduler: Option<SchedulerHandle>,
    /// Why the project cannot be shown (a missing folder, a failed migration).
    pub problem: Option<String>,
    /// Said once in the project's box (a state migration's notice).
    pub notice: Option<String>,
}

/// Builds a project's parts: `(folder, background mode)`.
pub type Opener = Box<dyn Fn(&Path, bool) -> ProjectParts>;

/// A new project accepted from a question
/// offer is seeded with the question as its starting instruction. Written
/// before the project is opened; `take_seed_instruction` sends it once, the
/// first time the project's box is idle and able to start (`OpenProject::poll`),
/// the same way a typed `/run` would, and removes the file so it never
/// resends (a restart before that first frame finds it waiting, unharmed).
pub fn seed_instruction_path(root: &Path) -> PathBuf {
    root.join(".kgraf").join("seed_instruction.txt")
}

/// Writes the seed instruction for a not-yet-opened (or freshly created)
/// project folder.
pub fn write_seed_instruction(root: &Path, question: &str) -> std::io::Result<()> {
    let path = seed_instruction_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, question)
}

/// Takes the seed instruction, if one is waiting, removing the file so it is
/// sent at most once.
fn take_seed_instruction(root: &Path) -> Option<String> {
    let path = seed_instruction_path(root);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(text)
}

/// Opens `root` through the same path "Add project" uses (`Workspace::open`),
/// seeded with `question` as its starting instruction ("accepting creates the project through the
/// same path 'Add project' uses"). The seed is written before the open, so it is waiting the first
/// time the new project's box can start one.
pub fn open_seeded_project(
    workspace: &mut Workspace,
    root: PathBuf,
    parts: impl FnOnce() -> ProjectParts,
    question: &str,
) -> std::io::Result<usize> {
    write_seed_instruction(&root, question)?;
    Ok(workspace.open(root, parts))
}

/// One open project.
pub struct OpenProject {
    pub root: PathBuf,
    /// Created the first time the project is shown, then kept, so switching back
    /// does not reload the graph.
    pub galaxy: Option<ProjectView>,
    pub galaxy_error: Option<String>,
    /// Made on the first frame after opening; lives as long as the project.
    pub interaction: Option<Interaction>,
    host: Option<HostParts>,
    /// Its background scheduler loop, when a tray exists to show it.
    pub scheduler: Option<SchedulerHandle>,
    /// The live activity tail, while the project is shown.
    pub activity: Option<ActivityWatch>,
    pub activity_rows: Arc<Vec<ActivityRow>>,
    pub activity_error: Option<String>,
    /// Each activity row's slot, for (rows, model generation).
    pub activity_slots: (Option<(usize, u64)>, Vec<Option<u32>>),
    /// The session graph's tail (decision 0067), off the UI thread like
    /// `activity` above; stopped the same way when the project is not shown.
    pub sessions: Option<crate::session_feed::SessionFeed>,
    pub session_graph: Option<Arc<kgraf_graph::sessions::SessionGraph>>,
    pub session_error: Option<String>,
    /// Results when the user last viewed the project; `None` until first read,
    /// when the reading becomes the baseline (nothing is new at launch).
    seen_results: Option<u64>,
    /// Why the project cannot be shown.
    pub problem: Option<String>,
    notice: Option<String>,
    /// Failure keys the user acknowledged (Dismiss), from `.kgraf/acknowledged.json`.
    acked: Vec<String>,
    /// Presumed work nodes from the roadmap, run at spin-up and again on every
    /// change (decision 0064; `presume_bg`).
    presume: PresumeWatcher,
    /// A close in progress: the scheduler pause, taken off the UI thread,
    /// arrives here (`None` when a tick did not end in time).
    closing: Option<std::sync::mpsc::Receiver<Option<crate::scheduler_bg::SchedulerPause>>>,
    /// Test probe: reports the thread that drops this project.
    #[cfg(test)]
    drop_probe: Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
}

#[cfg(test)]
impl Drop for OpenProject {
    fn drop(&mut self) {
        if let Some(tx) = self.drop_probe.take() {
            let _ = tx.send(std::thread::current().id());
        }
    }
}

impl std::fmt::Debug for OpenProject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenProject")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl OpenProject {
    pub fn new(root: PathBuf, parts: ProjectParts) -> Self {
        let acked = load_acked(&root);
        let presume = PresumeWatcher::new(root.clone());
        Self {
            root,
            galaxy: None,
            galaxy_error: None,
            interaction: None,
            host: parts.host,
            scheduler: parts.scheduler,
            activity: None,
            activity_rows: Arc::new(Vec::new()),
            activity_error: None,
            activity_slots: (None, Vec::new()),
            sessions: None,
            session_graph: None,
            session_error: None,
            seen_results: None,
            problem: parts.problem,
            notice: parts.notice,
            acked,
            closing: None,
            presume,
            #[cfg(test)]
            drop_probe: None,
        }
    }

    /// The folder's name.
    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// The tile letter: the name's first letter, upper case.
    pub fn letter(&self) -> String {
        self.name()
            .chars()
            .find(|c| c.is_alphanumeric())
            .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
    }

    /// The interaction, made on first use.
    pub fn interaction_mut(&mut self, ctx: &egui::Context) -> Option<&mut Interaction> {
        if self.problem.is_some() {
            return None;
        }
        if self.interaction.is_none() {
            let mut i = Interaction::new(ctx, Some(&self.root), self.host.take());
            if let Some(n) = self.notice.take() {
                i.box_model.say(crate::orchestrator_box::Line::Notice(n));
            }
            self.interaction = Some(i);
        }
        self.interaction.as_mut()
    }

    /// Takes what the project's background parts sent: run records, transcripts,
    /// the hosted run's events. Runs every frame for every open project, shown or
    /// not, so a run in the background advances and its dot is current.
    pub fn poll(&mut self, ctx: &egui::Context) {
        // A project that cannot be shown has nothing to presume into: its
        // folder or state may not even be there (`self.problem`).
        if self.problem.is_none() {
            self.presume.tick();
        }
        // Taken before `interaction_mut` (the borrow checker's, not a choice:
        // `i` below is `&mut self` for as long as it lives, so `self.presume`
        // cannot be touched once it exists). This is never a silent drop: as
        // long as `self.problem` never changes after construction (the only
        // place it is set), `interaction_mut` returns `None` exactly when the
        // tick above did not run, so `presumed` is `None` too on every frame
        // that reaches the `else` branch below - there is nothing to lose.
        // The day `self.problem` becomes mutable, this invariant needs
        // restating (or the notice needs to be pushed back onto the watcher
        // instead of dropped here).
        let presumed = self.presume.take_notice();
        let root = self.root.clone();
        let Some(i) = self.interaction_mut(ctx) else {
            debug_assert!(
                presumed.is_none(),
                "a presume notice arrived for a project with no box to show it in"
            );
            return;
        };
        if let Some(n) = presumed {
            i.box_model.say(if n.is_error {
                Line::Error(n.text)
            } else {
                Line::Notice(n.text)
            });
        }
        i.poll();
        i.absorb_backend();
        // A freshly opened, seeded project sends its
        // question as soon as it can start one, the same way a typed `/run`
        // would, and removes the file so it never resends.
        if matches!(
            i.host().state(),
            BackendState::Idle {
                can_start: true,
                ..
            }
        ) && let Some(seed) = take_seed_instruction(&root)
        {
            if let Err(e) = i.host_mut().start(&seed) {
                i.box_model.say(Line::Error(format!(
                    "the seeded instruction did not start: {e}"
                )));
            } else {
                i.box_model.say(Line::Notice(format!(
                    "starting a run with the seeded instruction: {seed}"
                )));
            }
        }
        if let Some(s) = &self.scheduler
            && let Some(i) = self.interaction.as_mut()
        {
            i.set_schedule_nodes(s.board().plan_nodes());
        }
        if let Some(watch) = &self.activity {
            match watch.latest() {
                Some(ActivityUpdate::Rows(rows)) => {
                    self.activity_rows = rows;
                    self.activity_error = None;
                }
                Some(ActivityUpdate::Unavailable(e)) => self.activity_error = Some(e),
                None => {}
            }
        }
        if let Some(feed) = &self.sessions {
            match feed.latest() {
                Some(crate::session_feed::SessionUpdate::Graph(graph)) => {
                    self.session_graph = Some(graph);
                    self.session_error = None;
                }
                Some(crate::session_feed::SessionUpdate::Unavailable(e)) => {
                    self.session_error = Some(e)
                }
                None => {}
            }
        }
    }

    /// The failures that need the user and are not acknowledged, each shown as
    /// a pinned failure card (3).
    pub fn failures(&self) -> Vec<Failure> {
        let host = self
            .interaction
            .as_ref()
            .and_then(|i| i.host().last_failure())
            .map(|f| (f.key(), format!("run {}: {}", f.run, f.words)));
        let scheduled = self
            .scheduler
            .as_ref()
            .map(|s| s.board().failed_runs())
            .unwrap_or_default();
        let (runs, unreadable) = match &self.interaction {
            Some(i) => (i.runs.runs.as_slice(), i.runs.error.clone()),
            None => (&[][..], None),
        };
        collect_failures(
            self.problem.as_deref(),
            unreadable.as_deref(),
            host,
            &scheduled,
            runs,
            &self.acked,
        )
    }

    /// Acknowledges failure `key`: its card leaves, and the dot and the bell no
    /// longer count it. Kept in `.kgraf/acknowledged.json`, so a restart does
    /// not bring it back. Returns why it was not kept, when the write failed.
    pub fn dismiss(&mut self, key: &str) -> Result<(), String> {
        if !self.acked.iter().any(|k| k == key) {
            self.acked.push(key.to_owned());
            // Bounded: the oldest acknowledgments leave first.
            let over = self.acked.len().saturating_sub(MAX_ACKED);
            self.acked.drain(..over);
        }
        save_acked(&self.root, &self.acked)
    }

    /// The live signals.
    pub fn signals(&self) -> Signals {
        let failures = self.failures().len();
        let Some(i) = &self.interaction else {
            return Signals {
                failures,
                ..Signals::default()
            };
        };
        let host = i.host();
        let running = !matches!(host.state(), BackendState::Idle { .. });
        let admitted = host.admission().is_some_and(|a| a.gate().in_flight().0 > 0);
        let rec = record_flags(&i.runs.runs);
        Signals {
            approvals: host.pending_asks().len(),
            questions: 0,
            held: rec.held,
            failures,
            working: running || admitted || !i.runs.leased.is_empty(),
            results: results_count(&i.runs.runs),
        }
    }

    /// The status, from [`Self::signals`].
    pub fn status(&self) -> ProjectStatus {
        dot_status(self.flags())
    }

    /// A close was asked for and waits for the scheduler's tick to end.
    pub fn is_closing(&self) -> bool {
        self.closing.is_some()
    }

    /// Why the project cannot be closed now, if it works.
    fn busy(&self) -> Option<String> {
        (self.hosting_a_run() || self.signals().working).then(|| {
            format!(
                "{} is working (a run, a scheduled run or work in flight); stop it first. \
                 Its record stays resumable",
                self.name()
            )
        })
    }

    /// The three flags, from [`Self::signals`] and the results last seen.
    pub fn flags(&self) -> Flags {
        let s = self.signals();
        Flags::of(&s, self.seen_results.unwrap_or(s.results))
    }

    /// The user is looking at the project: its results are seen.
    pub fn mark_viewed(&mut self) {
        self.seen_results = Some(self.signals().results);
    }

    /// Sets the baseline on the first read (nothing is new at launch).
    fn baseline(&mut self) {
        if self.seen_results.is_none() && self.interaction.is_some() {
            self.seen_results = Some(self.signals().results);
        }
    }

    /// The one line of state under the name (TXT-2: the state and its reason
    /// in words).
    pub fn state_line(&self) -> String {
        if self.is_closing() {
            return "Closing, after the scheduler's current check".to_owned();
        }
        if let Some(p) = &self.problem {
            return format!("Failed: {p}");
        }
        let s = self.signals();
        match self.status() {
            ProjectStatus::NeedsInput => {
                let failures = self.failures();
                match failures.as_slice() {
                    [one] if s.needs() == 1 => format!("Needs input: {}", one.words),
                    _ => format!("Needs input: {}", s.needs_words()),
                }
            }
            ProjectStatus::NewResults => {
                let n = s.results - self.seen_results.unwrap_or(s.results);
                format!("New results: {n} since you looked")
            }
            ProjectStatus::Working => match self.interaction.as_ref().map(|i| i.host().state()) {
                Some(BackendState::Running { run }) => format!("Working, run {run}"),
                Some(BackendState::Starting) => "Working, a run is starting".to_owned(),
                _ => "Working".to_owned(),
            },
            ProjectStatus::Idle => "Idle".to_owned(),
        }
    }

    /// Whether a run is hosted here now (closing is refused then).
    pub fn hosting_a_run(&self) -> bool {
        self.interaction
            .as_ref()
            .is_some_and(|i| !matches!(i.host().state(), BackendState::Idle { .. }))
    }
}

/// The open projects and which one is shown.
#[derive(Debug, Default)]
pub struct Workspace {
    projects: Vec<OpenProject>,
    active: Option<usize>,
    /// Why a pending close was refused, for the window to say once.
    close_errors: Vec<String>,
}

/// What [`Workspace::close`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseStart {
    /// The project is closed.
    Closed,
    /// The close waits for the scheduler's tick; [`Workspace::tick`] finishes it.
    Pending,
}

/// How long a close waits for a scheduler tick in progress.
const CLOSE_PAUSE_WAIT: Duration = Duration::from_secs(2);

impl Workspace {
    pub fn projects(&self) -> &[OpenProject] {
        &self.projects
    }

    pub fn projects_mut(&mut self) -> &mut [OpenProject] {
        &mut self.projects
    }

    pub fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub fn active(&self) -> Option<&OpenProject> {
        self.projects.get(self.active?)
    }

    pub fn active_mut(&mut self) -> Option<&mut OpenProject> {
        self.projects.get_mut(self.active?)
    }

    /// The workspace a launch restores: the saved projects in their order, then
    /// `launch` (the `--project` folder) when it is not among them. `launch` is
    /// shown; without it the saved active project is. `open` builds each
    /// project's parts and is given the canonical path (a
    /// caller comparing against `launch` compares canonical forms, see
    /// `lib.rs`), so every spelling of one folder opens it once.
    pub fn restore(
        saved: &[PathBuf],
        saved_active: Option<&Path>,
        launch: Option<&Path>,
        mut open: impl FnMut(&Path) -> ProjectParts,
    ) -> Self {
        let mut w = Self::default();
        for root in saved.iter().map(PathBuf::as_path).chain(launch) {
            let root = canonical(root);
            w.open(root.clone(), || open(&root));
        }
        if let Some(i) = launch.or(saved_active).and_then(|r| w.find(r)) {
            w.switch_to(i);
        }
        w
    }

    /// The index of the project at `root`, if it is open. Paths compare in
    /// canonical form, so `a/../a` or a link to an open folder finds it.
    pub fn find(&self, root: &Path) -> Option<usize> {
        let root = canonical(root);
        self.projects.iter().position(|p| p.root == root)
    }

    /// Opens `root` with `parts`, or finds it if it is open already (one folder is
    /// never opened twice: two hosts over one `.kgraf` would race). The
    /// first project opened becomes the shown one.
    pub fn open(&mut self, root: PathBuf, parts: impl FnOnce() -> ProjectParts) -> usize {
        let root = canonical(&root);
        if let Some(i) = self.find(&root) {
            return i;
        }
        self.projects.push(OpenProject::new(root, parts()));
        let i = self.projects.len() - 1;
        if self.active.is_none() {
            self.active = Some(i);
        }
        i
    }

    /// Shows project `i`. The one shown before keeps running; its activity tail
    /// stops (the card shows only the shown project).
    pub fn switch_to(&mut self, i: usize) {
        if i >= self.projects.len() || self.active == Some(i) {
            return;
        }
        if let Some(old) = self.active_mut() {
            old.mark_viewed();
            old.activity = None;
        }
        self.active = Some(i);
        if let Some(new) = self.active_mut() {
            new.mark_viewed();
        }
    }

    /// Closes project `i`, unless it works: a run hosted here, a scheduled run or
    /// any work its admission has in flight, or a run lease a live process holds
    /// (H2). Stop the work first; its record stays resumable.
    ///
    /// With a scheduler, the close pauses it across the check and the removal,
    /// so no scheduled run is admitted between "idle" and the drop. Taking the
    /// pause waits out a tick in progress, which may take a while, so it is
    /// taken on a thread of its own: the close is [`CloseStart::Pending`], the
    /// row says "Closing", and [`Self::tick`] finishes it (or says why not, in
    /// [`Self::take_close_error`]). The UI never waits (final check item 3). The
    /// closed project's parts are dropped on a thread of their own too.
    pub fn close(&mut self, i: usize) -> Result<CloseStart, String> {
        let Some(p) = self.projects.get_mut(i) else {
            return Err("no such project".to_owned());
        };
        if p.closing.is_some() {
            return Ok(CloseStart::Pending);
        }
        if let Some(why) = p.busy() {
            return Err(why);
        }
        let Some(pauser) = p.scheduler.as_ref().map(|s| s.pauser()) else {
            self.remove_closed(i);
            return Ok(CloseStart::Closed);
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("project-close-pause".into())
            .spawn(move || {
                let _ = tx.send(pauser.pause(CLOSE_PAUSE_WAIT));
            });
        match spawned {
            Ok(_) => {
                p.closing = Some(rx);
                Ok(CloseStart::Pending)
            }
            Err(e) => Err(format!("{}: the close could not start: {e}", p.name())),
        }
    }

    /// Finishes the closes whose scheduler pause arrived: checks again, under the
    /// pause, that the project is idle, then removes it (the pause is kept, so
    /// the scheduler stays off until it is dropped with the project), or gives
    /// the pause back and says why.
    fn finish_closes(&mut self) {
        let mut i = 0;
        while i < self.projects.len() {
            let p = &mut self.projects[i];
            let arrived = match p.closing.as_ref().map(|rx| rx.try_recv()) {
                None | Some(Err(std::sync::mpsc::TryRecvError::Empty)) => None,
                Some(Ok(pause)) => Some(pause),
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => Some(None),
            };
            let Some(pause) = arrived else {
                i += 1;
                continue;
            };
            p.closing = None;
            match pause {
                None => {
                    self.close_errors.push(format!(
                        "{} is starting scheduled work; try again in a moment",
                        p.name()
                    ));
                    i += 1;
                }
                Some(pause) => match p.busy() {
                    Some(why) => {
                        // Dropping the pause turns the scheduler back on.
                        drop(pause);
                        self.close_errors.push(why);
                        i += 1;
                    }
                    None => {
                        pause.keep();
                        self.remove_closed(i);
                    }
                },
            }
        }
    }

    /// Why a pending close was refused, once.
    pub fn take_close_error(&mut self) -> Option<String> {
        (!self.close_errors.is_empty()).then(|| self.close_errors.remove(0))
    }

    /// Removes project `i` and drops its parts off the calling thread.
    fn remove_closed(&mut self, i: usize) {
        let closed = self.projects.remove(i);
        self.active = match self.active {
            _ if self.projects.is_empty() => None,
            Some(a) if a > i => Some(a - 1),
            Some(a) if a == i => Some(i.min(self.projects.len() - 1)),
            other => other,
        };
        let name = closed.name();
        let dropped = std::thread::Builder::new()
            .name("project-close".into())
            .spawn(move || drop(closed));
        if let Err(e) = dropped {
            log::error!("{name}: its parts could not be released off the UI thread: {e}");
        }
    }

    /// Polls every project and keeps the shown one's results seen. Call once
    /// per frame.
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.finish_closes();
        if self.projects.iter().any(OpenProject::is_closing) {
            // The pause arrives on another thread; look again soon.
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        for p in &mut self.projects {
            p.poll(ctx);
            p.baseline();
        }
        if let Some(p) = self.active_mut() {
            p.mark_viewed();
        }
    }

    /// Each project's row dot, in order.
    pub fn statuses(&self) -> Vec<ProjectStatus> {
        self.projects.iter().map(OpenProject::status).collect()
    }

    /// The collapsed hamburger's aggregate over every open project.
    pub fn hamburger(&self) -> ProjectStatus {
        hamburger_status(self.projects.iter().map(OpenProject::flags))
    }

    /// The open folders, in order (for the settings file).
    pub fn roots(&self) -> Vec<PathBuf> {
        self.projects.iter().map(|p| p.root.clone()).collect()
    }

    /// The items that need the user over every open project (the bell):
    /// approvals, questions, held runs and failures (a failure
    /// needs input).
    pub fn bell(&self) -> usize {
        self.projects.iter().map(|p| p.signals().needs()).sum()
    }

    /// The collapsed hamburger's tooltip, naming the projects (CMP-BDG-4).
    pub fn hamburger_words(&self) -> String {
        let rows: Vec<(String, Flags, Signals)> = self
            .projects
            .iter()
            .map(|p| (p.name(), p.flags(), p.signals()))
            .collect();
        hamburger_words(&rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(approvals: usize, working: bool, results: u64) -> Signals {
        Signals {
            approvals,
            working,
            results,
            ..Signals::default()
        }
    }

    fn flags(needs_input: bool, working: bool, new_results: bool) -> Flags {
        Flags {
            needs_input,
            working,
            new_results,
        }
    }

    fn stored(
        active: bool,
        edit: impl FnOnce(&mut kgraf_orchestrator::RunStateRecord),
    ) -> StoredRun {
        let mut r = kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "go", 1);
        edit(&mut r);
        StoredRun {
            run: "r1".into(),
            active,
            record: Ok(r),
        }
    }

    /// A completed run is a result even when it verified nothing.
    #[test]
    fn a_completed_run_counts_as_a_result() {
        let done = stored(false, |r| r.status = RunStatus::Completed);
        assert_eq!(results_count(&[done]), 1);
        let verified = stored(true, |r| {
            r.verified_contracts = vec!["a".into(), "b".into()]
        });
        assert_eq!(results_count(&[verified]), 2);
        assert_eq!(results_count(&[stored(true, |_| {})]), 0);
    }

    /// A failed gate and a blocked run both
    /// need input (yellow), counted by kind; finished runs are history and
    /// count nothing.
    #[test]
    fn failures_and_blocks_need_input() {
        use kgraf_orchestrator::{GateStatus, OpenContract};
        let gate = |status: GateStatus| {
            let mut g: kgraf_orchestrator::GateNode = serde_json::from_value(serde_json::json!({
                "id": "g", "major": "root", "covers": [], "after": [], "region": []
            }))
            .expect("a gate node");
            g.status = status;
            g
        };
        let failed = stored(true, |r| {
            r.coordination.gates = vec![gate(GateStatus::Failed {
                pinned: None,
                at_ms: 1,
                remediation: "fix".into(),
            })]
        });
        assert_eq!(
            record_flags(std::slice::from_ref(&failed)),
            RecordFlags { failed: 1, held: 0 }
        );
        let blocked = stored(true, |r| {
            r.status = RunStatus::Blocked {
                reason: "a decision".into(),
            }
        });
        assert_eq!(record_flags(&[blocked]).held, 1);
        let held = stored(true, |r| {
            let mut c = OpenContract::planned("a", "r1/a".into(), "make a", "ok");
            c.status = kgraf_orchestrator::ContractStatus::Held {
                reason: "retries".into(),
            };
            r.open_contracts = vec![c];
        });
        assert_eq!(record_flags(&[held]).held, 1);
        let finished = StoredRun {
            active: false,
            ..failed
        };
        assert_eq!(record_flags(&[finished]), RecordFlags::default());
        // Through the flags and the one priority: yellow, and the bell counts it.
        let s = Signals {
            failures: 1,
            questions: 2,
            ..Signals::default()
        };
        assert_eq!(dot_status(Flags::of(&s, 0)), ProjectStatus::NeedsInput);
        assert_eq!(
            s.needs(),
            3,
            "the bell counts failures as needs-input items"
        );
        assert_eq!(s.needs_words(), "1 failure, 2 questions");
        let s = Signals {
            held: 1,
            working: true,
            results: 9,
            ..Signals::default()
        };
        assert_eq!(dot_status(Flags::of(&s, 0)), ProjectStatus::NeedsInput);
    }

    /// A failed scheduled run counts, and an acknowledged failure
    /// is gone while a condition cannot be dismissed. Superseding and the
    /// durable source are tested in `schedule_store`
    /// (`scheduled_failures_come_from_the_durable_history`).
    #[test]
    fn scheduled_failures_count_and_acknowledgments_hold() {
        use crate::scheduler_bg::ScheduleBoard;
        let board = ScheduleBoard::default();
        board.set_failed(vec![(
            "scheduled:nightly#7".to_owned(),
            "scheduled run nightly failed: no route".to_owned(),
        )]);
        let scheduled = board.failed_runs();
        assert_eq!(scheduled.len(), 1);
        let f = collect_failures(None, None, None, &scheduled, &[], &[]);
        assert_eq!(f.len(), 1);
        assert!(f[0].words.contains("nightly") && f[0].dismissable);
        let s = Signals {
            failures: f.len(),
            ..Signals::default()
        };
        assert_eq!(dot_status(Flags::of(&s, 0)), ProjectStatus::NeedsInput);
        // Acknowledged: gone. A condition stays whatever is acknowledged.
        let acked = vec![f[0].key.clone(), "records".to_owned()];
        assert!(collect_failures(None, None, None, &scheduled, &[], &acked).is_empty());
        let cond = collect_failures(None, Some("locked"), None, &[], &[], &acked);
        assert_eq!(cond.len(), 1);
        assert!(!cond[0].dismissable);
    }

    /// Close drops the project's parts on another thread, never on the caller's
    /// (the UI thread): their joins must not stall a frame (low item).
    #[test]
    fn close_drops_the_parts_off_the_calling_thread() {
        let dir = tempfile::tempdir().expect("dir");
        let mut w = Workspace::default();
        let i = w.open(dir.path().to_path_buf(), ProjectParts::default);
        let (tx, rx) = std::sync::mpsc::channel();
        w.projects_mut()[i].drop_probe = Some(tx);
        w.close(i).expect("an idle project closes");
        let dropped_on = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the closed project is dropped");
        assert_ne!(dropped_on, std::thread::current().id());
        assert!(w.projects().is_empty());
    }

    /// `open_seeded_project` opens through the same
    /// `Workspace::open` "Add project" uses, and leaves the question waiting
    /// as a seed instruction for the new project's first idle-and-able frame.
    #[test]
    fn a_seeded_project_opens_through_workspace_open_with_the_seed_waiting() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("new-project");
        let mut w = Workspace::default();
        let i = open_seeded_project(
            &mut w,
            root.clone(),
            ProjectParts::default,
            "design a separate mobile app",
        )
        .expect("opens");
        assert_eq!(w.projects()[i].root, canonical(&root));
        assert_eq!(
            take_seed_instruction(&root).as_deref(),
            Some("design a separate mobile app")
        );
        // Taken once: a second read finds nothing (the file was removed).
        assert_eq!(take_seed_instruction(&root), None);
    }

    /// Final check item 3: a close takes the scheduler's pause before it removes
    /// the project, and waits for it off the UI thread. A tick in progress (here
    /// a pause the test holds) keeps the close pending, with the row saying so,
    /// and `close` itself returns at once; the close finishes when the tick
    /// ends. A close without the pause removes the project at once and fails
    /// this test.
    #[test]
    fn a_close_waits_for_the_scheduler_tick_without_blocking() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = egui::Context::default();
        let h = SchedulerHandle::spawn(dir.path().to_path_buf(), true);
        let tick_in_progress = h.pause(Duration::from_secs(5)).expect("held");
        let mut w = Workspace::default();
        let i = w.open(dir.path().to_path_buf(), || ProjectParts {
            scheduler: Some(h),
            ..ProjectParts::default()
        });
        let asked = std::time::Instant::now();
        assert_eq!(w.close(i), Ok(CloseStart::Pending));
        assert!(
            asked.elapsed() < Duration::from_millis(100),
            "close returned after {:?}",
            asked.elapsed()
        );
        assert!(w.projects()[i].state_line().starts_with("Closing"));
        let until = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < until {
            w.tick(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(w.projects().len(), 1, "still waiting for the tick");
        assert!(w.projects()[i].is_closing());
        drop(tick_in_progress);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !w.projects().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the close never finished"
            );
            w.tick(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(w.take_close_error().is_none());
    }

    /// A tick that outlasts the close's wait refuses the close, says why, and
    /// turns the scheduler back on.
    #[test]
    fn a_close_that_cannot_pause_the_scheduler_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = egui::Context::default();
        let h = SchedulerHandle::spawn(dir.path().to_path_buf(), true);
        let pauser = h.pauser();
        let tick_in_progress = h.pause(Duration::from_secs(5)).expect("held");
        let mut w = Workspace::default();
        let i = w.open(dir.path().to_path_buf(), || ProjectParts {
            scheduler: Some(h),
            ..ProjectParts::default()
        });
        assert_eq!(w.close(i), Ok(CloseStart::Pending));
        let deadline = std::time::Instant::now() + CLOSE_PAUSE_WAIT + Duration::from_secs(3);
        while w.projects()[i].is_closing() {
            assert!(std::time::Instant::now() < deadline, "no answer");
            w.tick(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        let why = w.take_close_error().expect("refused");
        assert!(why.contains("scheduled work"), "{why}");
        assert_eq!(w.projects().len(), 1);
        drop(tick_in_progress);
        let resumed = pauser.pause(Duration::from_secs(1)).expect("free");
        drop(resumed);
        assert!(
            w.projects()[i]
                .scheduler
                .as_ref()
                .is_some_and(SchedulerHandle::background_mode),
            "the scheduler is on again"
        );
    }

    /// A close pauses the scheduler across its check: a refused close restores
    /// the switch, an accepted one leaves it off.
    #[test]
    fn close_pauses_the_scheduler_across_its_check() {
        let dir = tempfile::tempdir().expect("dir");
        let h = SchedulerHandle::spawn(dir.path().to_path_buf(), true);
        let g = h.pause(Duration::from_secs(5)).expect("paused");
        // While paused, a second pause cannot take the tick lock.
        assert!(!h.background_mode());
        assert!(h.pause(Duration::from_millis(50)).is_none());
        drop(g);
        assert!(h.background_mode(), "a dropped pause restores the switch");
        let g = h.pause(Duration::from_secs(5)).expect("paused again");
        g.keep();
        assert!(!h.background_mode(), "a kept pause leaves it off");
        assert!(
            h.pause(Duration::from_secs(5)).is_some(),
            "the lock is free"
        );
    }

    /// A dismissal survives a reload of the project.
    #[test]
    fn acknowledgments_are_kept_on_disk() {
        let dir = tempfile::tempdir().expect("dir");
        let mut p = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        p.dismiss("run:box-1").expect("dismiss");
        let again = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        assert_eq!(again.acked, ["run:box-1"]);
    }

    /// CMP-BDG-4: the tooltip names the projects and the kinds.
    #[test]
    fn the_hamburger_tooltip_names_the_projects() {
        let row = |n: &str, s: Signals| (n.to_owned(), Flags::of(&s, 0), s);
        let fail = Signals {
            failures: 1,
            questions: 2,
            ..Signals::default()
        };
        let work = Signals {
            working: true,
            ..Signals::default()
        };
        assert_eq!(
            hamburger_words(&[row("Flume", fail), row("Ad Astra", work)]),
            "Show projects. Flume: 1 failure, 2 questions"
        );
        assert_eq!(
            hamburger_words(&[row("A", work), row("B", work), row("C", work)]),
            "Show projects. A, B and C are working"
        );
        let new = Signals {
            results: 1,
            ..Signals::default()
        };
        assert_eq!(
            hamburger_words(&[row("A", new)]),
            "Show projects. A has new results"
        );
        assert_eq!(
            hamburger_words(&[row("A", Signals::default())]),
            "Show projects"
        );
    }

    /// One folder is not opened twice under another spelling.
    #[test]
    fn a_folder_opens_once_whatever_its_spelling() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("p");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        let dotted = root.join("sub").join("..");
        let mut opened = Vec::new();
        let w = Workspace::restore(&[dotted.clone(), root.clone()], None, Some(&dotted), |r| {
            opened.push(r.to_path_buf());
            ProjectParts::default()
        });
        // The opener sees the canonical path: `lib.rs` matches it against the
        // canonical `--project` to hand over the caller's host.
        assert_eq!(opened, [canonical(&root)], "one host for one folder");
        assert_eq!(w.projects().len(), 1);
        assert_eq!(w.roots(), [canonical(&root)]);
        assert_eq!(w.find(&dotted), Some(0));
    }

    /// The flags come from live signals against the results last seen.
    #[test]
    fn flags_follow_the_signals() {
        assert_eq!(Flags::of(&sig(0, false, 3), 3), flags(false, false, false));
        assert_eq!(Flags::of(&sig(0, false, 4), 3), flags(false, false, true));
        assert_eq!(Flags::of(&sig(1, true, 4), 3), flags(true, true, true));
        let q = Signals {
            questions: 1,
            ..Signals::default()
        };
        assert!(Flags::of(&q, 0).needs_input, "a question counts too");
    }

    /// One order everywhere (0066): yellow, then cyan, then gray.
    #[test]
    fn the_dot_is_yellow_then_blue_then_gray() {
        use ProjectStatus::*;
        assert_eq!(dot_status(flags(false, false, false)), Idle);
        assert_eq!(dot_status(flags(false, true, false)), Working);
        assert_eq!(dot_status(flags(false, false, true)), NewResults);
        assert_eq!(dot_status(flags(true, true, true)), NeedsInput);
        assert_eq!(dot_status(flags(true, false, false)), NeedsInput);
    }

    /// A project that is both working and has new results is cyan, in its row
    /// and in the hamburger: one function decides both.
    #[test]
    fn working_with_new_results_is_blue_in_the_row_and_the_hamburger() {
        use ProjectStatus::*;
        let both = flags(false, true, true);
        assert_eq!(dot_status(both), NewResults, "the row");
        assert_eq!(hamburger_status([both]), NewResults, "the hamburger");
        // Across projects the same order holds on the combined flags.
        assert_eq!(
            hamburger_status([flags(false, true, false), flags(false, false, true)]),
            NewResults,
            "one working, another with new results: cyan"
        );
        assert_eq!(
            hamburger_status([flags(false, true, false), flags(true, false, false)]),
            NeedsInput
        );
        assert_eq!(hamburger_status([flags(false, true, false)]), Working);
        assert_eq!(hamburger_status([]), Idle);
        assert_eq!(
            any_flags([both, flags(true, false, false)]),
            flags(true, true, true)
        );
    }

    /// The hamburger's look: yellow wiggles, gray blinks, cyan is still.
    #[test]
    fn the_hamburger_look_follows_its_status() {
        use ProjectStatus::*;
        let look = hamburger_look(
            hamburger_status([flags(true, true, false)]),
            false,
            false,
            false,
        );
        assert_eq!(look.indicator, Some(Indicator::NeedsInput));
        assert!(look.wiggle && !look.blink);
        let look = hamburger_look(Working, false, false, false);
        assert_eq!(look.indicator, Some(Indicator::Neutral));
        assert!(look.blink && !look.wiggle);
        let look = hamburger_look(NewResults, false, false, false);
        assert_eq!(look.indicator, Some(Indicator::Selection));
        assert!(!look.blink && !look.wiggle);
        assert_eq!(hamburger_look(Idle, false, false, false).indicator, None);
    }

    /// Open sidebar: static and plain. Hover or reduced motion: no wiggle, color kept.
    #[test]
    fn the_hamburger_is_static_when_open_hovered_or_reduced() {
        use ProjectStatus::*;
        let open = hamburger_look(NeedsInput, true, false, false);
        assert_eq!(open.indicator, None);
        assert!(!open.wiggle && !open.blink);
        let hovered = hamburger_look(NeedsInput, false, true, false);
        assert_eq!(hovered.indicator, Some(Indicator::NeedsInput));
        assert!(!hovered.wiggle);
        let reduced = hamburger_look(NeedsInput, false, false, true);
        assert!(!reduced.wiggle);
        assert!(!hamburger_look(Working, false, false, true).blink);
    }

    /// The wiggle swings within the first 600 ms of each 4.6 s, peaks at 8
    /// degrees, and rests after.
    #[test]
    fn the_wiggle_swings_then_rests() {
        assert_eq!(wiggle_degrees(0.0), 0.0);
        assert!((wiggle_degrees(0.03 * 4.6) - 8.0).abs() < 1e-3);
        assert!(wiggle_degrees(0.06 * 4.6) < -5.9);
        let swings = (0..600)
            .map(|ms| wiggle_degrees(ms as f32 / 1000.0))
            .collect::<Vec<_>>();
        let crossings = swings
            .windows(2)
            .filter(|w| w[0].signum() != w[1].signum())
            .count();
        assert!((2..=5).contains(&crossings), "{crossings} sign changes");
        for t in [0.7, 1.0, 2.0, 4.5] {
            assert_eq!(wiggle_degrees(t), 0.0, "rest at {t}");
        }
        assert!(
            (wiggle_degrees(4.6 + 0.03 * 4.6) - 8.0).abs() < 1e-3,
            "repeats"
        );
        assert!(swings.iter().all(|d| d.abs() <= 8.0 + 1e-3));
    }

    #[test]
    fn the_blink_runs_between_one_and_point_three() {
        assert!((blink_alpha(0.0) - 1.0).abs() < 1e-6);
        assert!((blink_alpha(0.8) - 0.3).abs() < 1e-3);
        assert!((blink_alpha(1.6) - 1.0).abs() < 1e-3);
    }

    // Presumed work at spin-up (decision 0064), driven
    // through `OpenProject::poll` exactly as the running window drives it: no
    // internal `PresumeWatcher` call is made directly, so a regression that
    // breaks the wiring (not just the mechanism) fails these too.

    /// Opens (and thereby creates) a project's graph, as `kgraf index` does,
    /// then drops it: the fixture for "an already-indexed project" every test
    /// below needs except the two that are specifically about an unindexed
    /// one - `presume_bg` never creates `graph.db` itself (C1, C2).
    fn index(root: &Path) {
        let path = kgraf_graph::snapshot::graph_db_path(root);
        std::fs::create_dir_all(path.parent().expect("state dir")).expect("mkdir");
        drop(kgraf_graph::GraphStore::open(&path, root).expect("graph"));
    }

    fn presumed_items(root: &Path) -> Vec<kgraf_interpret::presumed::PresumedView> {
        let store = kgraf_graph::GraphStore::open_read_only(
            &kgraf_graph::snapshot::graph_db_path(root),
            root,
        )
        .expect("graph");
        kgraf_interpret::presumed::views(&store)
            .expect("views")
            .into_iter()
            .filter(|v| v.kind == kgraf_graph::WorkKind::Item)
            .collect()
    }

    /// Polls `p` until its presume watcher has no read in flight and has said
    /// something at least once, folding every notice line said meanwhile.
    fn settle_presume(p: &mut OpenProject, ctx: &egui::Context) -> Vec<String> {
        let mut said = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            p.poll(ctx);
            if let Some(i) = &p.interaction {
                for line in i.box_model.lines() {
                    let text = match line {
                        crate::orchestrator_box::Line::Notice(t)
                        | crate::orchestrator_box::Line::Error(t) => Some(t.clone()),
                        _ => None,
                    };
                    if let Some(t) = text
                        && !said.contains(&t)
                    {
                        said.push(t);
                    }
                }
            }
            if p.presume.is_idle() {
                return said;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the presume read never finished"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Opening a project with a `ROADMAP.md` presumes its nodes, off the UI
    /// thread, through `OpenProject::poll` alone.
    #[test]
    fn opening_a_project_presumes_its_roadmap() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        std::fs::write(
            dir.path().join("ROADMAP.md"),
            "## Tasks\n\n- [x] done thing\n- [ ] to-do thing\n",
        )
        .expect("write");
        let ctx = egui::Context::default();
        let mut p = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        settle_presume(&mut p, &ctx);
        let items = presumed_items(dir.path());
        assert_eq!(items.len(), 2, "{items:?}");
    }

    /// Reopening the same project (a fresh `OpenProject`, as a relaunch makes)
    /// presumes again and writes no duplicate.
    #[test]
    fn reopening_does_not_duplicate_presumed_nodes() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] one\n- [x] two\n").expect("write");
        let ctx = egui::Context::default();
        let mut first = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        settle_presume(&mut first, &ctx);
        drop(first);
        let mut second = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        settle_presume(&mut second, &ctx);
        let items: Vec<_> = presumed_items(dir.path())
            .into_iter()
            .filter(|v| !v.gone)
            .collect();
        assert_eq!(items.len(), 2, "no duplicate from reopening: {items:?}");
    }

    /// Editing the roadmap after it was presumed updates the node, in place.
    #[test]
    fn editing_the_roadmap_updates_the_presumed_node() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        let path = dir.path().join("ROADMAP.md");
        std::fs::write(&path, "- [ ] one\n").expect("write");
        let ctx = egui::Context::default();
        let mut p = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        settle_presume(&mut p, &ctx);
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&path, "- [x] one\n").expect("write");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            p.poll(&ctx);
            let items: Vec<_> = presumed_items(dir.path())
                .into_iter()
                .filter(|v| !v.gone)
                .collect();
            if items.len() == 1 && items[0].state == kgraf_interpret::presumed::PresumedState::Done
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the edit was never presumed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Deleting the roadmap after it was presumed marks its node gone, not
    /// deleted, and the no-roadmap notice shows.
    #[test]
    fn deleting_the_roadmap_marks_the_presumed_node_gone() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        let path = dir.path().join("ROADMAP.md");
        std::fs::write(&path, "- [ ] one\n").expect("write");
        let ctx = egui::Context::default();
        let mut p = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        settle_presume(&mut p, &ctx);
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::remove_file(&path).expect("rm");
        let said = settle_presume(&mut p, &ctx);
        assert!(
            said.iter().any(|t| t.contains("no roadmap found")),
            "{said:?}"
        );
        let items = presumed_items(dir.path());
        assert_eq!(items.len(), 1);
        assert!(items[0].gone, "{items:?}");
    }

    /// A never-indexed project (C1, C2) shows the "not indexed" notice
    /// and is left exactly as it is: no `.kgraf`, no `graph.db`, whether or
    /// not a roadmap document happens to be sitting there.
    #[test]
    fn a_fresh_unindexed_project_shows_the_notice_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");
        let ctx = egui::Context::default();
        let mut p = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        let said = settle_presume(&mut p, &ctx);
        assert!(said.iter().any(|t| t.contains("not indexed")), "{said:?}");
        assert!(said.iter().any(|t| t.contains("kgraf index")), "{said:?}");
        assert!(
            !dir.path().join(".kgraf").exists(),
            "an unindexed project must never gain a state directory just from being opened"
        );
    }

    /// An already-indexed project with no roadmap shows the notice, once, and
    /// offers `orchestrate --corpus` rather than making a paid call itself.
    #[test]
    fn an_indexed_project_with_no_roadmap_shows_the_notice() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        let ctx = egui::Context::default();
        let mut p = OpenProject::new(dir.path().to_path_buf(), ProjectParts::default());
        let said = settle_presume(&mut p, &ctx);
        assert!(
            said.iter().any(|t| t.contains("no roadmap found")),
            "{said:?}"
        );
        assert!(
            said.iter().any(|t| t.contains("orchestrate --corpus")),
            "{said:?}"
        );
        assert!(presumed_items(dir.path()).is_empty());
        assert!(
            !dir.path().join(".kgraf").join("ledger.db").exists(),
            "no roadmap must never trigger a paid call"
        );
    }
}
