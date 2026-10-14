//! The settings surface's model: configuration only, never work state (0017).
//!
//! It carries the compaction slider (0057 addendum 2), the background-mode switch
//! (0052), the concurrency caps (0017), the worker-worktree switch (0088) and the four
//! run-limit sliders (rounds per worker, worker and orchestrator output caps, context
//! window). Anything that describes work in flight belongs on the graph, not here.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ShellError;

/// Slider bounds, percent of the effective window (0057 addendum 2: Roo Code's user
/// range starts at 5%, Codex caps usable input at 95%).
pub const COMPACTION_MIN_PERCENT: u8 = 5;
pub const COMPACTION_MAX_PERCENT: u8 = 95;
/// Production default (goose `DEFAULT_COMPACTION_THRESHOLD = 0.8`), shown as the
/// recommendation until a model has measured runs.
pub const COMPACTION_DEFAULT_PERCENT: u8 = 80;

/// A compaction point inside the slider's range. Construction is the only check, so a
/// value of this type is always in range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompactionPoint(u8);

impl CompactionPoint {
    pub fn new(percent: i64) -> Result<Self, ShellError> {
        let range = i64::from(COMPACTION_MIN_PERCENT)..=i64::from(COMPACTION_MAX_PERCENT);
        if !range.contains(&percent) {
            return Err(ShellError::CompactionOutOfRange { percent });
        }
        // In range 5..=95, so the cast cannot truncate.
        Ok(Self(percent as u8))
    }

    pub fn percent(self) -> u8 {
        self.0
    }
}

impl Default for CompactionPoint {
    fn default() -> Self {
        Self(COMPACTION_DEFAULT_PERCENT)
    }
}

/// Where the recommendation came from. Until a model has enough recorded runs it is
/// the unmeasured production default, and the slider says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recommendation {
    Unmeasured(CompactionPoint),
    Measured(CompactionPoint),
}

impl Recommendation {
    pub fn point(self) -> CompactionPoint {
        match self {
            Self::Unmeasured(p) | Self::Measured(p) => p,
        }
    }
}

impl Default for Recommendation {
    fn default() -> Self {
        Self::Unmeasured(CompactionPoint::default())
    }
}

/// The slider's state machine: it follows the recommendation until the user moves it,
/// then holds the user's value until the user returns it to the recommendation.
///
/// A user value equal to the current recommendation never pins: landing on the
/// recommendation (or touching the slider without moving it) is following it. Only a
/// move to a different value pins.
///
/// ```text
///   Following --user moves to v != rec--> Pinned(v)
///   Following --user moves to rec-------> Following
///   Pinned    --user moves to rec-------> Following
///   Pinned    --return------------------> Following
///   Following --recommendation updates--> Following (value tracks it)
///   Pinned    --recommendation updates--> Pinned (value unchanged)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompactionSlider {
    recommendation: Recommendation,
    pinned: Option<CompactionPoint>,
}

impl CompactionSlider {
    pub fn value(&self) -> CompactionPoint {
        self.pinned.unwrap_or(self.recommendation.point())
    }

    pub fn recommendation(&self) -> Recommendation {
        self.recommendation
    }

    pub fn follows_recommendation(&self) -> bool {
        self.pinned.is_none()
    }

    pub fn set_by_user(&mut self, point: CompactionPoint) {
        self.pinned = (point != self.recommendation.point()).then_some(point);
    }

    pub fn return_to_recommendation(&mut self) {
        self.pinned = None;
    }

    pub fn update_recommendation(&mut self, recommendation: Recommendation) {
        self.recommendation = recommendation;
    }

    /// The point a run takes from this slider: `None` while it follows the
    /// unmeasured default, which is the runtime's own default, so the run is
    /// labeled as running on it; otherwise the value shown.
    pub fn run_point(&self) -> Option<u8> {
        match (self.pinned, self.recommendation) {
            (None, Recommendation::Unmeasured(_)) => None,
            _ => Some(self.value().percent()),
        }
    }
}

/// The parallelism slider (decision 0067): one number, `max_agents`,
/// with a per-machine maximum computed elsewhere (`kgraf-capacity::compute`,
/// wired through the shell's re-measure thread) rather than the fixed
/// `MAX_LIVE_AGENTS_CAP` [`ConcurrencyCaps`] uses. Modeled after
/// [`CompactionSlider`]'s own state machine (follows the maximum until the
/// user moves it, holds where the user put it, returns to following on
/// request), with one addition [`CompactionSlider`] does not need: the
/// maximum can *fall* below a value the user pinned (a rate limit tightened,
/// a machine's free memory dropped), and [`ParallelismSlider::effective`]
/// is what a caller actually enforces in that case, distinct from
/// [`ParallelismSlider::value`] (what the slider control shows, so the
/// user's own choice is never silently rewritten out from under them).
///
/// ```text
///   Following --user pins v != max--> Pinned(v)
///   Following --user pins max------> Following
///   Pinned    --user pins max------> Following
///   Pinned    --return-------------> Following
///   Following --max updates--------> Following (value tracks it)
///   Pinned    --max updates--------> Pinned (value unchanged; effective may clamp)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParallelismSlider {
    max: u32,
    pinned: Option<u32>,
}

impl ParallelismSlider {
    /// A slider following `max` (not yet touched by the user).
    pub fn new(max: u32) -> Self {
        Self {
            max: max.max(1),
            pinned: None,
        }
    }

    /// What the slider control shows: the user's own choice if pinned,
    /// otherwise the current maximum. This is *not* clamped to the current
    /// maximum on its own (design: "the slider says so" when the maximum has
    /// fallen below it, which needs the un-clamped value to say anything
    /// meaningful); see [`ParallelismSlider::effective`] for the number a
    /// caller actually enforces.
    pub fn value(&self) -> u32 {
        self.pinned.unwrap_or(self.max)
    }

    /// The current per-machine maximum.
    pub fn max(&self) -> u32 {
        self.max
    }

    /// The number actually in force: `value()`, clamped down to `max()` when
    /// a pinned value now exceeds it (design: "If the maximum falls below
    /// the slider value, the effective limit is the maximum ... and running
    /// agents keep running", this method names what a fresh admission is
    /// bound by; it does not itself kill anything already running).
    pub fn effective(&self) -> u32 {
        self.value().min(self.max)
    }

    /// Whether the slider currently follows the maximum (never pinned, or
    /// last set to exactly the maximum: landing on the maximum is following,
    /// the same rule [`CompactionSlider::set_by_user`] uses).
    pub fn follows_max(&self) -> bool {
        self.pinned.is_none()
    }

    /// Whether `effective()` is below `value()` right now: the case the
    /// design says the slider must state, not clamp silently.
    pub fn is_clamped(&self) -> bool {
        self.pinned.is_some_and(|v| v > self.max)
    }

    /// The note to show next to the slider when it is clamped, or `None`
    /// when it is not (design: "the slider says so").
    pub fn clamp_note(&self) -> Option<String> {
        self.is_clamped().then(|| {
            format!(
                "Set to {}, but the machine's current maximum is {}; the effective limit is {} \
                 until the maximum rises or you lower the slider.",
                self.value(),
                self.max,
                self.effective()
            )
        })
    }

    /// Pin the slider to `v` (clamped to at least 1; not clamped to the
    /// current maximum, since a value above it is exactly the state
    /// `is_clamped`/`clamp_note` exist to report rather than silently
    /// prevent). Landing exactly on the maximum returns to following it.
    pub fn set_by_user(&mut self, v: u32) {
        let v = v.max(1);
        self.pinned = (v != self.max).then_some(v);
    }

    /// Stop following the maximum and return to it.
    pub fn return_to_max(&mut self) {
        self.pinned = None;
    }

    /// A newly measured maximum (`kgraf-capacity::compute`, through the
    /// shell's re-measure thread and its hysteresis). A pinned value is left
    /// alone; a following slider's `value()` tracks the new maximum at once.
    pub fn update_max(&mut self, max: u32) {
        self.max = max.max(1);
    }
}

impl Default for ParallelismSlider {
    /// Before the first measurement, the maximum is 0017's own starting
    /// ceiling (`DEFAULT_MAX_LIVE_AGENTS`): the same number `ConcurrencyCaps`
    /// already defaults to, so a settings file with neither the new
    /// `parallelism_agents` key nor a legacy `max_live_agents` key (a first
    /// run) behaves exactly as before this slider existed, until the shell's
    /// re-measure thread runs its first `kgraf-capacity::compute` and calls
    /// `update_max`.
    fn default() -> Self {
        Self::new(kgraf_orchestrator::DEFAULT_MAX_LIVE_AGENTS as u32)
    }
}

/// One run-limit slider: a value inside `min..=max`, moved in `step`s, with a
/// built-in default. Like [`CompactionSlider`], it follows its default until the
/// user moves it, and a followed slider writes no key to the settings file, so a
/// later change of the default reaches a user who never chose a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitSlider {
    min: u64,
    max: u64,
    step: u64,
    default: u64,
    chosen: Option<u64>,
    /// How the default is stated when it is not one fixed number (the answer
    /// wait follows the answering model); `None` states `default`.
    default_text: Option<&'static str>,
}

impl LimitSlider {
    /// Rounds per worker (decision 0017 addendum): 4 to 200, one at a time.
    pub fn rounds() -> Self {
        Self::new(
            u64::from(kgraf_host::limits::ROUNDS_BOUNDS.0),
            u64::from(kgraf_host::limits::ROUNDS_BOUNDS.1),
            1,
            u64::from(kgraf_host::limits::DEFAULT_ROUNDS_ORCHESTRATED),
        )
    }
    /// Progress checkpoints per worker (decision 0096): 1 to 50, one at a time.
    /// 1 is the old fixed round limit.
    pub fn max_checkpoints() -> Self {
        Self::new(
            u64::from(kgraf_host::limits::MAX_CHECKPOINTS_BOUNDS.0),
            u64::from(kgraf_host::limits::MAX_CHECKPOINTS_BOUNDS.1),
            1,
            u64::from(kgraf_host::limits::DEFAULT_MAX_CHECKPOINTS),
        )
    }
    /// Worker output cap per reply: 1,024 to 131,072 in steps of 1,024.
    pub fn worker_max_tokens() -> Self {
        Self::new(
            u64::from(kgraf_host::limits::WORKER_MAX_TOKENS_BOUNDS.0),
            u64::from(kgraf_host::limits::WORKER_MAX_TOKENS_BOUNDS.1),
            1_024,
            u64::from(kgraf_host::limits::DEFAULT_WORKER_MAX_TOKENS),
        )
    }
    /// Orchestrator output cap per call: 4,096 to 131,072 in steps of 1,024.
    pub fn planner_max_tokens() -> Self {
        Self::new(
            u64::from(kgraf_host::limits::PLANNER_MAX_TOKENS_BOUNDS.0),
            u64::from(kgraf_host::limits::PLANNER_MAX_TOKENS_BOUNDS.1),
            1_024,
            u64::from(kgraf_host::limits::DEFAULT_PLANNER_MAX_TOKENS),
        )
    }
    /// Assumed context window: 8,192 to 1,000,000 in steps of 1,024.
    pub fn window() -> Self {
        Self::new(
            kgraf_host::limits::WINDOW_BOUNDS.0,
            kgraf_host::limits::WINDOW_BOUNDS.1,
            1_024,
            kgraf_host::limits::DEFAULT_WINDOW,
        )
    }

    /// The wait, in seconds, for the orchestrator's model to answer a
    /// worker's question (decision 0067): 30 to 1,800 in steps of 30. While
    /// it follows its default, no wait is set and each call derives its own
    /// from the answering model's recent latencies; the slider then rests at
    /// the stated fallback of 300 s.
    pub fn answer_wait() -> Self {
        Self {
            default_text: Some(
                "measured from the answering model's recent answers (300 s until it has \
                 enough)",
            ),
            ..Self::new(
                u64::from(kgraf_host::limits::ANSWER_WAIT_BOUNDS.0),
                u64::from(kgraf_host::limits::ANSWER_WAIT_BOUNDS.1),
                30,
                kgraf_orchestrator::answer_wait::DEFAULT.as_secs(),
            )
        }
    }

    fn new(min: u64, max: u64, step: u64, default: u64) -> Self {
        Self {
            min,
            max,
            step,
            default,
            chosen: None,
            default_text: None,
        }
    }

    /// The default as the control states it, when it is not one number.
    pub fn default_text(&self) -> Option<&'static str> {
        self.default_text
    }

    pub fn min(&self) -> u64 {
        self.min
    }
    pub fn max(&self) -> u64 {
        self.max
    }
    pub fn step(&self) -> u64 {
        self.step
    }
    pub fn default_value(&self) -> u64 {
        self.default
    }
    /// What the control shows and what a run uses.
    pub fn value(&self) -> u64 {
        self.chosen.unwrap_or(self.default)
    }
    /// The user's choice, if any: what is written to the settings file.
    pub fn chosen(&self) -> Option<u64> {
        self.chosen
    }
    pub fn follows_default(&self) -> bool {
        self.chosen.is_none()
    }

    /// `v` moved to the nearest step and inside the bounds. The ends are always
    /// reachable even when they are not a multiple of the step.
    pub fn snap(&self, v: u64) -> u64 {
        let stepped = (v.saturating_add(self.step / 2) / self.step) * self.step;
        stepped.clamp(self.min, self.max)
    }

    /// The user moved the slider to `v` (snapped). Returns the stored value.
    pub fn set_by_user(&mut self, v: u64) -> u64 {
        let v = self.snap(v);
        self.chosen = Some(v);
        v
    }

    /// A value read from the file: kept as written (already inside the bounds),
    /// so a hand-edited 5,000 is what runs, not the nearest step.
    fn load_exact(&mut self, v: u64) {
        self.chosen = Some(v.clamp(self.min, self.max));
    }

    /// Back to following the default.
    pub fn reset(&mut self) {
        self.chosen = None;
    }
}

/// The five run-limit sliders, saved as the user's global settings. They apply
/// to every run the app starts, and to the command line when no flag is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimitSliders {
    pub rounds: LimitSlider,
    pub max_checkpoints: LimitSlider,
    pub worker_max_tokens: LimitSlider,
    pub planner_max_tokens: LimitSlider,
    pub window: LimitSlider,
    /// The wait for the orchestrator's model to answer a worker's question.
    pub answer_wait: LimitSlider,
    /// Warn at run start when your verify or gate command runs a program your
    /// deny or ask rules refuse to workers. Off by default; not a slider, but it
    /// rides in the same settings file and shared value as the limits.
    pub warn_rule_conflicts: bool,
}

impl Default for RunLimitSliders {
    fn default() -> Self {
        Self {
            rounds: LimitSlider::rounds(),
            max_checkpoints: LimitSlider::max_checkpoints(),
            worker_max_tokens: LimitSlider::worker_max_tokens(),
            planner_max_tokens: LimitSlider::planner_max_tokens(),
            window: LimitSlider::window(),
            answer_wait: LimitSlider::answer_wait(),
            warn_rule_conflicts: kgraf_host::limits::DEFAULT_WARN_RULE_CONFLICTS,
        }
    }
}

impl RunLimitSliders {
    /// The chosen values, as the host and the command line read them.
    pub fn user_limits(&self) -> kgraf_host::limits::UserLimits {
        kgraf_host::limits::UserLimits {
            rounds: self.rounds.chosen().map(|v| v as u32),
            max_checkpoints: self.max_checkpoints.chosen().map(|v| v as u32),
            worker_max_tokens: self.worker_max_tokens.chosen().map(|v| v as u32),
            planner_max_tokens: self.planner_max_tokens.chosen().map(|v| v as u32),
            window: self.window.chosen(),
            warn_rule_conflicts: self.warn_rule_conflicts.then_some(true),
            // Not one of these sliders: `Settings::run_limits` adds it.
            compaction_point: None,
            answer_wait_secs: self.answer_wait.chosen().map(|v| v as u32),
            // Not one of these sliders: `Settings::run_limits` adds it.
            reasoning: kgraf_provider::ReasoningLevels::new(),
            plan_mode: None,
            early_start: None,
        }
    }

    fn from_user_limits(u: kgraf_host::limits::UserLimits) -> Self {
        let mut s = Self::default();
        if let Some(v) = u.rounds {
            s.rounds.load_exact(u64::from(v));
        }
        if let Some(v) = u.max_checkpoints {
            s.max_checkpoints.load_exact(u64::from(v));
        }
        if let Some(v) = u.worker_max_tokens {
            s.worker_max_tokens.load_exact(u64::from(v));
        }
        if let Some(v) = u.planner_max_tokens {
            s.planner_max_tokens.load_exact(u64::from(v));
        }
        if let Some(v) = u.window {
            s.window.load_exact(v);
        }
        if let Some(v) = u.answer_wait_secs {
            s.answer_wait.load_exact(u64::from(v));
        }
        s.warn_rule_conflicts = u.warn_rule_conflicts.unwrap_or(s.warn_rule_conflicts);
        s
    }
}

/// Highest work-item cap the slider offers. 0017's starting band is three to five;
/// sixteen leaves room to measure past it without inviting an unbounded fan-out.
pub const MAX_WORK_ITEMS_CAP: usize = 16;
/// Highest live-agent ceiling the slider offers.
pub const MAX_LIVE_AGENTS_CAP: usize = 64;

/// The two limits of decision 0017, as configuration: concurrent work items (the
/// user-facing cap) and total live agents (the hard ceiling). Construction is the
/// only check, so a value of this type is always in range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyCaps {
    work_items: usize,
    live_agents: usize,
}

impl ConcurrencyCaps {
    pub fn new(work_items: i64, live_agents: i64) -> Result<Self, ShellError> {
        let check = |what, v: i64, max: usize| {
            usize::try_from(v)
                .ok()
                .filter(|&n| (1..=max).contains(&n))
                .ok_or(ShellError::CapOutOfRange {
                    what,
                    value: v,
                    max,
                })
        };
        Ok(Self {
            work_items: check("max_work_items", work_items, MAX_WORK_ITEMS_CAP)?,
            live_agents: check("max_live_agents", live_agents, MAX_LIVE_AGENTS_CAP)?,
        })
    }

    pub fn work_items(self) -> usize {
        self.work_items
    }

    pub fn live_agents(self) -> usize {
        self.live_agents
    }

    /// The orchestrator's limits. In range by construction, so never zero.
    pub fn limits(self) -> Result<kgraf_orchestrator::Limits, ShellError> {
        kgraf_orchestrator::Limits::new(self.work_items, self.live_agents)
            .map_err(|e| ShellError::Config(e.to_string()))
    }
}

impl Default for ConcurrencyCaps {
    /// The orchestrator's starting defaults (0017: a starting point, not researched).
    fn default() -> Self {
        Self {
            work_items: kgraf_orchestrator::DEFAULT_MAX_WORK_ITEMS,
            live_agents: kgraf_orchestrator::DEFAULT_MAX_LIVE_AGENTS,
        }
    }
}

/// The window's color theme (THM-1). The stage
/// stays near-black (`--stage-bg`) in every theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follow the operating system's light or dark setting.
    Device,
    #[default]
    Dark,
    Light,
}

/// All shell configuration.
///
/// The open projects (decision 0066) are configuration: which folders the window
/// shows, like `--project`. Nothing about their work state is kept here (0017).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub compaction: CompactionSlider,
    /// Closing the window leaves the process in the tray (0052).
    pub background_mode: bool,
    /// Discrete GPU first instead of integrated first. Read at launch, when the
    /// adapter is chosen.
    pub prefer_high_performance_gpu: bool,
    /// Retired as a user-facing control (2026-09-28, item 5;
    /// design consequence, "The work-item cap as a user control is gone").
    /// Kept only so a settings file this build saves round-trips through the
    /// legacy `max_work_items`/`max_live_agents` keys in a build that
    /// predates the slider (`to_toml_string`); neither `cap_controls` nor
    /// `apply_caps` (both removed) reads or writes it from the UI anymore.
    /// [`Settings::parallelism`] is the sole source of the live-agent
    /// ceiling every open project's admission actually enforces.
    pub caps: ConcurrencyCaps,
    /// The parallelism slider (decision 0067): one number, with a
    /// per-machine maximum. The sole source of the live-agent ceiling every
    /// open project's admission enforces (`kgraf-shell::layout`'s
    /// `sync_parallelism` wiring); its own migration and clamp behavior are
    /// complete and tested.
    pub parallelism: ParallelismSlider,
    /// Rounds per worker, the two output caps and the context window: global
    /// run limits in the user-level settings file (never a project file a worker
    /// could write). The command line reads the same keys; a flag outranks them.
    pub limits: RunLimitSliders,
    /// Set by [`Settings::from_toml_str`] when a legacy `max_work_items` or
    /// `max_live_agents` key drove [`Settings::parallelism`]'s initial pin
    /// (design: "A warning names the migration"). Never persisted (not a
    /// `SettingsFile` field): it is a one-time load-time notice, not stored
    /// state.
    pub migration_warning: Option<String>,
    /// Light, dark, or the device's theme.
    pub appearance: Appearance,
    /// Standard color vision or one mode of color blindness (decision 0070). Remaps the five
    /// semantic colors only; it lives in `kgraf_core::palette` and does not touch the style guide's
    /// one palette.
    pub color_vision: kgraf_core::palette::Vision,
    /// No blinking or wiggling indicators. egui cannot read the operating system's
    /// reduced-motion setting, so the user sets it here.
    pub reduce_motion: bool,
    /// The guide is expanded to its sidebar (it stays open until closed).
    pub guide_open: bool,
    /// The project folders open in the window, in the switcher's order.
    pub projects: Vec<PathBuf>,
    /// The project shown when the window opens, if it is one of `projects`.
    pub active_project: Option<PathBuf>,
    /// Check GitHub Releases for a newer kGraf on start.
    /// **Off by default.** Decision 0051 permits egress only to the
    /// model providers and MCP servers the user configured; a GitHub Releases query
    /// is neither, so it is opt-in here rather than a third standing destination
    /// (see `crate::update_check`'s module doc for the full reasoning). "Check now"
    /// in Settings runs regardless of this switch, since the user just asked.
    pub check_for_updates: bool,
    /// Offer a prerelease as an update too, not only a full release. Off by default.
    pub allow_prerelease_updates: bool,
    /// Delete a worker's worktree, with the build output in it, once its attempt is
    /// merged or discarded and nothing needs it (decision 0088). **On by default**: a
    /// Rust `target/` folder can be several gigabytes for each worktree. Off keeps
    /// every worktree and its builds for the user to inspect or reuse. A worktree a
    /// live worker, a stopped run or a pending merge needs is kept either way.
    pub delete_worker_worktrees: bool,
    /// The hosts each package manager's sandboxed install may reach
    /// (decision 0087, amended 2026-10-07): the built-in lists unless the
    /// user changed one. Only a changed list is written to the file; the
    /// host reads the file when an install is about to run.
    pub install_registries: kgraf_host::deps::InstallHosts,
    /// The reasoning level of each model call (decision 0098): the
    /// `[reasoning]` table, read with the host's reader and written back
    /// unchanged. No control shows it yet; every run the app starts takes it.
    pub reasoning: kgraf_provider::ReasoningLevels,
    /// Size a request before planning it (decision 0100): a small request
    /// runs as one contract with no planning call. Off plans every request
    /// in full; off by default.
    pub plan_auto: bool,
    /// Start a worker before the triage call answers, when it can start at
    /// once (decision 0100; on by default). Only has an effect with
    /// `plan_auto`.
    pub early_start: bool,
}

impl Settings {
    /// The switch as the host takes it.
    pub fn worktree_retention(&self) -> kgraf_host::WorktreeRetention {
        kgraf_host::WorktreeRetention::from_delete(self.delete_worker_worktrees)
    }

    /// What every run the app starts takes from Settings: the run-limit
    /// sliders, the rule-conflict switch and the compaction point.
    pub fn run_limits(&self) -> kgraf_host::limits::UserLimits {
        kgraf_host::limits::UserLimits {
            compaction_point: self.compaction.run_point(),
            reasoning: self.reasoning,
            plan_mode: self
                .plan_auto
                .then_some(kgraf_orchestrator::triage::PlanMode::Auto),
            // Only with small requests on: the setting has no effect
            // otherwise, and a run file or a flag decides with it unset.
            early_start: self.plan_auto.then_some(self.early_start),
            ..self.limits.user_limits()
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            compaction: CompactionSlider::default(),
            background_mode: true,
            prefer_high_performance_gpu: false,
            caps: ConcurrencyCaps::default(),
            parallelism: ParallelismSlider::default(),
            limits: RunLimitSliders::default(),
            migration_warning: None,
            appearance: Appearance::default(),
            color_vision: kgraf_core::palette::Vision::default(),
            reduce_motion: false,
            guide_open: false,
            projects: Vec::new(),
            active_project: None,
            check_for_updates: false,
            allow_prerelease_updates: false,
            delete_worker_worktrees: true,
            install_registries: kgraf_host::deps::InstallHosts::defaults(),
            reasoning: kgraf_provider::ReasoningLevels::new(),
            plan_auto: false,
            early_start: true,
        }
    }
}

/// On-disk form. Unknown keys are rejected so a misspelled key fails loudly instead of
/// being ignored.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    /// Absent means "follow the recommendation".
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_percent: Option<i64>,
    background_mode: Option<bool>,
    prefer_high_performance_gpu: Option<bool>,
    /// Legacy (decision 0017, before 0067): read once, then migrated onto
    /// `parallelism_agents` if that key is absent. Still written back too
    /// (see `to_toml_string`), so a settings file this build saves still
    /// loads correctly in a build that predates the slider.
    max_work_items: Option<i64>,
    /// Legacy: see `max_work_items`'s doc comment. `Settings::parallelism`'s
    /// initial pin, once, when `parallelism_agents` is absent (design:
    /// "`max_live_agents` becomes the slider value if it is in range").
    max_live_agents: Option<i64>,
    /// The parallelism slider's pinned value (decision 0067). Absent
    /// means "follow the machine's current maximum", exactly as
    /// `compaction_percent`'s absence means "follow the recommendation".
    #[serde(skip_serializing_if = "Option::is_none")]
    parallelism_agents: Option<i64>,
    /// `true` on every file this build (0067+) writes, `false` (the serde
    /// default) on a file from before this key existed. This is the only
    /// reliable way to tell "a file the parallelism slider does not know
    /// about yet" apart from "a file where the slider is simply following
    /// the maximum, which also omits `parallelism_agents`": both leave
    /// `parallelism_agents` absent, so absence alone cannot distinguish a
    /// genuinely legacy file from an ordinary, modern, unpinned one.
    #[serde(default)]
    parallelism_managed: bool,
    /// The run limits (`kgraf_host::limits`); absent means the built-in default.
    #[serde(skip_serializing_if = "Option::is_none")]
    rounds_per_worker: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_checkpoints: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    planner_max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<i64>,
    /// Written only when on; absent means off.
    #[serde(skip_serializing_if = "Option::is_none")]
    warn_rule_conflicts: Option<bool>,
    /// Absent means the wait is measured per call.
    #[serde(skip_serializing_if = "Option::is_none")]
    answer_wait_secs: Option<i64>,
    appearance: Option<Appearance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color_vision: Option<kgraf_core::palette::Vision>,
    reduce_motion: Option<bool>,
    guide_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    projects: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_project: Option<PathBuf>,
    /// Absent (an older settings file) means off, the
    /// same default a fresh install gets.
    check_for_updates: Option<bool>,
    allow_prerelease_updates: Option<bool>,
    /// Absent (an older settings file) means on, the default (decision 0088).
    delete_worker_worktrees: Option<bool>,
    /// The changed install host lists, by package manager; one left out is
    /// the built-in list. Read with the host's own reader
    /// (`kgraf_host::deps::InstallHosts::from_toml_str`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    install_registries: std::collections::BTreeMap<String, Vec<String>>,
    /// The reasoning setting (decision 0098); absent means none is set.
    #[serde(
        default,
        skip_serializing_if = "kgraf_provider::ReasoningLevels::is_empty"
    )]
    reasoning: kgraf_provider::ReasoningLevels,
    /// `auto` or `full` (decision 0100); written only for `auto`, so absent
    /// is `full`. Read with the host's reader.
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_mode: Option<String>,
    /// Absent (an older file, or small requests off) means on, the
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    early_start: Option<bool>,
}

impl Settings {
    /// Parses a TOML settings file. An out-of-range compaction point is an error.
    pub fn from_toml_str(text: &str) -> Result<Self, ShellError> {
        let file: SettingsFile =
            toml::from_str(text).map_err(|e| ShellError::Config(e.to_string()))?;
        let mut settings = Self::default();
        if let Some(p) = file.compaction_percent {
            settings.compaction.set_by_user(CompactionPoint::new(p)?);
        }
        if let Some(b) = file.background_mode {
            settings.background_mode = b;
        }
        if let Some(b) = file.prefer_high_performance_gpu {
            settings.prefer_high_performance_gpu = b;
        }
        let d = ConcurrencyCaps::default();
        settings.caps = ConcurrencyCaps::new(
            file.max_work_items.unwrap_or(d.work_items as i64),
            file.max_live_agents.unwrap_or(d.live_agents as i64),
        )?;
        // The parallelism slider (decision 0067).
        //
        // A file this build wrote (`parallelism_managed = true`) uses the
        // modern `parallelism_agents` key alone: absent means following the
        // maximum, exactly like `compaction_percent`.
        //
        // A file from before this key existed (`parallelism_managed` absent,
        // the serde default `false`) migrates its `max_live_agents` into the
        // slider's initial pin once, with a warning naming the migration
        // (design: "Old settings files with `max_work_items` or
        // `max_live_agents` are read once ... A warning names the
        // migration"). "In range" here means positive: an old file's value
        // above today's still-unmeasured default maximum is not rejected,
        // since `ParallelismSlider::effective` and `clamp_note` exist
        // precisely to state that case rather than silently reject or
        // silently truncate it.
        if file.parallelism_managed {
            match file.parallelism_agents {
                Some(n) if n > 0 => settings.parallelism.set_by_user(n as u32),
                Some(n) => {
                    return Err(ShellError::CapOutOfRange {
                        what: "parallelism_agents",
                        value: n,
                        max: u32::MAX as usize,
                    });
                }
                None => {}
            }
        } else if let Some(n) = file.max_live_agents.filter(|n| *n > 0) {
            settings.parallelism.set_by_user(n as u32);
            settings.migration_warning = Some(format!(
                "`max_live_agents = {n}` (and `max_work_items`, if present) is a legacy \
                 setting; the parallelism slider (decision 0067) replaces it, set here to {n} \
                 from the migrated value."
            ));
        }
        // The same reader the command line uses, so an out-of-range value is
        // refused in both places with the same words.
        let user = kgraf_host::limits::UserLimits::from_toml_str(text)
            .map_err(|e| ShellError::Config(e.to_string()))?;
        settings.reasoning = user.reasoning;
        settings.plan_auto = user.plan_mode == Some(kgraf_orchestrator::triage::PlanMode::Auto);
        settings.early_start = user.early_start.unwrap_or(settings.early_start);
        settings.limits = RunLimitSliders::from_user_limits(user);
        if let Some(a) = file.appearance {
            settings.appearance = a;
        }
        if let Some(v) = file.color_vision {
            settings.color_vision = v;
        }
        if let Some(b) = file.reduce_motion {
            settings.reduce_motion = b;
        }
        if let Some(b) = file.guide_open {
            settings.guide_open = b;
        }
        // A folder listed twice would open two hosts over one `.kgraf`; the
        // second entry is dropped.
        for p in file.projects {
            if !settings.projects.contains(&p) {
                settings.projects.push(p);
            }
        }
        settings.active_project = file
            .active_project
            .filter(|a| settings.projects.contains(a));
        if let Some(b) = file.check_for_updates {
            settings.check_for_updates = b;
        }
        if let Some(b) = file.allow_prerelease_updates {
            settings.allow_prerelease_updates = b;
        }
        if let Some(b) = file.delete_worker_worktrees {
            settings.delete_worker_worktrees = b;
        }
        // The same reader the host uses at install time, so a bad entry is
        // refused in both places with the same words.
        settings.install_registries =
            kgraf_host::deps::InstallHosts::from_toml_str(text).map_err(ShellError::Config)?;
        Ok(settings)
    }

    /// Serializes to TOML. A slider that follows the recommendation writes no
    /// `compaction_percent`, so it keeps following after a reload.
    pub fn to_toml_string(&self) -> Result<String, ShellError> {
        let file = SettingsFile {
            compaction_percent: (!self.compaction.follows_recommendation())
                .then(|| i64::from(self.compaction.value().percent())),
            background_mode: Some(self.background_mode),
            prefer_high_performance_gpu: Some(self.prefer_high_performance_gpu),
            max_work_items: Some(self.caps.work_items() as i64),
            max_live_agents: Some(self.caps.live_agents() as i64),
            parallelism_agents: (!self.parallelism.follows_max())
                .then(|| i64::from(self.parallelism.value())),
            parallelism_managed: true,
            rounds_per_worker: self.limits.rounds.chosen().map(|v| v as i64),
            max_checkpoints: self.limits.max_checkpoints.chosen().map(|v| v as i64),
            worker_max_tokens: self.limits.worker_max_tokens.chosen().map(|v| v as i64),
            planner_max_tokens: self.limits.planner_max_tokens.chosen().map(|v| v as i64),
            context_window: self.limits.window.chosen().map(|v| v as i64),
            warn_rule_conflicts: self.limits.warn_rule_conflicts.then_some(true),
            answer_wait_secs: self.limits.answer_wait.chosen().map(|v| v as i64),
            appearance: Some(self.appearance),
            color_vision: (self.color_vision != kgraf_core::palette::Vision::default())
                .then_some(self.color_vision),
            reduce_motion: Some(self.reduce_motion),
            guide_open: Some(self.guide_open),
            projects: self.projects.clone(),
            active_project: self.active_project.clone(),
            check_for_updates: Some(self.check_for_updates),
            allow_prerelease_updates: Some(self.allow_prerelease_updates),
            delete_worker_worktrees: Some(self.delete_worker_worktrees),
            install_registries: kgraf_host::deps::Ecosystem::ALL
                .iter()
                .filter(|e| !self.install_registries.is_default(**e))
                .map(|e| {
                    (
                        e.key().to_owned(),
                        self.install_registries
                            .for_ecosystem(*e)
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                    )
                })
                .collect(),
            reasoning: self.reasoning,
            plan_mode: self.plan_auto.then(|| "auto".to_owned()),
            // Written when it applies or differs from the default, so a file
            // with neither keeps the command line's own default (off).
            early_start: (self.plan_auto || !self.early_start).then_some(self.early_start),
        };
        toml::to_string(&file).map_err(|e| ShellError::Config(e.to_string()))
    }

    /// Loads from `path`. A missing file is the defaults (first run); any other read
    /// or parse failure is an error.
    pub fn load(path: &Path) -> Result<Self, ShellError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ShellError::Io {
                path: path.to_owned(),
                source: e,
            }),
        }
    }

    /// Writes to `path` atomically: a temporary file in the same directory, flushed to
    /// disk, then renamed over the target. A crash mid-write leaves the old file whole.
    pub fn save(&self, path: &Path) -> Result<(), ShellError> {
        let text = self.to_toml_string()?;
        let io = |source| ShellError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        // A name of this process and this write alone: two windows (two
        // processes) saving at once never write into one temporary file.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(format!(".{}.{n}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp_name);
        {
            let mut f = std::fs::File::create(&tmp).map_err(io)?;
            f.write_all(text.as_bytes()).map_err(io)?;
            f.sync_all().map_err(io)?;
        }
        std::fs::rename(&tmp, path).map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_inclusive_and_outside_is_an_error() {
        assert_eq!(CompactionPoint::new(5).expect("min").percent(), 5);
        assert_eq!(CompactionPoint::new(95).expect("max").percent(), 95);
        for bad in [-1, 0, 4, 96, 100, 1000] {
            let err = CompactionPoint::new(bad).expect_err("out of range");
            assert!(err.to_string().contains(&bad.to_string()));
        }
    }

    #[test]
    fn default_is_80_and_unmeasured() {
        let s = CompactionSlider::default();
        assert_eq!(s.value().percent(), 80);
        assert!(matches!(s.recommendation(), Recommendation::Unmeasured(_)));
        assert!(s.follows_recommendation());
    }

    #[test]
    fn follows_recommendation_until_moved_then_holds() {
        let mut s = CompactionSlider::default();
        let m = |p| CompactionPoint::new(p).expect("valid");
        s.update_recommendation(Recommendation::Measured(m(70)));
        assert_eq!(s.value(), m(70));

        s.set_by_user(m(50));
        s.update_recommendation(Recommendation::Measured(m(60)));
        assert_eq!(
            s.value(),
            m(50),
            "a moved slider stays where the user put it"
        );

        s.return_to_recommendation();
        assert_eq!(s.value(), m(60));
        assert!(s.follows_recommendation());
    }

    #[test]
    fn moving_onto_the_recommendation_does_not_pin() {
        let mut s = CompactionSlider::default();
        s.set_by_user(CompactionPoint::new(80).expect("valid"));
        assert!(s.follows_recommendation(), "landing on 80 = following");

        s.set_by_user(CompactionPoint::new(60).expect("valid"));
        assert!(!s.follows_recommendation());
        s.set_by_user(CompactionPoint::new(80).expect("valid"));
        assert!(s.follows_recommendation(), "moving back onto it unpins");

        // Still following: a later measurement moves the value.
        s.update_recommendation(Recommendation::Measured(
            CompactionPoint::new(70).expect("valid"),
        ));
        assert_eq!(s.value().percent(), 70);
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("settings.toml");

        let mut s = Settings::default();
        s.compaction
            .set_by_user(CompactionPoint::new(55).expect("valid"));
        s.background_mode = false;
        s.prefer_high_performance_gpu = true;
        s.save(&path).expect("save");
        let leftovers = std::fs::read_dir(dir.path().join("nested"))
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert!(leftovers == 0, "temp file renamed away");
        assert_eq!(Settings::load(&path).expect("load"), s);

        // Return to the recommendation, save, reload: the slider follows again.
        s.compaction.return_to_recommendation();
        s.save(&path).expect("save");
        let back = Settings::load(&path).expect("load");
        assert!(back.compaction.follows_recommendation());
        assert_eq!(back, s);
    }

    /// A non-default color vision round-trips
    /// through the file; the default (`Standard`) is never written, so an
    /// older build reading a file this one wrote sees no unfamiliar
    /// `color_vision` key unless the mode was actually changed.
    #[test]
    fn color_vision_round_trips_and_the_default_is_not_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut s = Settings::default();
        assert_eq!(s.color_vision, kgraf_core::palette::Vision::Standard);
        s.save(&path).expect("save");
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("color_vision"),
            "the default is not written"
        );
        assert_eq!(Settings::load(&path).expect("load"), s);

        s.color_vision = kgraf_core::palette::Vision::Protanopia;
        s.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("color_vision = \"protanopia\""),
            "a chosen mode is written: {text}"
        );
        let loaded = Settings::load(&path).expect("load");
        assert_eq!(loaded.color_vision, kgraf_core::palette::Vision::Protanopia);
        assert_eq!(loaded, s);

        // Moving back to standard drops the key again.
        s.color_vision = kgraf_core::palette::Vision::Standard;
        s.save(&path).expect("save");
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("color_vision"),
            "back to the default, the key is gone again"
        );
    }

    #[test]
    fn missing_file_is_defaults_but_unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent.toml");
        assert_eq!(
            Settings::load(&missing).expect("defaults"),
            Settings::default()
        );
        // A directory where the file should be cannot be read as a file.
        assert!(Settings::load(dir.path()).is_err());
    }

    #[test]
    fn config_file_out_of_range_is_an_error() {
        let err = Settings::from_toml_str("compaction_percent = 99").expect_err("reject");
        assert!(matches!(
            err,
            ShellError::CompactionOutOfRange { percent: 99 }
        ));
        assert!(Settings::from_toml_str("compaction_percent = 2").is_err());
    }

    #[test]
    fn config_file_in_range_pins_the_slider() {
        let s = Settings::from_toml_str("compaction_percent = 65\nbackground_mode = false")
            .expect("valid");
        assert_eq!(s.compaction.value().percent(), 65);
        assert!(!s.compaction.follows_recommendation());
        assert!(!s.background_mode);
    }

    #[test]
    fn empty_config_is_defaults() {
        assert_eq!(
            Settings::from_toml_str("").expect("valid"),
            Settings::default()
        );
    }

    #[test]
    fn unknown_key_is_an_error() {
        let err = Settings::from_toml_str("compaction_pct = 50").expect_err("reject");
        assert!(matches!(err, ShellError::Config(_)));
    }

    #[test]
    fn caps_default_to_the_orchestrators_and_outside_the_range_is_an_error() {
        let s = Settings::from_toml_str("").expect("valid");
        assert_eq!(
            s.caps.work_items(),
            kgraf_orchestrator::DEFAULT_MAX_WORK_ITEMS
        );
        let s = Settings::from_toml_str("max_work_items = 2\nmax_live_agents = 5").expect("valid");
        assert_eq!((s.caps.work_items(), s.caps.live_agents()), (2, 5));
        for bad in [
            "max_work_items = 0",
            "max_work_items = 17",
            "max_live_agents = -1",
        ] {
            assert!(
                matches!(
                    Settings::from_toml_str(bad),
                    Err(ShellError::CapOutOfRange { .. })
                ),
                "{bad}"
            );
        }
    }

    /// Decision 0066: the open projects and the active one survive a restart,
    /// through the file, in order; a duplicate is dropped, and an active project
    /// that is not in the list is not restored.
    #[test]
    fn open_projects_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let s = Settings {
            projects: vec![PathBuf::from("/a/one"), PathBuf::from("/b/two")],
            active_project: Some(PathBuf::from("/b/two")),
            guide_open: true,
            appearance: Appearance::Light,
            reduce_motion: true,
            ..Settings::default()
        };
        s.save(&path).expect("save");
        assert_eq!(Settings::load(&path).expect("load"), s);

        let s =
            Settings::from_toml_str("projects = [\"/a\", \"/b\", \"/a\"]\nactive_project = \"/c\"")
                .expect("valid");
        assert_eq!(s.projects, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(s.active_project, None, "not an open project");
        assert!(Settings::from_toml_str("appearance = \"sepia\"").is_err());
    }

    #[test]
    fn non_integer_percent_is_an_error() {
        assert!(Settings::from_toml_str("compaction_percent = 50.5").is_err());
    }

    // --- ParallelismSlider (decision 0067) ---

    #[test]
    fn the_slider_follows_the_maximum_until_the_user_moves_it() {
        let mut s = ParallelismSlider::new(8);
        assert_eq!(s.value(), 8);
        assert!(s.follows_max());
        s.update_max(12);
        assert_eq!(s.value(), 12, "a following slider tracks a new maximum");

        s.set_by_user(6);
        assert!(!s.follows_max());
        s.update_max(20);
        assert_eq!(s.value(), 6, "a pinned slider holds where the user put it");

        s.return_to_max();
        assert!(s.follows_max());
        assert_eq!(s.value(), 20);
    }

    #[test]
    fn landing_exactly_on_the_maximum_returns_to_following() {
        let mut s = ParallelismSlider::new(8);
        s.set_by_user(3);
        assert!(!s.follows_max());
        s.set_by_user(8);
        assert!(
            s.follows_max(),
            "landing on the maximum is following, like the compaction slider"
        );
    }

    /// "If the maximum falls below the slider value, the effective limit is
    /// the maximum, the slider says so, and running agents keep running"
    /// (design). This unit only states the fact (`effective`/`clamp_note`);
    /// "running agents keep running" is `AdmissionGate`/`MachineGate`'s own
    /// pre-flight-only behavior, tested in `kgraf-orchestrator::admission`.
    #[test]
    fn a_maximum_that_falls_below_a_pinned_value_clamps_the_effective_limit_and_says_so() {
        let mut s = ParallelismSlider::new(20);
        s.set_by_user(20); // still following at this point (see the test above)
        s.set_by_user(15);
        assert_eq!((s.value(), s.effective()), (15, 15));
        assert!(s.clamp_note().is_none());

        s.update_max(8);
        assert_eq!(
            s.value(),
            15,
            "the user's own choice is not silently rewritten"
        );
        assert_eq!(
            s.effective(),
            8,
            "but the number actually enforced is the new maximum"
        );
        assert!(s.is_clamped());
        let note = s.clamp_note().expect("stated, not hidden");
        assert!(note.contains('8') && note.contains("15"), "{note}");

        s.update_max(30);
        assert!(
            !s.is_clamped(),
            "raising the maximum past the pin un-clamps it"
        );
        assert_eq!(s.effective(), 15);
    }

    /// Design: "Old settings files with `max_work_items` or `max_live_agents`
    /// are read once ... `max_live_agents` becomes the slider value if it is
    /// in range. A warning names the migration."
    #[test]
    fn an_old_file_with_max_live_agents_migrates_the_slider_and_warns() {
        let s = Settings::from_toml_str("max_work_items = 4\nmax_live_agents = 12").expect("valid");
        assert_eq!(
            s.parallelism.value(),
            12,
            "the slider is set to the migrated value"
        );
        assert!(!s.parallelism.follows_max());
        let warning = s.migration_warning.expect("a warning names the migration");
        assert!(warning.contains("max_live_agents"), "{warning}");
        assert!(warning.contains("12"), "{warning}");
    }

    /// A file this build wrote never re-triggers the legacy migration on a
    /// later load, even though it still carries `max_live_agents` for
    /// `ConcurrencyCaps`'s own persistence: `parallelism_managed` is the
    /// signal, not mere key presence (see the field's own doc comment for
    /// why presence alone cannot distinguish the two cases).
    #[test]
    fn a_modern_saved_file_does_not_re_trigger_the_legacy_migration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let mut s = Settings::default();
        s.parallelism.set_by_user(5);
        s.save(&path).expect("save");

        let reloaded = Settings::load(&path).expect("load");
        assert_eq!(reloaded.parallelism.value(), 5);
        assert!(
            reloaded.migration_warning.is_none(),
            "a modern file must never produce a migration warning"
        );

        // A *following* slider (no pin) also round-trips without spuriously
        // migrating, even though `max_live_agents` is still written for
        // `ConcurrencyCaps`.
        let mut s2 = Settings::default();
        s2.parallelism.return_to_max();
        s2.save(&path).expect("save");
        let reloaded2 = Settings::load(&path).expect("load");
        assert!(reloaded2.parallelism.follows_max());
        assert!(reloaded2.migration_warning.is_none());
    }

    /// A `parallelism_agents` key wins over any legacy key, with no warning:
    /// the modern path.
    #[test]
    fn the_modern_key_wins_over_a_legacy_key_present_in_the_same_file() {
        let s = Settings::from_toml_str(
            "max_live_agents = 12\nparallelism_agents = 30\nparallelism_managed = true",
        )
        .expect("valid");
        assert_eq!(s.parallelism.value(), 30);
        assert!(s.migration_warning.is_none());
    }

    /// Off by default (a fresh settings file, and an old
    /// one written before this key existed, both read as off), and a chosen value
    /// round-trips.
    #[test]
    fn update_check_settings_default_off_and_round_trip() {
        let s = Settings::from_toml_str("").expect("valid");
        assert!(!s.check_for_updates);
        assert!(!s.allow_prerelease_updates);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let s = Settings {
            check_for_updates: true,
            allow_prerelease_updates: true,
            ..Settings::default()
        };
        s.save(&path).expect("save");
        let loaded = Settings::load(&path).expect("load");
        assert!(loaded.check_for_updates);
        assert!(loaded.allow_prerelease_updates);
    }

    /// The `[reasoning]` table (decision 0098) has no control yet: a file
    /// that sets it loads (it is not an unknown key), is written back as it
    /// was when the window saves, and reaches every run the app starts.
    #[test]
    fn the_reasoning_table_survives_a_save_and_reaches_the_runs() {
        use kgraf_provider::{ReasoningEffort, ReasoningGroup, ReasoningScope};
        let text = "background_mode = true\n[reasoning]\nall = \"low\"\nplanner = \"off\"\n";
        let s = Settings::from_toml_str(text).expect("valid");
        let planner = ReasoningScope::Group(ReasoningGroup::Planner);
        assert_eq!(s.reasoning.get(planner), Some(ReasoningEffort::Off.into()));
        assert_eq!(s.run_limits().reasoning, s.reasoning);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        s.save(&path).expect("save");
        let back = Settings::load(&path).expect("load");
        assert_eq!(back.reasoning, s.reasoning);
        let user = kgraf_host::limits::UserLimits::load(&path).expect("cli read");
        assert_eq!(user.reasoning, s.reasoning);
        let none = Settings::default().to_toml_string().expect("toml");
        assert!(!none.contains("reasoning"), "{none}");
        assert!(Settings::from_toml_str("[reasoning]\nall = \"none\"\n").is_err());
    }

    /// The install host lists (decision 0087, amended 2026-10-07): built-in
    /// unless changed; only a changed list is written, and the host's own
    /// reader reads what the window wrote.
    #[test]
    fn install_registries_round_trip_and_only_a_changed_list_is_written() {
        use kgraf_host::deps::{Ecosystem, InstallHosts};
        let s = Settings::from_toml_str("").expect("valid");
        assert_eq!(s.install_registries, InstallHosts::defaults());
        let text = Settings::default().to_toml_string().expect("toml");
        assert!(!text.contains("install_registries"), "{text}");
        let mut s = Settings::default();
        s.install_registries.set(
            Ecosystem::Npm,
            InstallHosts::parse_list(&["npm.example.com".into(), "127.0.0.1:4873".into()])
                .expect("parse"),
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        s.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("[install_registries]"), "{text}");
        assert!(
            !text.contains("pypi.org"),
            "unchanged lists are not written: {text}"
        );
        let loaded = Settings::load(&path).expect("load");
        assert_eq!(loaded.install_registries, s.install_registries);
        let host_view = InstallHosts::load(&path).expect("the host reads it");
        assert_eq!(host_view, s.install_registries);
        assert!(
            Settings::from_toml_str("[install_registries]\nnpm = [\"https://x/\"]\n").is_err(),
            "a bad entry is refused, not dropped"
        );
    }

    /// Decision 0088: on by default (a fresh file, and an older one written
    /// before the key existed), and a chosen value round-trips as the host's
    /// policy.
    #[test]
    fn worker_worktree_deletion_defaults_on_and_round_trips_as_the_hosts_policy() {
        use kgraf_host::WorktreeRetention;
        let fresh = Settings::from_toml_str("").expect("valid");
        assert!(fresh.delete_worker_worktrees);
        assert_eq!(fresh.worktree_retention(), WorktreeRetention::Delete);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let keep = Settings {
            delete_worker_worktrees: false,
            ..Settings::default()
        };
        keep.save(&path).expect("save");
        let loaded = Settings::load(&path).expect("load");
        assert!(!loaded.delete_worker_worktrees);
        assert_eq!(loaded.worktree_retention(), WorktreeRetention::Keep);
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("delete_worker_worktrees = false")
        );
    }

    /// Decision 0100's two toggles round-trip through the file the command
    /// line also reads, and reach every run the app starts: plan mode off
    /// (full) and early start on by default.
    #[test]
    fn the_small_request_toggles_round_trip_and_reach_the_run_limits() {
        use kgraf_orchestrator::triage::PlanMode;
        let fresh = Settings::from_toml_str("").expect("valid");
        assert!(!fresh.plan_auto);
        assert!(fresh.early_start);
        assert_eq!(fresh.run_limits().plan_mode, None);
        assert_eq!(
            fresh.run_limits().early_start,
            None,
            "no effect with plan mode off"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let chosen = Settings {
            plan_auto: true,
            early_start: false,
            ..Settings::default()
        };
        chosen.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        let user = kgraf_host::limits::UserLimits::from_toml_str(&text).expect("host reads it");
        assert_eq!(user.plan_mode, Some(PlanMode::Auto));
        assert_eq!(user.early_start, Some(false));
        let loaded = Settings::load(&path).expect("load");
        assert!(loaded.plan_auto);
        assert!(!loaded.early_start);
        assert_eq!(loaded.run_limits().plan_mode, Some(PlanMode::Auto));
        assert_eq!(loaded.run_limits().early_start, Some(false));
        assert!(Settings::from_toml_str("plan_mode = \"small\"").is_err());
    }

    #[test]
    fn a_zero_or_negative_parallelism_agents_is_rejected_not_clamped() {
        let err = Settings::from_toml_str("parallelism_agents = 0\nparallelism_managed = true")
            .expect_err("reject");
        assert!(matches!(err, ShellError::CapOutOfRange { .. }));
    }

    // --- Run-limit sliders ---

    #[test]
    fn limit_sliders_have_the_stated_bounds_and_defaults() {
        let l = RunLimitSliders::default();
        let shape = |s: &LimitSlider| (s.min(), s.max(), s.step(), s.value());
        assert_eq!(shape(&l.rounds), (4, 200, 1, 30));
        assert_eq!(shape(&l.max_checkpoints), (1, 50, 1, 10));
        assert_eq!(shape(&l.worker_max_tokens), (1_024, 131_072, 1_024, 16_384));
        assert_eq!(
            shape(&l.planner_max_tokens),
            (4_096, 131_072, 1_024, 32_768)
        );
        assert_eq!(shape(&l.window), (8_192, 1_000_000, 1_024, 128_000));
        assert_eq!(shape(&l.answer_wait), (30, 1_800, 30, 300));
        assert!(
            l.answer_wait
                .default_text()
                .is_some_and(|t| t.contains("measured"))
        );
        assert_eq!(l.user_limits(), kgraf_host::limits::UserLimits::default());
    }

    #[test]
    fn a_slider_moves_in_steps_and_stays_inside_its_bounds() {
        let mut s = LimitSlider::worker_max_tokens();
        assert_eq!(s.snap(5_000), 5_120, "nearest step");
        assert_eq!(s.snap(0), 1_024, "floor is the minimum");
        assert_eq!(s.snap(10_000_000), 131_072, "ceiling is the maximum");
        assert_eq!(s.set_by_user(20_000), 20_480);
        assert_eq!(s.value(), 20_480);
        assert!(!s.follows_default());
        s.reset();
        assert_eq!(s.value(), 16_384);
        assert!(s.follows_default());
        let w = LimitSlider::window();
        assert_eq!(
            w.snap(1_000_000),
            1_000_000,
            "the maximum is reachable off-step"
        );
        let r = LimitSlider::rounds();
        assert_eq!((r.snap(3), r.snap(201), r.snap(77)), (4, 200, 77));
    }

    #[test]
    fn run_limits_persist_and_a_followed_slider_writes_no_key() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.toml");
        let mut s = Settings::default();
        let text = s.to_toml_string().expect("toml");
        for key in [
            "rounds_per_worker",
            "max_checkpoints",
            "worker_max_tokens",
            "planner_max_tokens",
            "context_window",
            "warn_rule_conflicts",
            "answer_wait_secs",
        ] {
            assert!(!text.contains(key), "{key} is not written while followed");
        }
        s.limits.rounds.set_by_user(60);
        s.limits.max_checkpoints.set_by_user(3);
        s.limits.planner_max_tokens.set_by_user(65_536);
        s.limits.window.set_by_user(204_800);
        s.limits.warn_rule_conflicts = true;
        s.limits.answer_wait.set_by_user(600);
        s.save(&path).expect("save");
        let back = Settings::load(&path).expect("load");
        assert_eq!(back, s);
        assert_eq!(back.limits.rounds.value(), 60);
        assert!(back.limits.worker_max_tokens.follows_default());
        // The command line reads the same file with the host's reader.
        let user = kgraf_host::limits::UserLimits::load(&path).expect("cli read");
        assert_eq!(user, back.limits.user_limits());
        assert_eq!(user.rounds, Some(60));
        assert_eq!(user.max_checkpoints, Some(3));
        assert_eq!(user.planner_max_tokens, Some(65_536));
        assert_eq!(user.window, Some(204_800));
        assert_eq!(user.worker_max_tokens, None);
        assert_eq!(user.warn_rule_conflicts, Some(true));
        assert!(back.limits.warn_rule_conflicts);
        assert_eq!(user.answer_wait_secs, Some(600));
    }

    #[test]
    fn a_hand_written_value_runs_as_written_and_a_bad_one_is_refused() {
        let s = Settings::from_toml_str(
            "worker_max_tokens = 5000
",
        )
        .expect("in range");
        assert_eq!(s.limits.worker_max_tokens.value(), 5_000, "not snapped");
        for bad in [
            "rounds_per_worker = 3
",
            "max_checkpoints = 0
",
            "max_checkpoints = 51
",
            "worker_max_tokens = 200000
",
            "planner_max_tokens = 10
",
            "context_window = 5
",
            "warn_rule_conflicts = 3
",
            "answer_wait_secs = 10
",
        ] {
            let e = Settings::from_toml_str(bad).expect_err(bad).to_string();
            assert!(e.contains(bad.split(' ').next().expect("key")), "{e}");
        }
    }
}
