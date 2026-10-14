//! Run limits the user sets once, for every run: model rounds per worker, the
//! most progress checkpoints a worker may reach (decision 0096), the output cap
//! of a worker reply, the output cap of an orchestrator call, the assumed
//! context window, and the wait for the orchestrator's model to answer a
//! worker's question (unset: derived from the answering model's recent
//! latencies, decision 0067).
//!
//! They live in the user-level `settings.toml` (the desktop's Settings sliders
//! write them) and never in a project file a worker could write. Precedence,
//! highest first: a command-line flag (or a key in a run file), the user
//! setting, the built-in default. [`ResolvedLimits`] carries each effective
//! value with its source so a run can say where it came from.
//!
//! The bounds are the sliders' range. A settings file that names a value outside
//! them is an error, not a silent clamp. A flag is not bounded here: the
//! command line is the explicit override.
//!
//! The output caps are further clamped, per run, to each model's supported
//! output limit ([`clamp_output`]); that is reported as a notice
//! ([`crate::Notice::OutputCapClamped`]).

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Environment variable that points the command line at a settings file other
/// than the per-user one, as `KGRAF_PERMISSIONS_PATH` does for permissions.
/// Tests set it so they never read the real file.
pub const SETTINGS_PATH_ENV: &str = "KGRAF_SETTINGS_PATH";

/// `settings.toml` key: model rounds per worker.
pub const KEY_ROUNDS: &str = "rounds_per_worker";
/// `settings.toml` key: the most blocks of rounds a worker may be granted while
/// it makes measured progress (decision 0096). 1 is a fixed round limit.
pub const KEY_MAX_CHECKPOINTS: &str = "max_checkpoints";
/// `settings.toml` key: output cap of one worker reply, in tokens.
pub const KEY_WORKER_MAX_TOKENS: &str = "worker_max_tokens";
/// `settings.toml` key: output cap of one orchestrator call, in tokens.
pub const KEY_PLANNER_MAX_TOKENS: &str = "planner_max_tokens";
/// `settings.toml` key: the context window assumed for models, in tokens.
pub const KEY_WINDOW: &str = "context_window";
/// `settings.toml` key: whether a run warns when the user's own verify or gate
/// command runs a program the user's deny or ask rules refuse to workers.
/// Off unless set.
pub const KEY_WARN_RULE_CONFLICTS: &str = "warn_rule_conflicts";
/// `settings.toml` key: how long, in seconds, a worker's question waits for
/// the orchestrator's model to answer it (and how long a connection judgment
/// may run). Unset derives it from the answering model's recent latencies.
pub const KEY_ANSWER_WAIT: &str = "answer_wait_secs";
/// `settings.toml` key: how a run is planned, `auto` (a small request is one
/// contract with no plan call) or `full` (decision 0100). Unset is `full`.
pub const KEY_PLAN_MODE: &str = "plan_mode";
/// `settings.toml` key: whether a request the rule cannot size starts its one
/// worker beside the triage call (decision 0100). Unset is off.
pub const KEY_EARLY_START: &str = "early_start";
/// `settings.toml` key: whether a plan's contract may carry a check of its
/// own, run only when the run's check tests nothing (decision 0100). Unset
/// is off.
pub const KEY_OWN_CHECKS: &str = "own_checks";
/// `settings.toml` key: whether the zero-test rule applies to every
/// contract, not only a small request's (decision 0100). Unset is off.
pub const KEY_ZERO_TEST_PLANS: &str = "zero_test_plans";
/// `settings.toml` key: whether planner calls a dispatch waits on run as
/// tasks beside the run loop (decision 0100). Unset is off.
pub const KEY_CONCURRENT_PLANNER: &str = "concurrent_planner";
/// `settings.toml` key: whether a change to a finished run sends its plan
/// block in id order, marked for the provider's prompt cache (decision
/// 0100). Unset is off.
pub const KEY_CACHE_LAYOUT: &str = "cache_layout";

/// Slider bounds (inclusive).
pub const ROUNDS_BOUNDS: (u32, u32) = (4, 200);
pub const MAX_CHECKPOINTS_BOUNDS: (u32, u32) = (1, 50);
pub const WORKER_MAX_TOKENS_BOUNDS: (u32, u32) = (1_024, 131_072);
pub const PLANNER_MAX_TOKENS_BOUNDS: (u32, u32) = (4_096, 131_072);
pub const WINDOW_BOUNDS: (u64, u64) = (8_192, 1_000_000);
pub const ANSWER_WAIT_BOUNDS: (u32, u32) = (30, 1_800);

/// Built-in defaults.
pub const DEFAULT_ROUNDS_ORCHESTRATED: u32 = 30;
/// A single `kgraf worker` run keeps its own, smaller default: it is one
/// probe, not a share of a plan. The setting still overrides it.
pub const DEFAULT_ROUNDS_SINGLE_WORKER: u32 = 12;
/// The runtime's own default, so the two cannot drift apart.
pub const DEFAULT_MAX_CHECKPOINTS: u32 = kgraf_runtime::DEFAULT_MAX_CHECKPOINTS;
pub const DEFAULT_WORKER_MAX_TOKENS: u32 = 16_384;
pub const DEFAULT_PLANNER_MAX_TOKENS: u32 = 32_768;
pub const DEFAULT_WINDOW: u64 = 128_000;
pub const DEFAULT_WARN_RULE_CONFLICTS: bool = false;

/// The five limits and the rule-conflict warning, each possibly unset. Used for the user's settings and, in
/// the command, for the flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserLimits {
    pub rounds: Option<u32>,
    pub max_checkpoints: Option<u32>,
    pub worker_max_tokens: Option<u32>,
    pub planner_max_tokens: Option<u32>,
    pub window: Option<u64>,
    /// Not a limit: whether to warn at run start about a verify or gate command
    /// that runs a program the user's own rules refuse to workers. It lives
    /// here because it shares the settings file, the flag-then-setting-then-
    /// default order and the desktop's shared copy with the limits.
    pub warn_rule_conflicts: Option<bool>,
    /// The compaction point in percent of the effective window (decision 0057
    /// addendum), as the desktop's slider sets it: the run file's `point`
    /// outranks it. Only the desktop sets it. [`UserLimits::from_toml_str`]
    /// does not read it, so the command line keeps `--point` and its default.
    pub compaction_point: Option<u8>,
    /// The wait, in seconds, for the orchestrator's model to answer a
    /// worker's question. `None` derives it per call from the answering
    /// model's recent latencies (`kgraf_orchestrator::answer_wait`).
    pub answer_wait_secs: Option<u32>,
    /// Not a limit: the reasoning level of each model call (decision 0098),
    /// the `[reasoning]` table. It shares the settings file and the
    /// flag-then-setting order with the limits, scope by scope.
    pub reasoning: kgraf_provider::ReasoningLevels,
    /// Not a limit: how a run is planned (decision 0100). It shares the
    /// flag-then-setting order; a run file outranks the setting.
    pub plan_mode: Option<kgraf_orchestrator::triage::PlanMode>,
    /// Not a limit: early start beside a triage call (decision 0100).
    pub early_start: Option<bool>,
    /// Not a limit: contract checks of their own (decision 0100).
    pub own_checks: Option<bool>,
    /// Not a limit: the zero-test rule for every contract (decision 0100).
    pub zero_test_plans: Option<bool>,
    /// Not a limit: planner calls as tasks beside the loop (decision 0100).
    pub concurrent_planner: Option<bool>,
    /// Not a limit: the delta call's prompt-cache layout (decision 0100).
    pub cache_layout: Option<bool>,
}

/// `settings.toml` table: the reasoning level of each model call (decision
/// 0098), `all` and overrides by group or call kind, each `unset`, `off`,
/// `low`, `medium` or `high`.
pub const KEY_REASONING: &str = "reasoning";

fn read_reasoning(table: &toml::Table) -> Result<kgraf_provider::ReasoningLevels> {
    match table.get(KEY_REASONING) {
        None => Ok(kgraf_provider::ReasoningLevels::new()),
        Some(v) => v
            .clone()
            .try_into()
            .map_err(|e| Error::Invalid(format!("settings: `[{KEY_REASONING}]`: {e}"))),
    }
}

fn read_bool_key(table: &toml::Table, key: &str) -> Result<Option<bool>> {
    match table.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| Error::Invalid(format!("settings: `{key}` must be true or false"))),
    }
}

fn read_plan_mode(table: &toml::Table) -> Result<Option<kgraf_orchestrator::triage::PlanMode>> {
    match table.get(KEY_PLAN_MODE) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .ok_or_else(|| "a string".to_owned())
            .and_then(str::parse)
            .map(Some)
            .map_err(|e| Error::Invalid(format!("settings: `{KEY_PLAN_MODE}`: {e}"))),
    }
}

fn read_key(table: &toml::Table, key: &str, lo: u64, hi: u64) -> Result<Option<u64>> {
    let Some(v) = table.get(key) else {
        return Ok(None);
    };
    let n = v
        .as_integer()
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| Error::Invalid(format!("settings: `{key}` must be a whole number")))?;
    if !(lo..=hi).contains(&n) {
        return Err(Error::Invalid(format!(
            "settings: `{key}` = {n} is outside {lo} to {hi}"
        )));
    }
    Ok(Some(n))
}

impl UserLimits {
    /// The five keys of a settings file's text. Other keys are the shell's own
    /// and are not read here. A present key that is not a whole number inside
    /// its bounds is an error naming the key.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let table: toml::Table = text
            .parse()
            .map_err(|e| Error::Invalid(format!("settings file is not TOML: {e}")))?;
        let b32 = |key, (lo, hi): (u32, u32)| {
            read_key(&table, key, u64::from(lo), u64::from(hi)).map(|v| v.map(|n| n as u32))
        };
        Ok(Self {
            rounds: b32(KEY_ROUNDS, ROUNDS_BOUNDS)?,
            max_checkpoints: b32(KEY_MAX_CHECKPOINTS, MAX_CHECKPOINTS_BOUNDS)?,
            worker_max_tokens: b32(KEY_WORKER_MAX_TOKENS, WORKER_MAX_TOKENS_BOUNDS)?,
            planner_max_tokens: b32(KEY_PLANNER_MAX_TOKENS, PLANNER_MAX_TOKENS_BOUNDS)?,
            window: read_key(&table, KEY_WINDOW, WINDOW_BOUNDS.0, WINDOW_BOUNDS.1)?,
            warn_rule_conflicts: read_bool_key(&table, KEY_WARN_RULE_CONFLICTS)?,
            compaction_point: None,
            answer_wait_secs: b32(KEY_ANSWER_WAIT, ANSWER_WAIT_BOUNDS)?,
            reasoning: read_reasoning(&table)?,
            plan_mode: read_plan_mode(&table)?,
            early_start: read_bool_key(&table, KEY_EARLY_START)?,
            own_checks: read_bool_key(&table, KEY_OWN_CHECKS)?,
            zero_test_plans: read_bool_key(&table, KEY_ZERO_TEST_PLANS)?,
            concurrent_planner: read_bool_key(&table, KEY_CONCURRENT_PLANNER)?,
            cache_layout: read_bool_key(&table, KEY_CACHE_LAYOUT)?,
        })
    }

    /// Reads `path`. A missing file is no settings (first run); any other
    /// failure is an error, so a broken file is never read as "unset".
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text)
                .map_err(|e| Error::Invalid(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Invalid(format!(
                "cannot read settings file {}: {e}",
                path.display()
            ))),
        }
    }

    /// The per-user settings file: [`SETTINGS_PATH_ENV`], else the platform
    /// configuration directory the desktop app writes. `None` without a home
    /// directory.
    pub fn default_path() -> Option<PathBuf> {
        match std::env::var_os(SETTINGS_PATH_ENV) {
            Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
            _ => directories::ProjectDirs::from("", "", "kGraf")
                .map(|d| d.config_dir().join("settings.toml")),
        }
    }

    /// The user's settings from [`Self::default_path`]; none when there is no
    /// such file.
    pub fn load_default() -> Result<Self> {
        match Self::default_path() {
            Some(p) => Self::load(&p),
            None => Ok(Self::default()),
        }
    }
}

/// Where an effective value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Flag,
    Setting,
    Default,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Flag => "flag",
            Source::Setting => "user setting",
            Source::Default => "built-in default",
        }
    }
}

/// Flag, then setting, then default.
pub fn pick<T: Copy>(flag: Option<T>, setting: Option<T>, default: T) -> (T, Source) {
    match (flag, setting) {
        (Some(v), _) => (v, Source::Flag),
        (None, Some(v)) => (v, Source::Setting),
        (None, None) => (default, Source::Default),
    }
}

/// The effective limits of one command, each with its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedLimits {
    pub rounds: (u32, Source),
    pub max_checkpoints: (u32, Source),
    pub worker_max_tokens: (u32, Source),
    pub planner_max_tokens: (u32, Source),
    pub window: (u64, Source),
    /// Off by default.
    pub warn_rule_conflicts: (bool, Source),
    /// `None` by default: derived per call from the answering model.
    pub answer_wait_secs: (Option<u32>, Source),
    /// The flags' reasoning levels over the user's, scope by scope.
    pub reasoning: kgraf_provider::ReasoningLevels,
    /// Where each scope's level came from, for the report.
    reasoning_flags: kgraf_provider::ReasoningLevels,
    /// `full` by default, so a command with no flag plans as before.
    pub plan_mode: (kgraf_orchestrator::triage::PlanMode, Source),
    /// Off by default.
    pub early_start: (bool, Source),
    /// Off by default.
    pub own_checks: (bool, Source),
    /// Off by default.
    pub zero_test_plans: (bool, Source),
    /// Off by default.
    pub concurrent_planner: (bool, Source),
    pub cache_layout: (bool, Source),
}

impl ResolvedLimits {
    /// `rounds_default` differs by command ([`DEFAULT_ROUNDS_ORCHESTRATED`],
    /// [`DEFAULT_ROUNDS_SINGLE_WORKER`]).
    pub fn resolve(flags: &UserLimits, user: &UserLimits, rounds_default: u32) -> Self {
        Self {
            rounds: pick(flags.rounds, user.rounds, rounds_default),
            max_checkpoints: pick(
                flags.max_checkpoints,
                user.max_checkpoints,
                DEFAULT_MAX_CHECKPOINTS,
            ),
            worker_max_tokens: pick(
                flags.worker_max_tokens,
                user.worker_max_tokens,
                DEFAULT_WORKER_MAX_TOKENS,
            ),
            planner_max_tokens: pick(
                flags.planner_max_tokens,
                user.planner_max_tokens,
                DEFAULT_PLANNER_MAX_TOKENS,
            ),
            window: pick(flags.window, user.window, DEFAULT_WINDOW),
            warn_rule_conflicts: pick(
                flags.warn_rule_conflicts,
                user.warn_rule_conflicts,
                DEFAULT_WARN_RULE_CONFLICTS,
            ),
            answer_wait_secs: pick(
                flags.answer_wait_secs.map(Some),
                user.answer_wait_secs.map(Some),
                None,
            ),
            reasoning: flags.reasoning.over(user.reasoning),
            reasoning_flags: flags.reasoning,
            plan_mode: pick(flags.plan_mode, user.plan_mode, Default::default()),
            early_start: pick(flags.early_start, user.early_start, false),
            own_checks: pick(flags.own_checks, user.own_checks, false),
            zero_test_plans: pick(flags.zero_test_plans, user.zero_test_plans, false),
            concurrent_planner: pick(flags.concurrent_planner, user.concurrent_planner, false),
            cache_layout: pick(flags.cache_layout, user.cache_layout, false),
        }
    }

    /// The small-request settings a command runs with (decision 0100).
    pub fn small(&self) -> kgraf_orchestrator::triage::SmallRequestConfig {
        kgraf_orchestrator::triage::SmallRequestConfig {
            plan: self.plan_mode.0,
            early_start: self.early_start.0,
        }
    }

    /// One line per reasoning scope that is set, with its source; one line
    /// saying no field is sent when none is.
    pub fn reasoning_report(&self) -> Vec<String> {
        if self.reasoning.is_empty() {
            return vec![format!(
                "reasoning  unset on every call: no reasoning field is sent ({})",
                Source::Default.label()
            )];
        }
        self.reasoning
            .entries()
            .map(|(scope, level)| {
                let source = if self.reasoning_flags.get(scope).is_some() {
                    Source::Flag
                } else {
                    Source::Setting
                };
                format!("reasoning  {scope} {level} ({})", source.label())
            })
            .collect()
    }

    /// One line per limit, for the run report. `planner` is false for a command
    /// with no orchestrator.
    pub fn report(&self, planner: bool) -> Vec<String> {
        let mut lines = vec![
            format!(
                "limits     rounds per worker {} ({})",
                self.rounds.0,
                self.rounds.1.label()
            ),
            format!(
                "limits     progress checkpoints at most {} ({})",
                self.max_checkpoints.0,
                self.max_checkpoints.1.label()
            ),
            format!(
                "limits     worker output cap {} tokens ({})",
                self.worker_max_tokens.0,
                self.worker_max_tokens.1.label()
            ),
        ];
        if planner {
            lines.push(format!(
                "limits     planner output cap {} tokens ({})",
                self.planner_max_tokens.0,
                self.planner_max_tokens.1.label()
            ));
        }
        lines.push(format!(
            "limits     context window {} tokens ({})",
            self.window.0,
            self.window.1.label()
        ));
        if planner {
            lines.push(match self.answer_wait_secs.0 {
                Some(s) => format!(
                    "limits     answer wait {s} s ({})",
                    self.answer_wait_secs.1.label()
                ),
                None => format!(
                    "limits     answer wait measured from the answering model ({})",
                    self.answer_wait_secs.1.label()
                ),
            });
        }
        lines.extend(self.reasoning_report());
        lines
    }
}

/// An output cap lowered to a model's supported limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputClamp {
    pub requested: u32,
    pub limit: u32,
    pub route: String,
    pub model: String,
}

impl OutputClamp {
    /// The notice as one sentence. `which` is `worker` or `orchestrator`.
    pub fn sentence(&self, which: &str) -> String {
        format!(
            "{which} output cap {} lowered to {} tokens, the output limit of {}:{}",
            self.requested, self.limit, self.route, self.model
        )
    }
}

/// `requested`, lowered to the smallest known output limit among `models`
/// (`(route, model)` pairs; workers rotate across routes, so the cap that is
/// safe on all of them is the smallest). A model the table does not list has no
/// known limit and does not lower it. Returns the cap to use and, when it was
/// lowered, what lowered it.
pub fn clamp_output<'a>(
    requested: u32,
    models: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> (u32, Option<OutputClamp>) {
    clamp_output_with(requested, models, kgraf_core::output_limit::output_limit)
}

/// [`clamp_output`] over a given lookup (tests).
pub fn clamp_output_with<'a>(
    requested: u32,
    models: impl IntoIterator<Item = (&'a str, &'a str)>,
    lookup: impl Fn(&str, &str) -> Option<u64>,
) -> (u32, Option<OutputClamp>) {
    let lowest = models
        .into_iter()
        .filter_map(|(route, model)| {
            lookup(route, model).map(|l| (l.min(u64::from(u32::MAX)) as u32, route, model))
        })
        .min_by_key(|(l, _, _)| *l);
    match lowest {
        Some((limit, route, model)) if limit < requested => (
            limit,
            Some(OutputClamp {
                requested,
                limit,
                route: route.to_owned(),
                model: model.to_owned(),
            }),
        ),
        _ => (requested, None),
    }
}

/// The user's limits as one process shares them: the desktop writes it when a
/// slider moves, and each project's run factory reads it when it opens a host
/// and when it checks whether the host is stale. Cloning shares the same value.
#[derive(Clone, Debug, Default)]
pub struct SharedLimits(std::sync::Arc<std::sync::Mutex<UserLimits>>);

impl SharedLimits {
    pub fn new(limits: UserLimits) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(limits)))
    }

    pub fn get(&self) -> UserLimits {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set(&self, limits: UserLimits) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = limits;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_is_flag_then_setting_then_default() {
        assert_eq!(pick(Some(1), Some(2), 3), (1, Source::Flag));
        assert_eq!(pick(None, Some(2), 3), (2, Source::Setting));
        assert_eq!(pick(None, None, 3), (3, Source::Default));
    }

    #[test]
    fn resolve_takes_each_limit_from_its_own_highest_source() {
        let flags = UserLimits {
            rounds: Some(50),
            ..UserLimits::default()
        };
        let user = UserLimits {
            rounds: Some(40),
            planner_max_tokens: Some(65_536),
            ..UserLimits::default()
        };
        let r = ResolvedLimits::resolve(&flags, &user, 30);
        assert_eq!(r.rounds, (50, Source::Flag));
        assert_eq!(r.planner_max_tokens, (65_536, Source::Setting));
        assert_eq!(r.worker_max_tokens, (16_384, Source::Default));
        assert_eq!(r.window, (128_000, Source::Default));
        assert_eq!(r.max_checkpoints, (10, Source::Default));
        let user = UserLimits {
            max_checkpoints: Some(3),
            ..UserLimits::default()
        };
        let r = ResolvedLimits::resolve(&UserLimits::default(), &user, 30);
        assert_eq!(r.max_checkpoints, (3, Source::Setting));
        let flags = UserLimits {
            max_checkpoints: Some(1),
            ..UserLimits::default()
        };
        let r = ResolvedLimits::resolve(&flags, &user, 30);
        assert_eq!(r.max_checkpoints, (1, Source::Flag));
    }

    #[test]
    fn the_report_names_each_value_and_its_source() {
        let r = ResolvedLimits::resolve(&UserLimits::default(), &UserLimits::default(), 12);
        let text = r.report(true).join("\n");
        assert!(
            text.contains("rounds per worker 12 (built-in default)"),
            "{text}"
        );
        assert!(text.contains("planner output cap 32768"), "{text}");
        assert!(text.contains("reasoning  unset on every call"), "{text}");
        assert_eq!(
            r.report(false).len(),
            5,
            "no planner line without one; one reasoning line"
        );
    }

    /// The `[reasoning]` table of the settings file is read, an unknown
    /// scope or level in it is refused naming the table, and a flag wins
    /// over it scope by scope with each source named in the report.
    #[test]
    fn the_reasoning_table_is_read_and_a_flag_wins_scope_by_scope() {
        use kgraf_provider::{ReasoningEffort, ReasoningGroup, ReasoningLevel, ReasoningScope};
        let user = UserLimits::from_toml_str(
            "rounds_per_worker = 40\n[reasoning]\nall = \"low\"\nplanner = \"off\"\n",
        )
        .expect("read");
        assert_eq!(
            user.reasoning.get(ReasoningScope::All),
            Some(ReasoningLevel::Effort(ReasoningEffort::Low))
        );
        for bad in [
            "[reasoning]\nworkers = \"low\"\n",
            "[reasoning]\nall = \"none\"\n",
        ] {
            let e = UserLimits::from_toml_str(bad).expect_err(bad).to_string();
            assert!(e.contains("`[reasoning]`"), "{e}");
        }
        let flags = UserLimits {
            reasoning: kgraf_provider::ReasoningLevels::new().with(
                ReasoningScope::All,
                ReasoningLevel::Effort(ReasoningEffort::High),
            ),
            ..UserLimits::default()
        };
        let r = ResolvedLimits::resolve(&flags, &user, 30);
        assert_eq!(
            r.reasoning.get(ReasoningScope::All),
            Some(ReasoningLevel::Effort(ReasoningEffort::High))
        );
        assert_eq!(
            r.reasoning
                .get(ReasoningScope::Group(ReasoningGroup::Planner)),
            Some(ReasoningLevel::Effort(ReasoningEffort::Off))
        );
        let text = r.reasoning_report().join("\n");
        assert!(text.contains("reasoning  all high (flag)"), "{text}");
        assert!(
            text.contains("reasoning  planner off (user setting)"),
            "{text}"
        );
    }

    #[test]
    fn a_settings_file_gives_the_five_keys_and_ignores_the_shells_own() {
        let u = UserLimits::from_toml_str(
            "background_mode = true\nrounds_per_worker = 60\nmax_checkpoints = 7\n\
             worker_max_tokens = 8192\nplanner_max_tokens = 65536\ncontext_window = 200000\n",
        )
        .expect("parses");
        assert_eq!(u.rounds, Some(60));
        assert_eq!(u.max_checkpoints, Some(7));
        assert_eq!(u.worker_max_tokens, Some(8_192));
        assert_eq!(u.planner_max_tokens, Some(65_536));
        assert_eq!(u.window, Some(200_000));
        assert_eq!(
            UserLimits::from_toml_str("appearance = \"dark\"\n").expect("parses"),
            UserLimits::default()
        );
    }

    #[test]
    fn a_value_outside_the_bounds_or_not_a_number_is_an_error_naming_the_key() {
        for text in [
            "rounds_per_worker = 3\n",
            "rounds_per_worker = 201\n",
            "max_checkpoints = 0\n",
            "max_checkpoints = 51\n",
            "worker_max_tokens = 100\n",
            "planner_max_tokens = 999999\n",
            "context_window = 1000\n",
            "context_window = \"big\"\n",
            "rounds_per_worker = -5\n",
        ] {
            let e = UserLimits::from_toml_str(text).expect_err(text).to_string();
            let key = text.split(' ').next().expect("key");
            assert!(e.contains(key), "{text}: {e}");
        }
    }

    #[test]
    fn a_missing_file_is_unset_and_a_broken_one_is_an_error() {
        let dir = tempfile::tempdir().expect("dir");
        let missing = dir.path().join("settings.toml");
        assert_eq!(
            UserLimits::load(&missing).expect("first run"),
            UserLimits::default()
        );
        std::fs::write(&missing, "rounds_per_worker = [").expect("write");
        let e = UserLimits::load(&missing).expect_err("broken").to_string();
        assert!(e.contains("settings.toml"), "{e}");
    }

    #[test]
    fn a_cap_above_a_models_limit_is_lowered_with_the_cause() {
        let table = |_: &str, m: &str| match m {
            "small" => Some(8_192),
            "big" => Some(65_536),
            _ => None,
        };
        let (cap, why) = clamp_output_with(
            32_768,
            [("a", "big"), ("b", "small"), ("c", "unlisted")],
            table,
        );
        assert_eq!(cap, 8_192);
        let why = why.expect("lowered");
        assert_eq!((why.requested, why.limit), (32_768, 8_192));
        assert_eq!((why.route.as_str(), why.model.as_str()), ("b", "small"));
    }

    #[test]
    fn a_cap_within_every_limit_or_with_no_known_limit_is_unchanged() {
        let table = |_: &str, m: &str| (m == "big").then_some(65_536);
        assert_eq!(
            clamp_output_with(32_768, [("a", "big")], table),
            (32_768, None)
        );
        assert_eq!(
            clamp_output_with(32_768, [("a", "x")], table),
            (32_768, None)
        );
        assert_eq!(clamp_output_with(1, std::iter::empty(), table), (1, None));
    }

    /// The answer wait: unset (measured) by default, read in seconds inside
    /// its bounds, refused outside them by key, flag then setting then
    /// default, and reported with its source.
    #[test]
    fn the_answer_wait_is_measured_unless_set_and_follows_the_precedence() {
        let none = UserLimits::default();
        let r = |f: &UserLimits, u: &UserLimits| ResolvedLimits::resolve(f, u, 30);
        assert_eq!(r(&none, &none).answer_wait_secs, (None, Source::Default));
        assert!(
            r(&none, &none)
                .report(true)
                .iter()
                .any(|l| l.contains("answer wait measured from the answering model")),
        );
        let set = UserLimits::from_toml_str("answer_wait_secs = 600\n").expect("set");
        assert_eq!(set.answer_wait_secs, Some(600));
        assert_eq!(
            r(&none, &set).answer_wait_secs,
            (Some(600), Source::Setting)
        );
        assert!(
            r(&none, &set)
                .report(true)
                .contains(&"limits     answer wait 600 s (user setting)".to_owned())
        );
        let flag = UserLimits {
            answer_wait_secs: Some(90),
            ..none
        };
        assert_eq!(r(&flag, &set).answer_wait_secs, (Some(90), Source::Flag));
        let e = UserLimits::from_toml_str("answer_wait_secs = 5\n")
            .expect_err("below the bounds")
            .to_string();
        assert!(
            e.contains("answer_wait_secs") && e.contains("30 to 1800"),
            "{e}"
        );
    }

    /// The warning setting: off when unset, read as a boolean from the same file,
    /// a non-boolean refused by key, and flag then setting then default.
    #[test]
    fn the_rule_conflict_setting_is_off_by_default_and_follows_the_precedence() {
        assert_eq!(
            UserLimits::from_toml_str("")
                .expect("empty")
                .warn_rule_conflicts,
            None
        );
        let on = UserLimits::from_toml_str("warn_rule_conflicts = true\n").expect("on");
        assert_eq!(on.warn_rule_conflicts, Some(true));
        let e = UserLimits::from_toml_str("warn_rule_conflicts = 1\n")
            .expect_err("not a boolean")
            .to_string();
        assert!(e.contains("warn_rule_conflicts"), "{e}");
        let none = UserLimits::default();
        let off = UserLimits {
            warn_rule_conflicts: Some(false),
            ..none
        };
        let r = |f: &UserLimits, u: &UserLimits| ResolvedLimits::resolve(f, u, 30);
        assert_eq!(
            r(&none, &none).warn_rule_conflicts,
            (false, Source::Default)
        );
        assert_eq!(r(&none, &on).warn_rule_conflicts, (true, Source::Setting));
        assert_eq!(r(&off, &on).warn_rule_conflicts, (false, Source::Flag));
    }

    #[test]
    fn the_real_catalog_clamps_a_known_model() {
        let (cap, why) = clamp_output(131_072, [("openai", "gpt-4o")]);
        assert_eq!(cap, 16_384);
        assert!(why.is_some());
    }

    /// Decision 0100's settings: read from the file, a flag over the
    /// setting, and `full` with no early start when neither says.
    #[test]
    fn the_small_request_settings_resolve_flag_then_setting_then_default() {
        use kgraf_orchestrator::triage::{PlanMode, SmallRequestConfig};
        let user =
            UserLimits::from_toml_str("plan_mode = \"auto\"\nearly_start = true\n").expect("read");
        assert_eq!(user.plan_mode, Some(PlanMode::Auto));
        assert_eq!(user.early_start, Some(true));
        assert!(UserLimits::from_toml_str("plan_mode = \"small\"").is_err());
        assert!(UserLimits::from_toml_str("early_start = 1").is_err());
        let none = ResolvedLimits::resolve(&UserLimits::default(), &UserLimits::default(), 30);
        assert_eq!(none.small(), SmallRequestConfig::default());
        let setting = ResolvedLimits::resolve(&UserLimits::default(), &user, 30);
        assert_eq!(
            setting.small(),
            SmallRequestConfig {
                plan: PlanMode::Auto,
                early_start: true
            }
        );
        let flags = UserLimits {
            plan_mode: Some(PlanMode::Full),
            early_start: Some(false),
            ..UserLimits::default()
        };
        let flag = ResolvedLimits::resolve(&flags, &user, 30);
        assert_eq!(flag.small(), SmallRequestConfig::default());
        assert_eq!(flag.plan_mode.1, Source::Flag);
    }
}
