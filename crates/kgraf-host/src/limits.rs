//! Run limits the user sets once, for every run: model rounds per worker, the
//! most progress checkpoints a worker may reach (decision 0096), the output cap
//! of a worker reply, the output cap of an orchestrator call, and the assumed
//! context window.
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

/// Slider bounds (inclusive).
pub const ROUNDS_BOUNDS: (u32, u32) = (4, 200);
pub const MAX_CHECKPOINTS_BOUNDS: (u32, u32) = (1, 50);
pub const WORKER_MAX_TOKENS_BOUNDS: (u32, u32) = (1_024, 131_072);
pub const PLANNER_MAX_TOKENS_BOUNDS: (u32, u32) = (4_096, 131_072);
pub const WINDOW_BOUNDS: (u64, u64) = (8_192, 1_000_000);

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

/// The five limits, each possibly unset. Used for the user's settings and, in
/// the command, for the flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserLimits {
    pub rounds: Option<u32>,
    pub max_checkpoints: Option<u32>,
    pub worker_max_tokens: Option<u32>,
    pub planner_max_tokens: Option<u32>,
    pub window: Option<u64>,
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
        }
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
        assert_eq!(r.report(false).len(), 4, "no planner line without one");
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

    #[test]
    fn the_real_catalog_clamps_a_known_model() {
        let (cap, why) = clamp_output(131_072, [("openai", "gpt-4o")]);
        assert_eq!(cap, 16_384);
        assert!(why.is_some());
    }
}
