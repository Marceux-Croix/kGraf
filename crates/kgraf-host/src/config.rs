//! What a run is built from: [`RunConfig`], and the parsing `kgraf orchestrate`
//! did inline (routes, caps, rate limits).
//!
//! The fields are the command's arguments, with the command's defaults, so the
//! command fills one from its flags and the shell loads one from a file
//! ([`RunConfig::load`]). Everything else a run reads comes from the same places for
//! both: the price table named here, `.kgraf/worker.json` for the MCP servers,
//! and each route's credential from its environment variable
//! (`kgraf_fleet::credential_from_env`).

use crate::error::{Error, NotReady, Result, ResultExt};
use kgraf_core::PriceTable;
use kgraf_provider::{
    Provider, RateLimit, RateLimited, RouteLimiter, RouteRecorded, SharedLimiterStore, key_id,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Everything `kgraf orchestrate`'s flags and files provide that is the same
/// for every run of a project. What differs per run (the instruction, a resume, an
/// approved plan, a gate decision) is not here: see [`crate::RunStart`].
///
/// Serialized as JSON with the command's flag names in snake case; a field left
/// out takes the command's default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// The project root. Not serialized: [`RunConfig::load`] takes it.
    #[serde(skip)]
    pub root: PathBuf,
    /// Worker routes as `route:model` (`--route`); workers rotate across them.
    pub routes: Vec<String>,
    /// The orchestrator's own `route:model` (`--planner`); default: the first route.
    #[serde(default)]
    pub planner: Option<String>,
    /// Routes of non-generative model kinds as `kind=route:model`
    /// (`--model-route`, decision 0089): `typed-decision`, `embedding`,
    /// `rerank`. They run no worker and plan nothing; the orchestrator is told
    /// them by kind, and their tools (`decide`, `embed`, `rerank`) are offered
    /// in every worker environment of the run. Credentials and pricing are a
    /// route's like any other.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_routes: Vec<String>,
    /// The JSON price table (`--prices`). Relative paths in a loaded file are
    /// resolved against the file's directory.
    pub prices: PathBuf,
    /// Deprecated (decision 0080: kGraf does not limit
    /// spend). No longer read by [`crate::Host::open`] or its own flag;
    /// accepted here only so an old run file with a cap still set still
    /// loads, with a one-time notice on stderr that it is ignored, rather
    /// than an error (`RunConfig::load`).
    #[serde(default)]
    pub cap_run: Option<f64>,
    /// Deprecated, ignored: see `cap_run`.
    #[serde(default)]
    pub cap_task: Option<f64>,
    /// Deprecated, ignored: see `cap_run`.
    #[serde(default)]
    pub cap_hour: Option<f64>,
    /// Work items in flight at once (`--max-items`, decision 0017).
    #[serde(default = "defaults::max_items")]
    pub max_items: usize,
    /// Live workers at once (`--max-agents`).
    #[serde(default = "defaults::max_agents")]
    pub max_agents: usize,
    /// Model rounds per worker (`--steps`).
    #[serde(default = "defaults::steps")]
    pub steps: u32,
    /// Output ceiling per worker round (`--max-tokens`).
    #[serde(default = "defaults::max_tokens")]
    pub max_tokens: u32,
    /// The models' context window in tokens (`--window`).
    #[serde(default = "defaults::window")]
    pub window: u64,
    /// Compaction point in percent (`--point`).
    #[serde(default)]
    pub point: Option<u8>,
    /// The acceptance command run in each worker's worktree (`--verify`).
    #[serde(default = "defaults::verify")]
    pub verify: String,
    /// Quality-gate commands (`--gate-command`); empty discovers them.
    #[serde(default)]
    pub gate_commands: Vec<String>,
    /// Longest one gate command, or one run of the acceptance command
    /// (`verify`), may run (`--gate-timeout`, in seconds on disk).
    #[serde(default = "defaults::gate_timeout", with = "seconds")]
    pub gate_timeout: Duration,
    /// `ROUTE=RPM[/BURST][@MAX_WAIT_MS]` per rate-limited route (`--rate-limit`).
    #[serde(default)]
    pub rate_limits: Vec<String>,
    /// Interface contracts (0026-0037): the decomposition writes them, each is
    /// checked in the sandbox before its first party is dispatched
    /// (stub-compile with canaries, two-sided vacuity, the handshake), and
    /// workers may challenge them. **Off by default** until a live run against
    /// a real provider shows them working and their cost is measured;
    /// `--contract-checks` turns them on.
    #[serde(default = "defaults::contract_checks")]
    pub contract_checks: bool,
    /// Longest one contract-check test run may take (`--check-timeout`, in
    /// seconds on disk). A timeout against the stub is not an assertion failure.
    #[serde(default = "defaults::check_timeout", with = "seconds")]
    pub check_timeout: Duration,
    /// Longest one contract-check compile may take
    /// (`--check-compile-timeout`), bounded apart from the test runs: a cold
    /// compile on a loaded machine is slow for reasons that say nothing about
    /// the contract, and two timeouts in a row leave the check inconclusive.
    #[serde(default = "defaults::check_compile_timeout", with = "seconds")]
    pub check_compile_timeout: Duration,
    /// Longest one worker shell command may run (`--shell-timeout`, in
    /// seconds on disk; decision 0084). `None` keeps the worker
    /// configuration's `shell_timeout_secs`, itself 600 s when absent.
    #[serde(default, with = "opt_seconds", skip_serializing_if = "Option::is_none")]
    pub shell_timeout: Option<Duration>,
    /// Every agent is an addressable session (decision 0067): registers the
    /// session tools (`ask_user_question`, `spawn_subagent`, `send_message`,
    /// `request_connection`) in every worker environment. **On by default**:
    /// a live run shows the steer, an
    /// orchestrator-answered question and an admitted, merged subagent
    /// together.
    #[serde(default = "defaults::sessions")]
    pub sessions: bool,
    /// Output ceiling of each orchestrator call (`--planner-max-tokens`):
    /// decomposition, re-grounding, the handshake, a challenge review. An
    /// answer cut off at it fails loudly, naming this setting; with contract
    /// checks on, the decomposition carries whole interface files, so it may
    /// need more than the default.
    #[serde(default = "defaults::planner_max_tokens")]
    pub planner_max_tokens: u32,
    /// Cross-vendor review at the interface (decision 0079,
    /// `--interface-review`): before a producer attempt is counted verified,
    /// one review call goes to a model on a cross-vendor consumer's route.
    /// **Off by default**; the benefit is unmeasured. When on,
    /// `Host::run` binds a `kgraf_orchestrator::RoutedInterfaceReviewer` on the
    /// run's own routes.
    #[serde(default = "defaults::interface_review")]
    pub interface_review: bool,
    /// Vendor/model routing policy (`--routing`, decision 0075). **Rotation by
    /// default**: a three-arm experiment elsewhere in this project depends on
    /// rotation's behavior being unchanged. `fitness` is opt-in
    /// coupling- and measured-capability-aware routing
    /// (`kgraf_orchestrator::routing`).
    #[serde(default)]
    pub routing: kgraf_orchestrator::RoutingPolicy,
    /// `--tbench-bridge-socket`: a Unix socket a Python Harbor shim listens
    /// on. When set, every attempt's shell commands are routed into that
    /// shim's task container instead of the local sandbox, and its file
    /// writes are synced into the container. `None` for a normal run; not
    /// written to a resumed run file unless it was actually set (a run
    /// resumed without this flag stays a normal, unrouted run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tbench_bridge_socket: Option<PathBuf>,
    /// This run's answer about fetching dependencies (decision 0087): the
    /// command's `--fetch-deps` / `--no-fetch-deps`. Not in `run.json`: a
    /// scheduled or desktop run goes by the project's stored consent.
    #[serde(skip)]
    pub dependency_fetch: crate::deps::RunFetch,
    /// What happens to a worker's worktree, and the build output in it, once
    /// nothing needs it (decision 0088): `Delete` (the default) or `Keep`.
    /// One source per surface: the command's `--keep-worktrees`, and in the
    /// desktop app its settings switch. Not in `run.json`, which only the
    /// desktop app reads and where its switch decides; a file that sets
    /// `worktrees` is refused ([`RunConfig::load`]) rather than ignored.
    #[serde(skip)]
    pub worktrees: WorktreeRetention,
    /// `--approve-plan` (decision 0094): with plan review on, approve the
    /// plan without showing it. Command line only: never in `run.json`.
    #[serde(skip)]
    pub approve_plan: bool,
}

/// Keys a run file may not set (decision 0094): the permission mode, its
/// allow set and rules, plan review and the bypass switches are the user's
/// alone, set from the desktop control, the user-level permissions file or
/// the command line. A project file a worker could write never sets them.
pub const USER_ONLY_KEYS: &[&str] = &[
    "permission_mode",
    "mode",
    "permissions",
    "allow",
    "deny",
    "ask",
    "plan_review",
    "approve_plan",
    "bypass",
    "allow_bypass",
    "disable_bypass",
    "bypass_warning_accepted",
    "may_grant",
    "container_sandbox",
];

/// The run-file key [`RunConfig::load`] refuses (decision 0088).
const WORKTREES_KEY: &str = "worktrees";

/// Whether a worker's worktree, with the build output inside it, is deleted
/// after use (decision 0088).
///
/// A worktree a live worker, a stopped or resumable run, a pending merge or a
/// conflict resolver still needs is kept whichever way this is set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeRetention {
    /// Remove the worktree and everything in it once nothing needs it.
    #[default]
    Delete,
    /// Keep worktrees and their builds on disk for the user to inspect or
    /// reuse; the user removes them (`kgraf clean`).
    Keep,
}

impl WorktreeRetention {
    /// `true` for [`WorktreeRetention::Delete`].
    pub fn deletes(self) -> bool {
        self == Self::Delete
    }

    /// The setting as a switch: on means delete after use.
    pub fn from_delete(delete: bool) -> Self {
        if delete { Self::Delete } else { Self::Keep }
    }
}

/// The command's defaults, in one place (`main.rs`'s clap attributes name the same
/// values; a test holds the two together).
pub mod defaults {
    use std::time::Duration;
    pub fn max_items() -> usize {
        4
    }
    pub fn max_agents() -> usize {
        8
    }
    pub fn steps() -> u32 {
        30
    }
    pub fn max_tokens() -> u32 {
        4096
    }
    pub fn window() -> u64 {
        128_000
    }
    pub fn verify() -> String {
        "cargo test --workspace".to_owned()
    }
    pub fn gate_timeout() -> Duration {
        Duration::from_secs(1800)
    }
    /// Off until shown live with a real provider and measured. Kept off
    /// by decision 0078: measured at about $0.021 per interface, but no live
    /// run with checks on has reached dispatch (the default planner ceiling
    /// cut off the decomposition; the replayed handshake holds on a real
    /// shared-file overlap).
    pub fn contract_checks() -> bool {
        false
    }
    /// Off: unmeasured (decision 0079).
    pub fn interface_review() -> bool {
        false
    }
    /// The orchestrator's `SandboxRunner` default for test runs.
    pub fn check_timeout() -> Duration {
        Duration::from_secs(600)
    }
    /// The orchestrator's `SandboxRunner` default for compiles.
    pub fn check_compile_timeout() -> Duration {
        Duration::from_secs(600)
    }
    /// On by default (decision 0067): one live run (with
    /// the run's own caps sized from a dry run of the reservation math) shows
    /// a steer reaching the worker, a question the orchestrator itself
    /// answers (a ledgered answer call whose answer is the tool result), and
    /// a subagent that is admitted, runs, and merges its work back, all in
    /// one run. `--sessions false` (or `"sessions": false` in a run file)
    /// turns it off.
    pub fn sessions() -> bool {
        true
    }
    /// The orchestrator's output ceiling before the setting existed.
    pub fn planner_max_tokens() -> u32 {
        8_192
    }
}

mod seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}

mod opt_seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&d.as_secs()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Option::<u64>::deserialize(d).map(|o| o.map(Duration::from_secs))
    }
}

impl RunConfig {
    /// A configuration with the command's defaults for everything but what the
    /// command requires.
    pub fn new(root: impl Into<PathBuf>, routes: Vec<String>, prices: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            routes,
            planner: None,
            model_routes: Vec::new(),
            prices: prices.into(),
            cap_run: None,
            cap_task: None,
            cap_hour: None,
            max_items: defaults::max_items(),
            max_agents: defaults::max_agents(),
            steps: defaults::steps(),
            max_tokens: defaults::max_tokens(),
            window: defaults::window(),
            point: None,
            verify: defaults::verify(),
            gate_commands: Vec::new(),
            gate_timeout: defaults::gate_timeout(),
            rate_limits: Vec::new(),
            contract_checks: defaults::contract_checks(),
            check_timeout: defaults::check_timeout(),
            check_compile_timeout: defaults::check_compile_timeout(),
            shell_timeout: None,
            sessions: defaults::sessions(),
            planner_max_tokens: defaults::planner_max_tokens(),
            interface_review: defaults::interface_review(),
            routing: kgraf_orchestrator::RoutingPolicy::default(),
            tbench_bridge_socket: None,
            dependency_fetch: crate::deps::RunFetch::Stored,
            worktrees: WorktreeRetention::default(),
            approve_plan: false,
        }
    }

    /// Read a configuration file for the project at `root`. Parsing only: the
    /// values are checked by [`RunConfig::check_configured`] and by
    /// [`crate::Host::open`], exactly as the command's flags are.
    ///
    /// An old file that still sets `cap_run`, `cap_task` or `cap_hour` loads
    /// without error (decision 0080: kGraf does not limit spend, so those
    /// fields are deprecated, not removed from the format) and prints one
    /// notice on stderr, per file loaded, naming which were set and that they
    /// no longer apply.
    pub fn load(root: impl Into<PathBuf>, path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_step(|| format!("cannot read {}", path.display()))?;
        // Said plainly, rather than as serde's "unknown field": the key was
        // once documented here, and a user who set it must learn where the
        // choice lives now.
        if serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .is_some_and(|v| v.get(WORKTREES_KEY).is_some())
        {
            return Err(Error::Invalid(format!(
                "{} sets `{WORKTREES_KEY}`, which a run file does not choose: in the desktop \
                 app, Settings > General > \"Delete worker worktrees after use\" decides for \
                 every run it starts, and `kgraf orchestrate` and `kgraf schedule run-now` \
                 take `--keep-worktrees`. Remove the key.",
                path.display()
            )));
        }
        if let Some(key) = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                USER_ONLY_KEYS
                    .iter()
                    .find(|k| v.get(**k).is_some())
                    .copied()
            })
        {
            return Err(Error::Invalid(format!(
                "{} sets `{key}`, which only you set: permission settings come from the \
                 desktop's permission control, the user-level permissions file, or the command \
                 line, never from a project file a worker could write. Remove the key.",
                path.display()
            )));
        }
        let mut config: RunConfig = serde_json::from_str(&text)
            .with_step(|| format!("{} is not a run configuration", path.display()))?;
        config.root = root.into();
        if config.prices.is_relative()
            && let Some(dir) = path.parent()
        {
            config.prices = dir.join(&config.prices);
        }
        config.notice_deprecated_caps(path);
        Ok(config)
    }

    /// The stderr notice `RunConfig::load` prints once per loaded file that
    /// still sets a cap. Never an error: decision 0080 removed enforcement,
    /// not the fields, so an old file must still load.
    fn notice_deprecated_caps(&self, path: &Path) {
        let set: Vec<&str> = [
            ("cap_run", self.cap_run.is_some()),
            ("cap_task", self.cap_task.is_some()),
            ("cap_hour", self.cap_hour.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, is_set)| is_set.then_some(name))
        .collect();
        if !set.is_empty() {
            eprintln!(
                "note: {} sets {} ({}), but kGraf does not limit spend (decision 0080); \
                 ignored, not applied",
                path.display(),
                if set.len() == 1 { "a cap" } else { "caps" },
                set.join(", ")
            );
        }
    }

    /// Whole-process readiness: can this configuration run anything at all? Routes
    /// present and well formed, rate limits well formed and on used routes, the
    /// context window and compaction point valid, the concurrency limits nonzero,
    /// the price table readable (no spend cap: decision 0080). Reads the price
    /// table and nothing
    /// else; writes nothing, **except** that a non-empty, otherwise-valid
    /// `rate_limits` opens (and, on first use, creates) the per-user shared rate
    /// limit store ([`limiters`], decision 0072): that open is itself the
    /// fail-loud check this method exists to do, the file it creates is the same
    /// one `Host::open` would create moments later, and opening it touches no
    /// project state. Per-route facts (catalog, credential, prices) are
    /// [`crate::Host::check_ready`]'s.
    ///
    /// Delegates to [`Self::check_configured_with_store`] with
    /// [`production_rate_limit_store_path`], the real per-user file; that
    /// method takes the path explicitly only so this crate's own tests can each
    /// point at a temporary file instead (no test may touch the
    /// real user directory).
    pub fn check_configured(&self) -> Result<(), NotReady> {
        // Resolved lazily: `limiters` never touches `store_path` at all when
        // `rate_limits` is empty, so an empty-rate-limits caller (most of
        // them) must not pay `production_rate_limit_store_path`'s test-mode
        // panic (or, in production, an unnecessary directory resolution)
        // just because this method happened to be called.
        let store_path = if self.rate_limits.is_empty() {
            None
        } else {
            production_rate_limit_store_path()
        };
        self.check_configured_with_store(store_path.as_deref())
    }

    /// [`Self::check_configured`] with the shared rate limit store's path given
    /// explicitly.
    pub(crate) fn check_configured_with_store(
        &self,
        store_path: Option<&Path>,
    ) -> Result<(), NotReady> {
        if self.routes.is_empty() {
            return Err(NotReady::NoRoutes);
        }
        let invalid = |e: Error| NotReady::Invalid(e.to_string());
        let (_, used) = self.planner_and_used().map_err(invalid)?;
        let context = kgraf_runtime::ContextConfig::for_model(self.window)
            .map_err(|e| NotReady::Invalid(e.to_string()))?;
        if let Some(point) = self.point {
            context
                .with_point(point, kgraf_runtime::PointSource::User)
                .map_err(|e| NotReady::Invalid(e.to_string()))?;
        }
        kgraf_orchestrator::Limits::new(self.max_items, self.max_agents)
            .map_err(|e| NotReady::Invalid(e.to_string()))?;
        limiters(&self.rate_limits, &used, store_path).map_err(invalid)?;
        self.check_bounds().map_err(invalid)?;
        read_prices(&self.prices).map_err(|e| NotReady::PricesUnreadable {
            path: self.prices.clone(),
            detail: format!("{e:#}"),
        })?;
        Ok(())
    }

    /// The contract-check bounds, when checks are on: a zero bound would time
    /// out every check, which reads as a run with checks that never pass. And
    /// the worker shell timeout, when set (decision 0084): zero kills every
    /// command.
    pub(crate) fn check_bounds(&self) -> Result<()> {
        if self.contract_checks {
            for (flag, bound) in [
                ("--check-timeout", self.check_timeout),
                ("--check-compile-timeout", self.check_compile_timeout),
            ] {
                if bound.is_zero() {
                    return Err(Error::Invalid(format!(
                        "{flag} must be at least one second: every contract check would time out"
                    )));
                }
            }
        }
        // An empty command runs as a no-op shell and passes, so an empty
        // acceptance or gate command would accept everything.
        if self.verify.trim().is_empty() {
            return Err(Error::Invalid(
                "--verify must not be empty: an empty acceptance command passes every attempt"
                    .into(),
            ));
        }
        if self.gate_commands.iter().any(|c| c.trim().is_empty()) {
            return Err(Error::Invalid(
                "--gate-command must not be empty: an empty gate command passes without \
                 checking anything"
                    .into(),
            ));
        }
        // The gate timeout also bounds the acceptance command: zero kills both.
        if self.gate_timeout.is_zero() {
            return Err(Error::Invalid(
                "--gate-timeout must be at least one second: every gate and acceptance command \
                 would be killed"
                    .into(),
            ));
        }
        // Decision 0084: a zero shell timeout would kill every worker command.
        if self.shell_timeout.is_some_and(|t| t.is_zero()) {
            return Err(Error::Invalid(
                "--shell-timeout must be at least one second: every worker shell command \
                 would be killed"
                    .into(),
            ));
        }
        Ok(())
    }

    /// The planner route, and every route the run uses (the planner's included).
    pub(crate) fn planner_and_used(&self) -> Result<(RouteSpec, BTreeSet<String>)> {
        let first = self
            .routes
            .first()
            .ok_or_else(|| Error::Invalid("give at least one --route route:model".into()))?;
        // Worker and planner routes do generative work only (decision 0089): a
        // kind prefix there is refused, never read as part of the route id.
        for spec in self.routes.iter().chain(self.planner.iter()) {
            generative_only(spec)?;
        }
        let planner = RouteSpec::parse(self.planner.as_deref().unwrap_or(first))?;
        let mut used = BTreeSet::from([planner.route.clone()]);
        for spec in &self.routes {
            used.insert(split_route(spec)?.0);
        }
        // A model route is a used route too (decision 0089): `--rate-limit`
        // paces its calls as it paces a worker's.
        for spec in self.model_route_specs()? {
            used.insert(spec.route.route);
        }
        Ok((planner, used))
    }

    /// [`Self::model_routes`], parsed: each names a non-generative kind, and no
    /// `kind=route:model` repeats.
    pub fn model_route_specs(&self) -> Result<Vec<ModelRouteSpec>> {
        let mut out: Vec<ModelRouteSpec> = Vec::with_capacity(self.model_routes.len());
        for spec in &self.model_routes {
            let parsed = ModelRouteSpec::parse(spec)?;
            if out.contains(&parsed) {
                return Err(Error::Invalid(format!(
                    "model route `{spec}` is given twice"
                )));
            }
            out.push(parsed);
        }
        Ok(out)
    }
}

/// Refuse a kind prefix on a worker or planner route.
fn generative_only(spec: &str) -> Result<()> {
    let (kind, rest) = kgraf_core::split_kind_prefix(spec)
        .map_err(|e| Error::Invalid(format!("`{spec}`: {e}")))?;
    if rest.len() == spec.len() {
        // No prefix (an `=` inside the model name is not one).
        return Ok(());
    }
    if kind.is_generative() {
        return Err(Error::Invalid(format!(
            "`{spec}`: --route and --planner take no kind prefix; they are generative already"
        )));
    }
    Err(Error::Invalid(format!(
        "`{spec}` names the {kind} kind, but --route and --planner run generative work only; \
         declare a {kind} route with --model-route (`model_routes` in run.json)"
    )))
}

/// A non-generative route: `kind=route:model` (decision 0089).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRouteSpec {
    pub kind: kgraf_core::ModelKind,
    pub route: RouteSpec,
}

impl ModelRouteSpec {
    /// Parse `kind=route:model`. A spec with no kind, or the generative kind,
    /// is refused: generative routes are `--route` and `--planner`.
    pub fn parse(spec: &str) -> Result<Self> {
        let (kind, rest) = kgraf_core::split_kind_prefix(spec)
            .map_err(|e| Error::Invalid(format!("`{spec}`: {e}")))?;
        if kind.is_generative() {
            return Err(Error::Invalid(format!(
                "model route `{spec}` names no non-generative kind; write kind=route:model with \
                 a kind of {}; generative routes go in --route",
                kgraf_core::KINDS
                    .iter()
                    .filter(|k| !k.kind.is_generative())
                    .map(|k| k.kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(Self {
            kind,
            route: RouteSpec::parse(rest)?,
        })
    }
}

impl std::fmt::Display for ModelRouteSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.kind, self.route)
    }
}

/// Build the run's non-generative routes from the catalog, each with its own
/// credential through `lookup`, behind its route's limiter where `--rate-limit`
/// sets one: the same limiter the route's generative calls take, bound to the
/// same key, so a route's embedding calls and its worker calls share one pace.
/// A route that does not serve its kind, or has no credential, raises.
pub(crate) fn model_routes_from(
    specs: &[ModelRouteSpec],
    limiters: &Limiters,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<kgraf_provider::ModelRoutes> {
    let mut routes = Vec::with_capacity(specs.len());
    for spec in specs {
        let entry = kgraf_fleet::entry(&spec.route.route).settings()?;
        let credential = kgraf_fleet::credential_from(entry, &lookup)
            .map_err(|e| Error::Credential(crate::error::Chained::bare(Box::new(e))))?;
        let route =
            kgraf_fleet::build_kind(spec.kind, &spec.route.route, &credential, &spec.route.model)
                .settings()?;
        routes.push(match limiters.get(&spec.route.route) {
            Some(limiter) => {
                limiter.bind_key(&key_id(&credential));
                route.with_limiter(limiter.clone())
            }
            None => route,
        });
    }
    Ok(kgraf_provider::ModelRoutes::new(routes))
}

/// Read and parse a price table, as the command did.
pub fn read_prices(path: &Path) -> Result<PriceTable> {
    let mut table: PriceTable = serde_json::from_str(
        &std::fs::read_to_string(path).with_step(|| format!("cannot read {}", path.display()))?,
    )
    .step("the price table is not valid JSON")?;
    // A route that names a user-chosen deployment is priced only under its
    // `<model>@<route>` key, never by a bare model entry that may not be it.
    for route in kgraf_fleet::route_scoped_price_routes() {
        table.require_route_scoped(route);
    }
    Ok(table)
}

/// A provider route and a model on it: `route:model`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteSpec {
    /// The catalog route id (`kgraf providers`).
    pub route: String,
    pub model: String,
}

impl RouteSpec {
    pub fn parse(spec: &str) -> Result<Self> {
        let (route, model) = split_route(spec)?;
        Ok(Self { route, model })
    }
}

impl std::fmt::Display for RouteSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.route, self.model)
    }
}

impl std::str::FromStr for RouteSpec {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// `route:model`.
pub(crate) fn split_route(spec: &str) -> Result<(String, String)> {
    let not = || Error::Invalid(format!("`{spec}` is not route:model"));
    let (r, m) = spec.split_once(':').ok_or_else(not)?;
    if r.is_empty() || m.is_empty() {
        return Err(not());
    }
    Ok((r.to_owned(), m.to_owned()))
}

/// Route name to its shared limiter: every provider built for a route
/// passes the same limiter, since a vendor's limit is per endpoint and key.
pub(crate) type Limiters = BTreeMap<String, Arc<RouteLimiter>>;

/// The real per-user shared rate limit store path
/// (`kgraf_provider::default_store_path`), used only by this crate's two
/// production call sites ([`RunConfig::check_configured`] and
/// [`crate::Host::open`]).
///
/// **Never callable from this crate's own tests.** A test that resolved the
/// default store would write a real `rate-limits.sqlite3` into the actual
/// per-user data directory. `kgraf_provider::default_store_path`
/// already panics under its own crate's `cfg(test)`, which does not cover a
/// *dependent* crate's tests (this one): when `kgraf-host` is the crate under
/// test, `kgraf-provider` is compiled as an ordinary, non-test dependency, so
/// its own guard never fires here. This wrapper adds the matching guard on
/// this crate's side of the same two call sites, so no test in this crate,
/// present or future, can reach the real directory through either of them:
/// `#[cfg(test)]` panics instead of resolving anything.
#[cfg(not(test))]
pub(crate) fn production_rate_limit_store_path() -> Option<PathBuf> {
    kgraf_provider::default_store_path()
}

/// [`production_rate_limit_store_path`]'s test build: see its doc comment.
#[cfg(test)]
pub(crate) fn production_rate_limit_store_path() -> Option<PathBuf> {
    panic!(
        "a test in kgraf-host resolved the real per-user rate limit store path; pass an \
         explicit store path instead (RunConfig::check_configured_with_store, or limiters's \
         store_path parameter)"
    );
}

/// Parse `--rate-limit ROUTE=RPM[/BURST][@MAX_WAIT_MS]`. A limit on a route the
/// run does not use is refused: an ignored parameter must raise.
///
/// Every limiter is built on the shared, cross-process store at `store_path`
/// (decision 0072). Both production callers
/// ([`RunConfig::check_configured_with_store`] and [`crate::Host::open`]) pass
/// [`production_rate_limit_store_path`], the one per-user file every kGraf
/// process on this machine shares; this function takes the path rather than
/// resolving it itself only so this crate's tests can each point at their own
/// temporary file instead of the real user's shared state.
/// There is no path here that falls back to a process-local `governor`
/// limiter: `RouteLimiter::new` (in-process only) exists solely for
/// `kgraf-provider`'s own tests and bench. If `store_path` is `None` (no
/// per-user data directory on this machine) or the store cannot be opened,
/// the run refuses to start rather than silently limiting only within this
/// process.
///
/// **Every spec is parsed and validated first, in one pass over `specs`, with
/// no disk access at all; the shared store is opened only once every spec has
/// passed**: an invalid `--rate-limit` (a bad route, an
/// unused route, a duplicate, a bad limit) must not have already created a
/// file on disk before it is reported.
pub(crate) fn limiters(
    specs: &[String],
    used: &BTreeSet<String>,
    store_path: Option<&Path>,
) -> Result<Limiters> {
    if specs.is_empty() {
        return Ok(Limiters::new());
    }
    let mut parsed = Vec::with_capacity(specs.len());
    let mut seen = BTreeSet::new();
    for spec in specs {
        let (route, limit) = spec.split_once('=').ok_or_else(|| {
            Error::Invalid(format!("`{spec}` is not ROUTE=RPM[/BURST][@MAX_WAIT_MS]"))
        })?;
        if !used.contains(route) {
            return Err(Error::Invalid(format!(
                "--rate-limit names route `{route}`, which no --route or --planner uses"
            )));
        }
        if !seen.insert(route.to_owned()) {
            return Err(Error::Invalid(format!(
                "--rate-limit gives route `{route}` twice"
            )));
        }
        let limit = RateLimit::parse(limit).lift()?;
        parsed.push((route.to_owned(), limit));
    }

    let path = store_path.ok_or_else(|| {
        Error::Invalid(
            "no per-user data directory is available on this machine, so a --rate-limit cannot \
             be enforced across processes"
                .into(),
        )
    })?;
    let store =
        Arc::new(SharedLimiterStore::open(path).map_err(|e| {
            Error::Invalid(format!("cannot open the shared rate limit store: {e}"))
        })?);
    let mut out = Limiters::new();
    for (route, limit) in parsed {
        out.insert(
            route.clone(),
            RouteLimiter::new_shared(route, limit, store.clone()).lift()?,
        );
    }
    Ok(out)
}

pub(crate) fn provider_for(
    route: &str,
    model: &str,
    limiters: &Limiters,
) -> Result<Arc<dyn Provider>> {
    provider_from(route, model, limiters, kgraf_core::legacy::env_var)
}

/// [`provider_for`] with the environment lookup given. A missing credential is
/// [`Error::Credential`]: a scheduler disables the schedule with that reason.
pub(crate) fn provider_from(
    route: &str,
    model: &str,
    limiters: &Limiters,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn Provider>> {
    let entry = kgraf_fleet::entry(route).settings()?;
    let credential = kgraf_fleet::credential_from(entry, lookup)
        .map_err(|e| Error::Credential(crate::error::Chained::bare(Box::new(e))))?;
    provider_with(route, &credential, model, limiters)
}

/// The route's provider from `credential`. A catalog entry that cannot be
/// configured is a settings failure: it fails the same way every time.
pub(crate) fn provider_with(
    route: &str,
    credential: &str,
    model: &str,
    limiters: &Limiters,
) -> Result<Arc<dyn Provider>> {
    let provider: Arc<dyn Provider> =
        Arc::from(kgraf_fleet::build(route, credential, vec![model.to_owned()]).settings()?);
    Ok(limited(provider, route, credential, limiters))
}

/// The route's limiter around a provider, when one is configured, and a route
/// recorder otherwise, so every call's ledger row names its route whether or not
/// the route is limited. The one place `--rate-limit` reaches a provider, so its
/// test covers the wiring.
///
/// Binds the shared limiter's key to `key_id(credential)` (never the
/// credential itself) before the provider it wraps can be called: a route's
/// credential does not change mid-run, so the first bind is the only one that
/// matters, and it always happens here, before `RateLimited` hands the
/// provider to any caller.
pub(crate) fn limited(
    provider: Arc<dyn Provider>,
    route: &str,
    credential: &str,
    limiters: &Limiters,
) -> Arc<dyn Provider> {
    match limiters.get(route) {
        Some(limiter) => {
            limiter.bind_key(&key_id(credential));
            Arc::new(RateLimited::new(provider, limiter.clone()))
        }
        None => Arc::new(RouteRecorded::new(provider, route)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decision 0088: a run file does not choose whether worktrees are kept.
    /// Without the key it loads with the default; with it, whatever the
    /// value, it is refused, naming where the choice is made, never ignored.
    /// A configuration serialized back out never carries the key.
    #[test]
    fn a_run_file_that_sets_worktrees_is_refused_not_ignored() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("run.json");
        let load = |body: &str| {
            std::fs::write(&path, body).expect("write");
            RunConfig::load("/project", &path)
        };
        let base = r#""routes": ["ollama:llama3"], "prices": "p.json""#;
        let plain = load(&format!("{{{base}}}")).expect("load");
        assert_eq!(plain.worktrees, WorktreeRetention::Delete);
        for value in ["\"keep\"", "\"delete\"", "\"sometimes\""] {
            let refused = load(&format!(r#"{{{base}, "worktrees": {value}}}"#))
                .expect_err("the key is refused")
                .to_string();
            assert!(
                refused.contains("worktrees")
                    && refused.contains("--keep-worktrees")
                    && refused.contains("Delete worker worktrees after use"),
                "{refused}"
            );
        }
        let mut keep = plain;
        keep.worktrees = WorktreeRetention::Keep;
        let text = serde_json::to_string(&keep).expect("json");
        assert!(!text.contains("worktrees"), "{text}");
    }

    #[test]
    fn a_loaded_file_takes_the_commands_defaults_and_resolves_prices_beside_it() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("run.json");
        std::fs::write(
            &path,
            r#"{"routes": ["ollama:llama3"], "prices": "prices.json"}"#,
        )
        .expect("write");
        let loaded = RunConfig::load("/project", &path).expect("load");
        let mut expected = RunConfig::new(
            "/project",
            vec!["ollama:llama3".into()],
            dir.path().join("prices.json"),
        );
        assert_eq!(loaded, expected);
        std::fs::write(
            &path,
            r#"{"routes": [], "prices": "/p", "cap_run": 1, "typo": 1}"#,
        )
        .expect("write");
        assert!(
            RunConfig::load("/project", &path).is_err(),
            "an unknown field is refused, never ignored"
        );
        expected.gate_timeout = Duration::from_secs(60);
        let round = serde_json::to_string(&expected).expect("json");
        assert!(round.contains(r#""gate_timeout":60"#), "{round}");
    }

    /// Decision 0080: kGraf does not limit spend. An old run file that still
    /// sets `cap_run`, `cap_task` or `cap_hour` loads without error, exactly
    /// as one with none set, since decision 0080 deprecated the fields but
    /// kept them in the format for this reason.
    #[test]
    fn an_old_file_with_deprecated_caps_still_loads() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("run.json");
        std::fs::write(
            &path,
            r#"{"routes": ["ollama:llama3"], "prices": "prices.json",
                "cap_run": 2.5, "cap_task": 0.5, "cap_hour": 1.0}"#,
        )
        .expect("write");
        let loaded = RunConfig::load("/project", &path).expect("an old cap never errors");
        assert_eq!(loaded.cap_run, Some(2.5));
        assert_eq!(loaded.cap_task, Some(0.5));
        assert_eq!(loaded.cap_hour, Some(1.0));
    }

    #[test]
    fn configured_checks_what_the_whole_process_needs() {
        // A store path under this test's own tempdir, never
        // `check_configured()`'s real per-user default. No assertion below may
        // be changed to call `check_configured()` directly.
        let dir = tempfile::tempdir().expect("dir");
        let prices = dir.path().join("prices.json");
        let store_path_buf = dir.path().join("rate-limits.sqlite3");
        let store_path = Some(store_path_buf.as_path());
        let mut c = RunConfig::new(dir.path(), vec![], &prices);
        assert_eq!(
            c.check_configured_with_store(store_path),
            Err(NotReady::NoRoutes)
        );
        c.routes = vec!["kimi".into()];
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::Invalid(_))
        ));
        c.routes = vec!["ollama:llama3".into()];
        c.rate_limits = vec!["kimi=60".into()];
        assert!(
            matches!(
                c.check_configured_with_store(store_path),
                Err(NotReady::Invalid(m)) if m.contains("kimi")
            ),
            "a limit on an unused route, rejected before the store is ever opened"
        );
        assert!(
            !store_path_buf.exists(),
            "an invalid --rate-limit must not have already created the store file"
        );
        c.rate_limits.clear();
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::PricesUnreadable { .. })
        ));
        std::fs::write(&prices, "{}").expect("write");
        assert_eq!(c.check_configured_with_store(store_path), Ok(()));
        // Decision 0080: kGraf does not limit spend, so a deprecated cap of
        // any value, even zero, is never itself a readiness failure.
        c.cap_task = Some(0.0);
        assert_eq!(c.check_configured_with_store(store_path), Ok(()));
        c.cap_task = None;
        c.point = Some(200);
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::Invalid(_))
        ));
        c.point = None;
        c.max_agents = 0;
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::Invalid(_))
        ));
        c.max_agents = 1;
        // Decision 0084: a zero shell timeout would kill every worker command.
        c.shell_timeout = Some(Duration::ZERO);
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::Invalid(m)) if m.contains("--shell-timeout")
        ));
        c.shell_timeout = Some(Duration::from_secs(1));
        assert_eq!(c.check_configured_with_store(store_path), Ok(()));

        // An empty acceptance or gate command passes everything; a zero gate
        // timeout kills everything.
        let original = c.clone();
        c.verify = "  ".into();
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::Invalid(m)) if m.contains("--verify")
        ));
        c = original.clone();
        c.gate_commands = vec!["cargo test".into(), String::new()];
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::Invalid(m)) if m.contains("--gate-command")
        ));
        c = original.clone();
        c.gate_timeout = Duration::ZERO;
        assert!(matches!(
            c.check_configured_with_store(store_path),
            Err(NotReady::Invalid(m)) if m.contains("--gate-timeout")
        ));
        assert_eq!(original.check_configured_with_store(store_path), Ok(()));
    }

    /// Decision 0084: `shell_timeout` in a run file is seconds, absent is
    /// `None` (the worker configuration decides), and it round-trips.
    #[test]
    fn a_run_files_shell_timeout_is_seconds_and_optional() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("run.json");
        std::fs::write(
            &path,
            r#"{"routes": ["kimi:model"], "prices": "p.json", "shell_timeout": 900}"#,
        )
        .expect("write");
        let c = RunConfig::load(dir.path(), &path).expect("load");
        assert_eq!(c.shell_timeout, Some(Duration::from_secs(900)));
        let back: RunConfig =
            serde_json::from_str(&serde_json::to_string(&c).expect("ser")).expect("de");
        assert_eq!(back.shell_timeout, Some(Duration::from_secs(900)));
        std::fs::write(&path, r#"{"routes": ["kimi:model"], "prices": "p.json"}"#).expect("write");
        assert_eq!(
            RunConfig::load(dir.path(), &path)
                .expect("load")
                .shell_timeout,
            None
        );
    }

    /// The public `check_configured()` (no rate limits
    /// configured) never touches the shared store at all, so it is safe to
    /// call directly even in a test; this is the one test allowed to call it,
    /// specifically to pin that "no rate limits configured" never opens
    /// anything.
    #[test]
    fn check_configured_with_no_rate_limits_never_touches_the_shared_store() {
        let dir = tempfile::tempdir().expect("dir");
        let prices = dir.path().join("prices.json");
        std::fs::write(&prices, "{}").expect("write");
        let c = RunConfig::new(dir.path(), vec!["kimi:model".into()], &prices);
        assert_eq!(c.rate_limits, Vec::<String>::new());
        assert_eq!(c.check_configured(), Ok(()));
    }

    /// A test that calls the *public*
    /// `check_configured()` (never `check_configured_with_store`) with a
    /// valid, used `--rate-limit` must not silently touch the user's real
    /// per-user data directory; `production_rate_limit_store_path` must
    /// panic instead.
    #[test]
    #[should_panic(expected = "resolved the real per-user rate limit store path")]
    fn check_configured_refuses_to_resolve_the_real_store_path_in_a_test() {
        let dir = tempfile::tempdir().expect("dir");
        let prices = dir.path().join("prices.json");
        std::fs::write(&prices, "{}").expect("write");
        let mut c = RunConfig::new(dir.path(), vec!["kimi:model".into()], &prices);
        c.rate_limits = vec!["kimi=60".into()];
        let _ = c.check_configured();
    }

    /// Decision 0089: generative work never reaches a non-generative route.
    #[test]
    fn a_kind_prefix_on_a_worker_or_planner_route_is_refused_naming_model_route() {
        let mut c = RunConfig::new("/p", vec!["embedding=openai:e".into()], "p.json");
        let err = c.planner_and_used().expect_err("refused").to_string();
        assert!(
            err.contains("--model-route") && err.contains("embedding"),
            "{err}"
        );
        c.routes = vec!["ollama:llama3".into()];
        c.planner = Some("typed-decision=systemone:jev-latest".into());
        assert!(c.planner_and_used().is_err());
        c.planner = Some("generative=ollama:llama3".into());
        assert!(
            c.planner_and_used().is_err(),
            "a redundant prefix is refused too"
        );
        c.planner = None;
        // An `=` inside a model name is not a prefix.
        c.routes = vec!["ollama:llama3=q4".into()];
        c.planner_and_used().expect("plain generative route");
    }

    #[test]
    fn a_model_route_must_name_a_non_generative_kind_once() {
        let mut c = RunConfig::new("/p", vec!["ollama:llama3".into()], "p.json");
        c.model_routes = vec!["openai:text-embedding-3-small".into()];
        let err = c.model_route_specs().expect_err("no kind").to_string();
        assert!(err.contains("typed-decision, embedding, rerank"), "{err}");
        c.model_routes = vec!["generative=ollama:llama3".into()];
        assert!(c.model_route_specs().is_err());
        c.model_routes = vec!["world=ollama:m".into()];
        assert!(
            c.model_route_specs().is_err(),
            "an unregistered kind raises"
        );
        c.model_routes = vec!["embedding=openai:e".into(), "embedding=openai:e".into()];
        assert!(c.model_route_specs().is_err(), "a repeat raises");
        c.model_routes = vec!["embedding=openai:e".into(), "rerank=llama-cpp:r".into()];
        let specs = c.model_route_specs().expect("parsed");
        assert_eq!(specs[0].kind, kgraf_core::ModelKind::EMBEDDING);
        assert_eq!(specs[1].route.route, "llama-cpp");
        assert_eq!(specs[1].to_string(), "rerank=llama-cpp:r");
        // The planner check runs it too, so `check_configured` and `open` refuse.
        c.model_routes = vec!["openai:e".into()];
        assert!(c.planner_and_used().is_err());
    }

    #[test]
    fn model_routes_load_from_a_run_file_and_are_left_out_when_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("run.json");
        std::fs::write(
            &path,
            r#"{"routes": ["ollama:llama3"], "prices": "p.json",
                "model_routes": ["embedding=ollama:nomic-embed-text"]}"#,
        )
        .expect("write");
        let loaded = RunConfig::load("/project", &path).expect("load");
        assert_eq!(
            loaded.model_routes,
            vec!["embedding=ollama:nomic-embed-text"]
        );
        let mut plain = loaded.clone();
        plain.model_routes.clear();
        let text = serde_json::to_string(&plain).expect("json");
        assert!(!text.contains("model_routes"), "{text}");
    }

    #[test]
    fn model_routes_are_built_with_their_own_credentials_and_offer_their_tools() {
        let specs = vec![
            ModelRouteSpec::parse("embedding=openai:text-embedding-3-small").expect("e"),
            ModelRouteSpec::parse("typed-decision=systemone:jev-latest").expect("d"),
        ];
        // No credential for the remote route: raised, naming the variable.
        let err = model_routes_from(&specs, &Limiters::new(), |_| None).expect_err("no key");
        assert!(matches!(err, Error::Credential(_)), "{err}");
        let routes = model_routes_from(&specs, &Limiters::new(), |name| {
            (name == "KGRAF_OPENAI_API_KEY").then(|| "sk-test-model-kinds-000000".to_owned())
        })
        .expect("built");
        assert_eq!(
            routes.kinds(),
            vec![
                kgraf_core::ModelKind::TYPED_DECISION,
                kgraf_core::ModelKind::EMBEDDING
            ]
        );
        let ledger = std::sync::Arc::new(kgraf_provider::Ledger::open_in_memory().expect("ledger"));
        let calls = kgraf_tools::ModelCalls::new(ledger, std::sync::Arc::new(routes));
        let names: Vec<String> = kgraf_tools::model_tools(&calls)
            .iter()
            .map(|t| t.name().to_owned())
            .collect();
        assert_eq!(names, vec!["decide", "embed"]);
        // A route that does not serve the kind is refused at build.
        let wrong = vec![ModelRouteSpec::parse("embedding=anthropic:m").expect("parse")];
        assert!(
            model_routes_from(&wrong, &Limiters::new(), |_| Some(
                "k-000000000000000000".into()
            ))
            .is_err()
        );
    }

    #[test]
    fn a_rate_limit_on_a_model_route_is_accepted_and_paces_that_route() {
        let dir = tempfile::tempdir().expect("dir");
        let store = dir.path().join("rate-limits.sqlite3");
        let prices = dir.path().join("prices.json");
        let mut c = RunConfig::new(dir.path(), vec!["ollama:llama3".into()], &prices);
        c.model_routes = vec!["embedding=tei:bge".into(), "rerank=tei:bge".into()];
        let (_, used) = c.planner_and_used().expect("used");
        assert!(used.contains("tei"), "{used:?}");
        let paced = limiters(&["tei=60".into()], &used, Some(&store)).expect("limiters");
        let routes = model_routes_from(&c.model_route_specs().expect("specs"), &paced, |_| None)
            .expect("built");
        assert_eq!(routes.iter().count(), 2);
        for route in routes.iter() {
            let limiter = route.limiter().expect("paced");
            assert!(Arc::ptr_eq(limiter, &paced["tei"]), "one shared limiter");
        }
        // A limit on a route neither kind of route uses is still refused.
        assert!(limiters(&["kimi=60".into()], &used, Some(&store)).is_err());
    }

    #[test]
    fn readiness_names_an_unserved_kind_an_unknown_route_and_a_missing_key() {
        use crate::host::check_model_routes;
        let spec = |s: &str| ModelRouteSpec::parse(s).expect("parse");
        let none = |_: &str| None;
        let err = check_model_routes(&[spec("embedding=anthropic:m")], Some(&none))
            .expect_err("not served");
        assert!(
            err.to_string().contains("does not serve embedding"),
            "{err}"
        );
        assert!(matches!(
            check_model_routes(&[spec("rerank=nowhere:m")], Some(&none)),
            Err(NotReady::UnknownRoute { .. })
        ));
        assert!(matches!(
            check_model_routes(&[spec("embedding=openai:e")], Some(&none)),
            Err(NotReady::MissingCredential { .. })
        ));
        check_model_routes(&[spec("typed-decision=systemone:jev-latest")], Some(&none))
            .expect("a local route needs no key");
    }
}
