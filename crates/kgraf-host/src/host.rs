//! [`Host`]: the construction of a running orchestrator, in the order
//! `kgraf orchestrate` did it, as steps a caller composes.
//!
//! ```text
//!   Host::open(config)                   prices, ledger, graph, context, routes,
//!     |                                  rate limits, the one Admission (no spend cap: 0080)
//!     |-- read_corpus / approve_plan     creates nothing in the repository
//!     |-- startup(notices) -> Startup    worktree manager, stale cleanup, stranded,
//!     |                                  dead MCP homes
//!     |-- decide_gate(...)               a held gate's retry or waiver
//!     `-- build(startup, run, plan)      integration branch, merge-back, worktrees,
//!           -> PreparedRun               worker config, providers, route probes,
//!                |                       services, orchestrator, gate runner,
//!                |                       contract checks (unless turned off)
//!                |-- attach_port / summary / shape
//!                |-- run(start) -> RunOutcome     report, then the ref sweep
//!                `-- into_orchestrator()          for a factory; `Host::finish_run`
//!                                                 does the sweep later
//! ```
//!
//! [`Host::prepare`] is `startup` then `build`; [`Host::start_run`] is `prepare`,
//! a port, and the run spawned, returning a [`RunHandle`]. The command calls the
//! steps one by one because it prints between them and records a gate decision
//! between `startup` and `build`.
//!
//! Every provider is built through `kgraf_fleet` and wrapped in the route's
//! rate limiter; every model call goes through the one [`Ledger`]. A
//! failure at any step returns [`Error`]; nothing is substituted.

use crate::assembler::{AttemptAssembler, ResolverOverride, RouteParts, route_envs};
use crate::config::{
    Limiters, RouteSpec, RunConfig, limiters, provider_for, read_prices, split_route,
};
use crate::error::{Error, NotReady, Result, ResultExt};
use crate::gates::{CommandGateRunner, GATE_OUTPUT_CAP, discover_validation_commands};
use crate::verifier::CommandVerifier;
use crate::worker::assemble::{clean_mcp_homes, state_read_denies};
use crate::worker::config::WorkerConfig;
use crate::workspaces::{
    GitWorkspaces, clean_stale_worktrees, ensure_integration_branch, state_dir,
};
use kgraf_core::{ParallelismLimits, PriceTable, RunId};
use kgraf_graph::{GraphStore, Indexer};
use kgraf_interpret::ApprovedPlan;
use kgraf_orchestrator::{
    Admission, AttemptEnvs, ChallengeTool, CheckBase, ContractChecks, ContractLine,
    DEFAULT_MAX_REVIEWS, DEFAULT_MAX_SUPERSESSIONS, DEFAULT_RECOMPUTE_RUNS, GateDecision,
    GitStubLander, Intake, IntakeParts, Limits, MachineGate, MessagePort, ModelAnswerer,
    ModelAsker, ModelCaller, ModelDecomposer, ModelGrounder, ModelReviewer, Orchestrator,
    OrchestratorConfig, OrchestratorError, OrchestratorFactory, OrchestratorHandle,
    ReadinessFailure, RoutedConsumerTestWriter, RoutedHandshaker, RoutedInterfaceReviewer,
    RunReport, RunStore, SandboxRunner, Services, SessionLine, SharedGraph, SystemClock,
    interpret_corpus, process_machine_gate,
};
use kgraf_provider::{Ledger, Provider, RateLimit};
use kgraf_runtime::{AskResolver, ContextConfig, PointSource, PrefixRegistry};
use kgraf_sandbox::{MergeBack, WorkspaceManager};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What building a run reports as it goes, in order. The command prints each
/// as one line; the shell shows them or keeps them ([`Host::take_notices`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// Startup cleanup removed what a dead process left.
    Cleaned(kgraf_sandbox::OrphanRemoval),
    /// A worktree nothing here can judge (another machine or PID namespace, an
    /// unreadable host); it stays until the owner runs `kgraf clean <name>`.
    Stranded { path: PathBuf, name: String },
    /// A dead process's MCP home was removed.
    CleanedMcpHome(PathBuf),
    /// A dead process's contract-check scratch directory was removed.
    CleanedCheckScratch(PathBuf),
    /// A cargo file above the worktrees that sandboxed commands may not read,
    /// with the cause (Linux): cargo in a worktree will fail on it
    /// with "Permission denied", and this says why before it does.
    AncestorReadRefused(String),
    /// `.kgraf/worker.json` is absent: no MCP servers. Said, not assumed.
    NoWorkerConfig(PathBuf),
    /// One MCP server's registration on one route, from the route probe.
    McpServer {
        route: String,
        server: String,
        report: kgraf_mcp::RegistrationReport,
    },
    /// The route's shared prefix differs from the one the previous run in this
    /// process used for the same vendor and model (another tool list, other MCP
    /// servers, contract checks turned on or off): its first calls miss the
    /// provider's prompt cache.
    PrefixChanged {
        route: String,
        change: kgraf_runtime::PrefixInvalidation,
    },
    /// A ledger call of the last hour counted toward the hourly cap at a
    /// worst-case estimate, not its price: its model is not in the price table,
    /// or it is unreconciled ([`Host::seed_trailing_hour`]).
    EstimatedSpend {
        call: String,
        model: String,
        amount: kgraf_core::Money,
        reason: String,
    },
    /// The graph was indexed for this build ([`Host::build_base`]) but has no
    /// nodes afterward: a fresh checkout in a language the indexer does not
    /// support, or a root with no source files at all. Workers still get no
    /// graph slices, exactly as before, but this says so where a person reads
    /// it, instead of only the orchestrator's own trace event.
    GraphEmpty { root: PathBuf, files_seen: u64 },
    /// The project's dependencies were not ready offline, and kGraf fetched
    /// them on the host with the user's consent (decision 0087): the one
    /// network access that is not a model provider or an MCP server.
    DependenciesFetched { registry: String, summary: String },
    /// The offline dependency check could not decide (a manifest cargo
    /// cannot read, say); the run goes on, and workers' builds will show the
    /// same error.
    DependenciesUnchecked(String),
}

/// How a prepared run begins.
#[derive(Debug)]
pub enum RunStart {
    /// Plan and run this instruction, verbatim.
    Instruction(String),
    /// Resume the run from its record.
    Resume,
    /// Run a plan the user approved ([`Host::approve_plan`]).
    Plan(Box<ApprovedPlan>),
}

impl RunStart {
    /// The approved plan, when this start is one; [`Host::build`] takes its gates.
    pub fn plan(&self) -> Option<&ApprovedPlan> {
        match self {
            RunStart::Plan(p) => Some(p.as_ref()),
            _ => None,
        }
    }
}

/// A corpus reading's inputs. Reading creates nothing in the
/// repository and makes at most one ledgered model call.
#[derive(Clone, Debug, Default)]
pub struct CorpusRequest {
    /// The instruction that leads the reading, verbatim; required in an empty
    /// directory.
    pub instruction: Option<String>,
    /// Corrections to the previous reading, in order.
    pub corrections: Vec<String>,
    /// The interpretation call's input bound in tokens; default 40% of the
    /// planner's effective window (0057).
    pub budget_tokens: Option<u64>,
}

/// The worktree manager after startup cleanup, which [`Host::build`] uses.
pub struct Startup {
    manager: Arc<WorkspaceManager>,
}

/// One configured route's capacity input (decision 0067), plus the base
/// URL a caller needs to check for a recent 429
/// (`kgraf_provider::limits::last_429`) when overlaying the learned rate.
#[derive(Debug, Clone)]
pub struct RouteCapacityInput {
    pub inputs: kgraf_capacity::RouteInputs,
    /// `None` only for a route whose catalog entry could not be resolved
    /// (see [`Host::route_capacity_inputs`]'s doc comment).
    pub base_url: Option<String>,
}

/// The process-wide part of hosting runs: everything in [`RunConfig`] resolved
/// once, and the one [`Admission`] every run shares (decision 0017's limits
/// bind work, not runs).
pub struct Host {
    config: RunConfig,
    state: PathBuf,
    /// The price table and the modification time it was read at; read again
    /// when the file changes ([`Host::prices`]), so a fixed table applies
    /// without a restart.
    prices: Mutex<(PriceTable, Option<std::time::SystemTime>)>,
    ledger: Arc<Ledger>,
    graph: SharedGraph,
    graph_path: PathBuf,
    context: ContextConfig,
    planner: RouteSpec,
    routes: Vec<RouteSpec>,
    limiters: Limiters,
    orchestrator_config: OrchestratorConfig,
    admission: Arc<Admission>,
    /// The shared, process-wide execution and agent-slot gate (decision
    /// 0067): every `Host` in this process shares one, so two open
    /// projects together cannot exceed the machine's own ceiling. Kept
    /// alongside `admission` (which already holds it internally, through
    /// `Admission::with_machine`) so `ShellTool::with_exec_gate` and the
    /// shell's slider (`Host::machine_gate`) can both reach it without
    /// `Admission` needing to expose its own internals for that purpose.
    machine_gate: Arc<MachineGate>,
    asks: Option<Arc<dyn AskResolver>>,
    /// Routes served by a provider the embedding process gave
    /// ([`Host::with_route_provider`]) instead of `kgraf_fleet`.
    in_process: BTreeMap<String, Arc<dyn Provider>>,
    /// Runs handed out by [`PreparedRun::into_orchestrator`], waiting for
    /// [`Host::finish_run`].
    ended: Mutex<BTreeMap<RunId, RunEnd>>,
    /// Notices of builds nobody was there to print ([`HostFactory`]).
    notices: Mutex<Vec<Notice>>,
    /// Longest a [`Host::prepare_blocking`] build may take.
    build_timeout: Duration,
    /// Measured per-model capability profiles (decision 0075), loaded once at
    /// open (a malformed file fails loud there); shared with every run's
    /// `Services` and updated in place by the orchestrator's live-recording
    /// hook.
    profiles: Arc<Mutex<kgraf_core::model_profile::ProfileStore>>,
    /// The package manager the dependency gate checks and fetches with
    /// (decision 0087); cargo unless a test gives a fake.
    deps_tool: Arc<dyn crate::deps::DependencyTool>,
    /// Runs a person allowed to fetch for (the desktop's "Allow once"),
    /// until [`Host::finish_run`].
    fetch_grants: Mutex<std::collections::BTreeSet<RunId>>,
    /// Each run's refetcher, until [`Host::finish_run`] stops it (a worker's
    /// fetch still running then is killed).
    refetchers: Mutex<BTreeMap<RunId, crate::deps::HostRefetch>>,
    /// The question each run stopped on for want of an answer about
    /// fetching, until [`Host::take_dependency_question`] (the desktop asks it).
    dependency_questions: Mutex<BTreeMap<RunId, crate::deps::ConsentQuestion>>,
}

/// Most dependency questions a host keeps for [`Host::take_dependency_question`].
const MAX_KEPT_QUESTIONS: usize = 32;

/// The default bound on one blocking build ([`Host::with_build_timeout`]). A
/// build is a git worktree, a route probe per route (every MCP server connects
/// and lists its tools) and the orchestrator; seconds when healthy. The bound
/// keeps a hung MCP server from holding the scheduler's tick, whose lease
/// renewal and stop check wait for it; it is far under the 15-minute lease.
pub const DEFAULT_BUILD_TIMEOUT: Duration = Duration::from_secs(120);

fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The rpm a route's capacity input reports, and which source bound it. The vendor's own header
/// must never *raise* the effective limit above what the user configured: a route capped at 6 rpm
/// whose vendor happens to report a header limit of 500 must still bind at 6, not 500 (about an 80x
/// increase in the agents the machine would admit against that route, entirely defeating the user's
/// own `--rate-limit`). The *tighter* (lower) of the two always wins, and the returned
/// [`kgraf_capacity::LimitSource`] names whichever one actually bound, so Settings' breakdown
/// states the truth rather than always crediting the header. A pure function, kept free of `Host`
/// and the process-wide header registry, so it has a direct, race-free test.
pub(crate) fn tighter_rpm(
    header_rpm: Option<u32>,
    configured_rpm: Option<u32>,
) -> Option<(u32, kgraf_capacity::LimitSource)> {
    match (header_rpm, configured_rpm) {
        (Some(h), Some(c)) if c <= h => Some((c, kgraf_capacity::LimitSource::Configured)),
        (Some(h), Some(_)) => Some((h, kgraf_capacity::LimitSource::Header)),
        (Some(h), None) => Some((h, kgraf_capacity::LimitSource::Header)),
        (None, Some(c)) => Some((c, kgraf_capacity::LimitSource::Configured)),
        (None, None) => None,
    }
}

/// The starting `ParallelismLimits` a freshly opened process seeds its one
/// [`MachineGate`] with (decision 0067): `config.max_agents` for the
/// agent-slot and writer bounds (the same starting point `Limits::new` uses
/// today), and this machine's own core count for `exec_slots`, since that
/// bound is cores-and-memory-based, not configured (design, "Cores do not
/// bound agents"). Only the *first* `Host` opened in a process actually seeds
/// the gate ([`process_machine_gate`] is a singleton); every later `Host`'s
/// own value here is ignored in favor of the one already open. The shell's
/// re-measure thread replaces these numbers with `kgraf-capacity::compute`'s
/// real ones through `MachineGate::set_limits`, without reopening the gate.
fn default_machine_limits(config: &RunConfig) -> ParallelismLimits {
    let agents = u32::try_from(config.max_agents).unwrap_or(u32::MAX).max(1);
    let exec_slots = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    ParallelismLimits::derive(agents, agents, exec_slots)
}

impl Host {
    /// Resolve `config`: read the price table, open the ledger and the graph,
    /// validate the context settings, the routes and the rate limits (no spend cap: 0080),
    /// and create the process's admission. Creates `.kgraf` and nothing
    /// else in the repository. Builds no provider, so needs no credential yet.
    ///
    /// Resolves the shared rate limit store's path lazily, same reasoning as
    /// [`RunConfig::check_configured`]: an empty `rate_limits` (most runs)
    /// must not pay [`crate::config::production_rate_limit_store_path`]'s
    /// test-mode panic just because `open` happened to be called.
    pub fn open(config: RunConfig) -> Result<Self> {
        let store_path = if config.rate_limits.is_empty() {
            None
        } else {
            crate::config::production_rate_limit_store_path()
        };
        Self::open_with_rate_limit_store(config, store_path.as_deref())
    }

    /// [`Self::open`] with the shared rate limit store's path given
    /// explicitly. `pub`, not `pub(crate)`, and feature-gated (`test-support`,
    /// same reasoning as `accounting.rs`'s `WriterFaults`): so a *dependent*
    /// crate's own tests (`kgraf-cli`'s
    /// `orchestrate_cmd::the_command_and_the_library_build_the_same_run`,
    /// which needs a real `Host` with real rate-limit wiring, not a stub) can
    /// point at their own temporary file instead of resolving the user's
    /// real per-user directory. Production wiring always calls [`Self::open`]
    /// instead, never this. Shares [`Self::open`]'s process-wide
    /// [`MachineGate`], since a caller of this seam (real rate-limit wiring)
    /// still wants the real, shared gate; see
    /// [`Self::open_with_machine_gate`] for a caller that wants its own.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn open_with_rate_limit_store(
        config: RunConfig,
        store_path: Option<&Path>,
    ) -> Result<Self> {
        let gate = process_machine_gate(default_machine_limits(&config));
        Self::open_impl(config, store_path, gate)
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn open_with_rate_limit_store(config: RunConfig, store_path: Option<&Path>) -> Result<Self> {
        let gate = process_machine_gate(default_machine_limits(&config));
        Self::open_impl(config, store_path, gate)
    }

    /// [`Self::open`] with both the rate limit store's path and the
    /// [`MachineGate`] given explicitly. `test-support`-gated for the same
    /// reason [`Self::open_with_rate_limit_store`] is: this crate's own
    /// tests, and `kgraf-shell`'s, open more than one `Host` in one test
    /// binary (`cargo test` runs a crate's tests as threads in one process),
    /// so each test that cares about admission limits passes its own
    /// `MachineGate::new(...)` here instead of coupling to
    /// [`process_machine_gate`]'s process-wide singleton and to whichever
    /// test happened to seed it first. Production wiring (the app, the CLI)
    /// always calls [`Self::open`] instead, which shares one process-wide
    /// gate across every project by construction (decision 0067:
    /// the slider binds this app process).
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn open_with_machine_gate(
        config: RunConfig,
        store_path: Option<&Path>,
        gate: Arc<MachineGate>,
    ) -> Result<Self> {
        Self::open_impl(config, store_path, gate)
    }

    fn open_impl(
        config: RunConfig,
        store_path: Option<&Path>,
        gate: Arc<MachineGate>,
    ) -> Result<Self> {
        if config.routes.is_empty() {
            return Err(Error::Invalid(
                "give at least one --route route:model".into(),
            ));
        }
        let state = state_dir(&config.root)?;
        let prices_read_at = modified(&config.prices);
        let prices = read_prices(&config.prices)?;
        let ledger =
            Arc::new(Ledger::open(&state.join("ledger.db")).step("cannot open the ledger")?);
        let graph_path = state.join("graph.db");
        let graph = Arc::new(Mutex::new(
            GraphStore::open(&graph_path, &config.root).step("cannot open the graph")?,
        ));
        graph
            .lock()
            .map_err(|_| Error::failed("graph lock poisoned"))?
            .node_count()
            .lift()?;
        let mut context = ContextConfig::for_model(config.window).lift()?;
        if let Some(point) = config.point {
            context = context.with_point(point, PointSource::User).lift()?;
        }
        let (planner, used) = config.planner_and_used()?;
        let limiters = limiters(&config.rate_limits, &used, store_path)?;
        config.check_bounds()?;
        let orchestrator_config = OrchestratorConfig {
            step_budget: config.steps,
            // Decision 0079; the reviewer itself is bound in `run`.
            interface_review: config.interface_review,
            routing: config.routing,
            ..OrchestratorConfig::default()
        };
        // Decision 0075: loaded once here, so a malformed store fails loud at
        // open rather than degrading routing silently; a missing file is an
        // empty store (`ProfileStore::load`'s own rule).
        let profiles = Arc::new(Mutex::new(
            kgraf_core::model_profile::ProfileStore::load(&state)
                .step("cannot read the measured model-capability profiles")?,
        ));
        // The one admission of this process: every run it starts shares it,
        // and it is additionally reserved against `gate`, the shared,
        // process-wide `MachineGate` (decision 0067). `Self::open`
        // passes `process_machine_gate(...)`, one singleton per process, so
        // every project's `Host` in the app or the CLI shares one ceiling
        // (the slider binds this app process); a test passes its
        // own fresh `MachineGate::new(...)` through
        // `Self::open_with_machine_gate` instead, so test order never
        // couples one test's admission limits to another's.
        let admission = Admission::with_machine(
            Limits::new(config.max_items, config.max_agents).lift()?,
            gate.clone(),
        );
        let routes = config
            .routes
            .iter()
            .map(|s| RouteSpec::parse(s))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            config,
            state,
            prices: Mutex::new((prices, prices_read_at)),
            ledger,
            graph,
            graph_path,
            context,
            planner,
            routes,
            limiters,
            orchestrator_config,
            admission,
            machine_gate: gate,
            asks: None,
            in_process: BTreeMap::new(),
            ended: Mutex::new(BTreeMap::new()),
            notices: Mutex::new(Vec::new()),
            build_timeout: DEFAULT_BUILD_TIMEOUT,
            profiles,
            deps_tool: Arc::new(crate::deps::CargoDeps::default()),
            fetch_grants: Mutex::new(std::collections::BTreeSet::new()),
            refetchers: Mutex::new(BTreeMap::new()),
            dependency_questions: Mutex::new(BTreeMap::new()),
        })
    }

    /// Bound each [`Host::prepare_blocking`] build (default
    /// [`DEFAULT_BUILD_TIMEOUT`]). A build past it fails as a step, which the
    /// scheduler counts as transient. Its probe worktree, if any, belongs to
    /// this live process and is removed by the next process's startup cleanup.
    pub fn with_build_timeout(mut self, timeout: Duration) -> Self {
        self.build_timeout = timeout;
        self
    }

    /// Check and fetch dependencies with `tool` instead of cargo (decision
    /// 0087). For tests, so none contacts a registry; also the seam a later
    /// package manager plugs into.
    pub fn with_dependency_tool(mut self, tool: Arc<dyn crate::deps::DependencyTool>) -> Self {
        self.deps_tool = tool;
        self
    }

    /// This project's consent store (decision 0087).
    pub fn dependency_consents(&self) -> crate::deps::ConsentStore {
        crate::deps::ConsentStore::in_state_dir(&self.state)
    }

    /// Store the project's answer about fetching from `registry` ("Always
    /// for this project" stores [`crate::deps::Consent::Allowed`]).
    pub fn set_dependency_consent(
        &self,
        registry: crate::deps::Registry,
        consent: crate::deps::Consent,
    ) -> Result<()> {
        Ok(self.dependency_consents().set(registry, consent)?)
    }

    /// Allow fetching for `run` only, storing nothing: the desktop's "Allow
    /// once". Covers the run's start and its workers, until
    /// [`Host::finish_run`].
    pub fn grant_dependency_fetch(&self, run: &RunId) {
        self.fetch_grants
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(run.clone());
    }

    /// The question `run` stopped on for want of an answer about fetching
    /// its dependencies, once: a caller whose start failed through a seam
    /// that keeps only the error's words (the orchestrator factory) asks it,
    /// then starts again.
    pub fn take_dependency_question(&self, run: &RunId) -> Option<crate::deps::ConsentQuestion> {
        self.dependency_questions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run)
    }

    /// This run's own answer: the configured flag, else a grant, else none
    /// (the stored consent decides).
    fn run_fetch(&self, run: &RunId) -> crate::deps::RunFetch {
        use crate::deps::RunFetch;
        match self.config.dependency_fetch {
            RunFetch::Stored
                if self
                    .fetch_grants
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains(run) =>
            {
                RunFetch::Allow
            }
            other => other,
        }
    }

    /// The dependency gate (decision 0087), before anything of the run is
    /// built and so before any worker is dispatched: the offline check, and a
    /// fetch on the host only with consent. Every entry point that starts a
    /// run passes here ([`Host::build`], [`Host::prepare_blocking`]).
    ///
    /// Blocking: a cargo process, about 245 ms on a cached 800-package
    /// workspace; a fetch takes as long as the network does.
    pub fn ensure_dependencies(
        &self,
        run: &RunId,
        notices: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<()> {
        use crate::deps::{FetchSite, GateOutcome, Trigger, ensure};
        let outcome = ensure(
            self.deps_tool.as_ref(),
            &FetchSite::project(&self.config.root),
            &self.dependency_consents(),
            self.run_fetch(run),
            Trigger::RunStart,
        )
        .inspect_err(|e| {
            if let Some(q) = e.consent_question() {
                let mut questions = self
                    .dependency_questions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                // Only the desktop takes these; a scheduled run's question is
                // never taken, so keep a bounded few, dropping the lowest run
                // id first. A dropped question is asked again at the next start.
                while questions.len() >= MAX_KEPT_QUESTIONS {
                    questions.pop_first();
                }
                questions.insert(run.clone(), q);
            }
        })?;
        match outcome {
            GateOutcome::NotApplicable | GateOutcome::Ready => {}
            GateOutcome::Fetched(report) => notices(Notice::DependenciesFetched {
                registry: report.registry.name().to_owned(),
                summary: report.summary(),
            }),
            GateOutcome::Unchecked(why) => notices(Notice::DependenciesUnchecked(why)),
        }
        Ok(())
    }

    /// `run`'s refetcher, while the run is not finished.
    #[cfg(test)]
    pub(crate) fn run_refetcher(&self, run: &RunId) -> Option<crate::deps::HostRefetch> {
        self.refetchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run)
            .cloned()
    }

    /// The refetcher every worker environment of `run` gets (decision 0087).
    fn dependency_refetch(&self, run: &RunId) -> Arc<dyn kgraf_tools::DependencyRefetch> {
        // One per run: a second build of the same run shares the first's, so
        // its bounds and its stop cover every worker of the run.
        let refetch = self
            .refetchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(run.clone())
            .or_insert_with(|| {
                crate::deps::HostRefetch::new(
                    self.deps_tool.clone(),
                    self.dependency_consents(),
                    self.run_fetch(run),
                    self.config.root.clone(),
                )
            })
            .clone();
        Arc::new(refetch)
    }

    /// The price table, read again first when its file changed since it was
    /// last read. A table that no longer reads fails; the old one is not used
    /// in its place.
    pub fn prices(&self) -> Result<PriceTable> {
        let mut cached = self.prices.lock().unwrap_or_else(|p| p.into_inner());
        let now = modified(&self.config.prices);
        if now != cached.1 {
            let table = read_prices(&self.config.prices)?;
            *cached = (table, now);
        }
        Ok(cached.0.clone())
    }

    /// Runs handed out by [`PreparedRun::into_orchestrator`] whose
    /// [`Host::finish_run`] has not run yet.
    pub fn unfinished_runs(&self) -> usize {
        self.ended.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Every attempt's approval asks go to `asks` instead of the persisted-rule
    /// resolver, which denies what no rule allows (the command's delegates are
    /// unattended). The desktop shell passes its box's desk here.
    pub fn with_ask_resolver(mut self, asks: Arc<dyn AskResolver>) -> Self {
        self.asks = Some(asks);
        self
    }

    /// Serve `route` with `provider`, built by the embedding process, instead of a
    /// `kgraf_fleet` provider: a fake model in a test. Behind the
    /// `in-process-provider` feature, which only tests enable. The route's rate
    /// limiter still wraps it and every call still goes through the host's
    /// ledger. The route needs no catalog entry and no credential; its models
    /// still need prices. Refused for a route the catalog knows (a real route
    /// must not be replaced) and for a route no configured `route:model` uses
    /// (the provider would be silently unused).
    #[cfg(any(test, feature = "in-process-provider"))]
    pub fn with_route_provider(mut self, route: &str, provider: Arc<dyn Provider>) -> Result<Self> {
        if kgraf_fleet::entry(route).is_ok() {
            return Err(Error::Invalid(format!(
                "route `{route}` is in the provider catalog; an in-process provider takes a \
                 route name of its own"
            )));
        }
        if !self.route_set().iter().any(|r| r.route == route) {
            return Err(Error::Invalid(format!(
                "an in-process provider for route `{route}`, which no route or planner uses"
            )));
        }
        self.in_process.insert(route.to_owned(), provider);
        Ok(self)
    }

    /// The route's provider behind its limiter: the in-process one, or one from
    /// `kgraf_fleet` (which needs the route's credential).
    fn provider(&self, route: &str, model: &str) -> Result<Arc<dyn Provider>> {
        match self.in_process.get(route) {
            // An in-process test provider has no real credential; the route
            // name stands in for one so its key id is at least stable.
            Some(p) => Ok(crate::config::limited(
                p.clone(),
                route,
                route,
                &self.limiters,
            )),
            None => provider_for(route, model, &self.limiters),
        }
    }

    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// `.kgraf` under the root.
    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    /// The worker routes, parsed.
    pub fn routes(&self) -> &[RouteSpec] {
        &self.routes
    }

    /// The orchestrator's own route.
    pub fn planner(&self) -> &RouteSpec {
        &self.planner
    }

    /// The planner and every worker route: the set [`Host::check_ready`] needs
    /// for a run built from this host.
    pub fn route_set(&self) -> Vec<RouteSpec> {
        let mut set = vec![self.planner.clone()];
        for r in &self.routes {
            if !set.contains(r) {
                set.push(r.clone());
            }
        }
        set
    }

    /// The process's one admission: the settings surface's concurrency cap is
    /// applied here, and every run built by this host takes capacity and money
    /// through it.
    pub fn admission(&self) -> &Arc<Admission> {
        &self.admission
    }

    /// Every configured route's capacity input (decision 0067, "The
    /// per-machine maximum", point 1: the slider maximum
    /// comes from the loaded models and their rate limits as well as
    /// this machine), for `kgraf-capacity::compute`. Deduplicated by route
    /// name (`route_set` keeps the planner's own model too, which does not
    /// matter here: a route's rate limit does not vary by model).
    ///
    /// rpm/tpm preference order, as the design states: header-observed
    /// (`kgraf_provider::limits::get`), then configured (`self.rate_limit`),
    /// then neither (`kgraf-capacity`'s own "unmeasured" fallback). The
    /// learned (429-adjusted) rate is not applied here: it decays over time
    /// independently of any one `Host`, so the caller (the shell's
    /// re-measure thread, which polls every open project) owns that state
    /// and overlays it on what this method returns.
    ///
    /// **Known limitation, stated not hidden.** `round_ms` and
    /// `tokens_per_round` come from [`Ledger::route_round_stats`], which
    /// groups by the wire adapter (`CallRecord.adapter`, for example
    /// `"openai-chat-completions"`), not by this catalog route id: the
    /// ledger schema has no route column. For a route whose catalog id
    /// happens to equal its wire adapter name, this is exact; for others it
    /// returns "not enough rounds yet" (`None`), the same honest default an
    /// unmeasured route already gets, never a wrong number.
    pub fn route_capacity_inputs(&self) -> Vec<RouteCapacityInput> {
        let mut seen = std::collections::BTreeSet::new();
        self.route_set()
            .into_iter()
            .filter(|r| seen.insert(r.route.clone()))
            .map(|r| self.one_route_capacity_input(&r))
            .collect()
    }

    fn one_route_capacity_input(&self, route: &RouteSpec) -> RouteCapacityInput {
        let Ok(entry) = kgraf_fleet::entry(&route.route) else {
            // A route this host's own `RunConfig` already validated at
            // `open` should always resolve; kept as a graceful "unmeasured"
            // fallback rather than a panic, in case a future catalog change
            // ever makes this reachable.
            return RouteCapacityInput {
                inputs: kgraf_capacity::RouteInputs::unmeasured(route.route.clone()),
                base_url: None,
            };
        };
        if !entry.requires_credential {
            return RouteCapacityInput {
                inputs: kgraf_capacity::RouteInputs {
                    route: route.route.clone(),
                    rpm: None,
                    tpm: None,
                    round_ms: None,
                    tokens_per_round: None,
                    // No configured parallel-slot count exists in
                    // `RunConfig` yet; `Some(0)` is "local, unconfigured",
                    // which `kgraf-capacity::compute` clamps up to 1
                    // (design: "and 1 when none is configured").
                    local_slots: Some(0),
                },
                base_url: Some(entry.base_url.to_owned()),
            };
        }
        let header = kgraf_provider::limits::get(entry.base_url);
        let configured = self.rate_limit(&route.route);
        let rpm = tighter_rpm(
            header.and_then(|o| o.requests_limit),
            configured.map(|c| c.requests_per_minute),
        );
        let tpm = header
            .and_then(|o| o.tokens_limit)
            .map(|n| (n, kgraf_capacity::LimitSource::Header));
        let stats = self.ledger.route_round_stats(&route.route).ok();
        RouteCapacityInput {
            inputs: kgraf_capacity::RouteInputs {
                route: route.route.clone(),
                rpm,
                tpm,
                round_ms: stats.as_ref().and_then(|s| s.round_ms),
                tokens_per_round: stats.as_ref().and_then(|s| s.tokens_per_round),
                local_slots: None,
            },
            base_url: Some(entry.base_url.to_owned()),
        }
    }

    /// The shared, process-wide machine gate `admission` is bound to
    /// (decision 0067): the shell's parallelism slider calls
    /// [`MachineGate::set_limits`] on this when the user moves it, or when
    /// the re-measure thread's `kgraf-capacity::compute` changes the
    /// maximum. Every open project's `Host` in this process shares the same
    /// one (`Self::open`).
    pub fn machine_gate(&self) -> &Arc<MachineGate> {
        &self.machine_gate
    }

    /// The ledger every call of every run goes through.
    pub fn ledger(&self) -> &Arc<Ledger> {
        &self.ledger
    }

    fn current_prices(&self) -> Result<PriceTable, NotReady> {
        self.prices().map_err(|e| NotReady::PricesUnreadable {
            path: self.config.prices.clone(),
            detail: format!("{e:#}"),
        })
    }

    /// Whole-process readiness: [`RunConfig::check_configured`], and every
    /// route of the host in the catalog with its model in the price table (read
    /// again when it changed). None of this is one schedule's fault, so a
    /// scheduler reports it without claiming (`TickEvent::NotRunnable`).
    pub fn check_configured(&self) -> Result<(), NotReady> {
        self.config.check_configured()?;
        let prices = self.current_prices()?;
        check_routes(&self.route_set(), Some(&prices), &self.in_process, None)
    }

    /// The credential of each route of the set, from the environment: the one
    /// readiness fact that can differ between runs of a configured host (a key
    /// removed from the shell), so the per-schedule check.
    pub fn check_credentials(&self, routes: &[RouteSpec]) -> Result<(), NotReady> {
        let lookup = |v: &str| kgraf_core::legacy::env_var(v);
        check_routes(routes, None, &self.in_process, Some(&lookup))
    }

    /// Readiness of a route set: each route is in the catalog and has its
    /// credential in the environment, and the price table (read again when it
    /// changed) prices each model. Builds nothing and calls nothing.
    pub fn check_ready(&self, routes: &[RouteSpec]) -> Result<(), NotReady> {
        let prices = self.current_prices()?;
        let lookup = |v: &str| kgraf_core::legacy::env_var(v);
        check_routes(routes, Some(&prices), &self.in_process, Some(&lookup))
    }

    fn plan_limits(&self) -> kgraf_interpret::PlanLimits {
        kgraf_interpret::PlanLimits {
            max_items: self.orchestrator_config.max_contracts_per_plan,
        }
    }

    /// The planner's caller for `run`: its calls are attributed to the run.
    fn caller(&self, run: &RunId) -> Result<ModelCaller> {
        self.planner_caller(&format!("orchestrator:{}", run.as_str()), &run.to_string())
    }

    /// A planner-route caller for a ledgered call outside any run:
    /// question routing (the classification call
    /// and the orchestrator answering a question itself), which needs a
    /// provider and the ledger but not a whole `Orchestrator` or run id.
    /// `agent` and `task` are the ledger attribution the caller chooses in
    /// their place.
    pub fn planner_caller(&self, agent: &str, task: &str) -> Result<ModelCaller> {
        Ok(ModelCaller {
            provider: self.provider(&self.planner.route, &self.planner.model)?,
            ledger: self.ledger.clone(),
            model: self.planner.model.clone(),
            max_output_tokens: self.config.planner_max_tokens,
            agent: agent.to_owned(),
            task: task.to_owned(),
        })
    }

    /// The contract checks of `run` (0030, 0031, 0035-0037), and their scratch
    /// directory:
    ///
    /// - **Runner**: the orchestrator's `SandboxRunner` over the run's
    ///   integration branch, so a check sees what a fresh worktree starts from
    ///   (the orchestrator moves it as merge-back advances the branch). Same
    ///   confinement as the gates: network denied, writes only in the check's own
    ///   directory, reads of the state directory, the ledger and the transcripts
    ///   denied (the gates' `read_denies`), and partial isolation refused, as the
    ///   sandbox refuses it for gates. Test runs and compiles are bounded
    ///   apart (`check_timeout`, `check_compile_timeout`).
    /// - **Handshaker**: each party's statement is made by the model of the
    ///   route its first attempt runs on (0030), through the
    ///   host's ledger, attributed to `orchestrator:<run>` with the run as task:
    ///   the orchestrator asks, so the call is its own spend.
    /// - **Reviewer**: the planner's model, the same way. The orchestrator
    ///   reserves each call for accounting before it is made (no spend cap: 0080).
    /// - **Artifacts** a challenge quotes are read from the runner's base commit
    ///   through hardened git, not from the checkout.
    /// - Caps: the orchestrator's defaults (supersessions, paid reviews per
    ///   version, recompute runs).
    /// - **Stub lander** (0081): built by the caller over the same
    ///   `manager`/`merge_back` `build_base` already opened for this run's own
    ///   merge-back, so a landed stub gets every protection a worker's attempt
    ///   gets (protected paths refused, attributes pinned to the trusted tree)
    ///   with nothing new to keep in sync. The caller keeps it for the
    ///   end-of-run sweep of what a refused land left (`RunEnd::finish`).
    fn contract_checks(
        &self,
        run: &RunId,
        integration: &str,
        read_denies: &[PathBuf],
        routes: &BTreeMap<String, (Arc<dyn Provider>, String)>,
        stub_lander: &Arc<GitStubLander>,
    ) -> Result<(ContractChecks, tempfile::TempDir)> {
        let root = self.config.root.clone();
        // The orchestrator refuses a runner that does not deny the run
        // directory, and a deny is resolved, so the directory must exist.
        let runs = self.state.join("runs");
        std::fs::create_dir_all(&runs).with_step(|| format!("cannot create {}", runs.display()))?;
        let scratch = check_scratch(run)?;
        let runner = SandboxRunner::new(
            CheckBase::Commit {
                repo: root.clone(),
                // The full ref: a remote named `kgraf` cannot make it ambiguous.
                rev: format!("refs/heads/{integration}"),
            },
            scratch.path().to_path_buf(),
            read_denies.to_vec(),
        )
        .step("cannot set up the contract checks")?
        .with_timeout(self.config.check_timeout)
        .with_compile_timeout(self.config.check_compile_timeout);
        // Built twice (handshaker, consumer-test writer): `ModelCaller` is not
        // `Clone` (it carries a raw `Arc<dyn Provider>` and `Arc<Ledger>`, cheap
        // to construct again, not worth a shared-ownership redesign for one
        // more caller).
        let model_routes = |max_output_tokens: u32| -> BTreeMap<String, ModelCaller> {
            routes
                .iter()
                .map(|(name, (provider, model))| {
                    (
                        name.clone(),
                        ModelCaller {
                            provider: provider.clone(),
                            ledger: self.ledger.clone(),
                            model: model.clone(),
                            max_output_tokens,
                            agent: format!("orchestrator:{}", run.as_str()),
                            task: run.to_string(),
                        },
                    )
                })
                .collect()
        };
        let checks = ContractChecks {
            interfaces: Vec::new(),
            runner: Arc::new(runner),
            handshaker: Arc::new(RoutedHandshaker {
                routes: model_routes(self.config.planner_max_tokens),
            }),
            reviewer: Arc::new(ModelReviewer(self.caller(run)?)),
            max_supersessions: DEFAULT_MAX_SUPERSESSIONS,
            max_reviews: DEFAULT_MAX_REVIEWS,
            recompute_runs: DEFAULT_RECOMPUTE_RUNS,
            // 0076: each consumer writes its own test of what it expects from
            // the producer, on the same per-route models as the handshake.
            consumer_test_writer: Some(Arc::new(RoutedConsumerTestWriter {
                routes: model_routes(self.config.planner_max_tokens),
            })),
            stub_lander: stub_lander.clone(),
        };
        Ok((checks, scratch))
    }

    /// Read the corpus (with the instruction and corrections, if any).
    /// Creates nothing in the repository: no branch, no worktree. The outer error
    /// is the planner's provider failing to build; the reading's own outcome, and
    /// the ledger calls already charged, are in the [`Intake`].
    pub async fn read_corpus(&self, run: &RunId, request: CorpusRequest) -> Result<Intake> {
        let asker = ModelAsker {
            caller: self.caller(run)?,
            route: self.planner.route.clone(),
        };
        let mut interpret =
            kgraf_interpret::InterpretRequest::new(kgraf_interpret::Asker::identity(&asker));
        interpret.instruction = request.instruction;
        interpret.corrections = request.corrections;
        // 0057: the call is bounded like a worker block, 40% of the planner's
        // effective window (`--window`, `--point`).
        interpret.budget_tokens = request
            .budget_tokens
            .unwrap_or(self.context.worker_plan_target_tokens());
        interpret.limits = self.plan_limits();
        let clock = SystemClock;
        let prices = self.prices()?;
        Ok(interpret_corpus(
            IntakeParts {
                graph: &self.graph,
                ledger: &self.ledger,
                prices: &prices,
                admission: &self.admission,
                clock: &clock,
            },
            run,
            interpret,
            &asker,
            &kgraf_interpret::NoTestRuns,
        )
        .await)
    }

    /// Decision 0064: run `f` on the project's graph under its lock, for the
    /// presumed-node reads and writes (`kgraf_interpret::presumed`) that go with
    /// a corpus reading. No model call is made here.
    pub fn with_graph<T>(&self, f: impl FnOnce(&kgraf_graph::GraphStore) -> T) -> Result<T> {
        let g = self
            .graph
            .lock()
            .map_err(|_| Error::failed("graph lock poisoned"))?;
        Ok(f(&g))
    }

    /// Approve the stored reading `plan_id`, with the proposed
    /// validation commands named in `accept_commands` as gates. No model call; an
    /// id with no stored reading is refused. Creates nothing in the repository.
    pub fn approve_plan(&self, plan_id: &str, accept_commands: &[String]) -> Result<ApprovedPlan> {
        let g = self
            .graph
            .lock()
            .map_err(|_| Error::failed("graph lock poisoned"))?;
        kgraf_interpret::approve_stored(
            &g,
            plan_id,
            accept_commands,
            &self.plan_limits(),
            &kgraf_interpret::GatherOptions::default(),
            &kgraf_interpret::NoTestRuns,
        )
        .lift()
    }

    /// Open the worktree manager and clean up after dead processes (0056):
    /// their worktrees no run record names, and their MCP homes. Work of live
    /// processes is never touched, so this is safe while other runs of this
    /// process are live.
    pub fn startup(&self, notices: &mut (dyn FnMut(Notice) + Send)) -> Result<Startup> {
        let manager = Arc::new(
            WorkspaceManager::open(&self.config.root, &self.state.join("worktrees"))
                .step("cannot open the worktree manager")?,
        );
        for removed in clean_stale_worktrees(&manager, &RunStore::new(self.graph.clone()))? {
            notices(Notice::Cleaned(removed));
        }
        // Never silent: work nothing here can judge (another machine or PID
        // namespace, an unreadable host) stays until a person removes it.
        for path in manager.stranded().lift()? {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            notices(Notice::Stranded { path, name });
        }
        for removed in clean_mcp_homes(&self.state.join("mcp-home")).lift()? {
            notices(Notice::CleanedMcpHome(removed));
        }
        for removed in sweep_check_scratch() {
            notices(Notice::CleanedCheckScratch(removed));
        }
        // Read the user's private group again for this run: a long-lived
        // process (the tray host) must not keep a group change it cached at
        // its start.
        #[cfg(unix)]
        kgraf_sandbox::trust::refresh_private_group();
        // The same walk the gates, verifier and shell make, once, up front.
        // A failure to make it is itself said: every sandboxed command would
        // fail the same way.
        let probe = kgraf_sandbox::SandboxPolicy::new(
            vec![manager.base().to_path_buf()],
            kgraf_sandbox::Network::Deny,
        )
        .with_ancestor_reads(manager.repo().to_path_buf());
        match probe.ancestor_refusals() {
            Ok(refused) => refused
                .into_iter()
                .for_each(|r| notices(Notice::AncestorReadRefused(r))),
            Err(e) => notices(Notice::AncestorReadRefused(format!(
                "cannot check the cargo files above the worktrees: {e}"
            ))),
        }
        Ok(Startup { manager })
    }

    /// Record the user's decision about a held or failed gate of `run`, before
    /// the run resumes.
    pub fn decide_gate(
        &self,
        run: &RunId,
        gate: &str,
        decision: GateDecision,
        note: &str,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        RunStore::new(self.graph.clone())
            .decide_gate(run, gate, decision, note, now)
            .lift()
    }

    /// Build `run`: its integration branch `kgraf/<run>` (created at HEAD the
    /// first time, reused on resume; never checked out), merge-back onto it, a
    /// worktree per worker, each route's provider behind its rate limiter, each
    /// route's environment probed (every MCP server must connect), and the
    /// orchestrator with its verifier, attempt environments and gate runner.
    /// Gate commands: configured; else, for an approved `plan`, exactly its
    /// declared commands; else discovered from the build files. With
    /// `contract_checks` (the default) the orchestrator also gets its contract
    /// checks (`Host::contract_checks`) and every environment the
    /// `challenge_contract` tool, bound to the orchestrator's handle here.
    pub async fn build(
        &self,
        startup: &Startup,
        run: RunId,
        plan: Option<&ApprovedPlan>,
        notices: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<PreparedRun> {
        // Decision 0087: dependencies first, before a branch or a worktree.
        self.ensure_dependencies(&run, notices)?;
        let base = self.build_base(startup, run, notices)?;
        self.finish_build(startup, base, plan, notices).await
    }

    /// The synchronous first part of [`Host::build`]: git and files, up to the
    /// route providers. [`Host::prepare_blocking`] runs it on a blocking thread
    /// so a git call that hangs never holds an async worker.
    fn build_base(
        &self,
        startup: &Startup,
        run: RunId,
        notices: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<BuildBase> {
        let root = self.config.root.as_path();
        let manager = &startup.manager;
        // Keep the graph in step with the working tree before planning: a run
        // built against a stale or never-built index would end every planner
        // slice lookup with nothing, even though the code it names exists.
        // Incremental (`Indexer::index`'s content-hash check), so an
        // already-current index costs one pass comparing unchanged file
        // hashes; see `build_base`'s callers for where this runs (every entry
        // point that starts a run).
        let index_report = {
            let store = self
                .graph
                .lock()
                .map_err(|_| Error::failed("graph lock poisoned"))?;
            Indexer::new(&store)
                .index()
                .step("cannot index the project's graph")?
        };
        if index_report.nodes == 0 {
            notices(Notice::GraphEmpty {
                root: root.to_path_buf(),
                files_seen: index_report.files_seen,
            });
        }
        // Continuous merge-back (0018) into the run's own integration branch. It
        // is never checked out (merge-back refuses a checked-out branch), so the
        // user's checkout is untouched; the user merges `kgraf/<run>`.
        let integration = ensure_integration_branch(root, run.as_str())?;
        // No commit identity, or one merge-back refuses (empty, or naming Claude),
        // and git too old: settings, which fail the same way every time.
        let identity = crate::worker::workspace::owner_identity(root).settings()?;
        let merge_back = Arc::new(
            MergeBack::open(manager.clone(), &integration, identity).map_err(|e| match e {
                e @ (kgraf_sandbox::MergeError::IdentityRefused(_)
                | kgraf_sandbox::MergeError::GitTooOld { .. }) => Err::<(), _>(e)
                    .with_settings(|| "cannot open merge-back".to_owned())
                    .unwrap_err(),
                other => Err::<(), _>(other)
                    .step("cannot open merge-back")
                    .unwrap_err(),
            })?,
        );
        let workspaces = Arc::new(GitWorkspaces::new(manager.clone(), merge_back.clone()));
        let read_denies = state_read_denies(
            &self.state,
            None,
            &self.state.join("ledger.db"),
            &self.state.join("runs"),
        )
        .lift()?;
        // The worker configuration (MCP servers), as `kgraf worker` reads it
        // by default. Absent is said, not assumed.
        let config_path = self.state.join("worker.json");
        let worker_config = if config_path.exists() {
            WorkerConfig::load(&config_path).settings()?
        } else {
            notices(Notice::NoWorkerConfig(config_path));
            WorkerConfig::default()
        };
        // Decision 0084: a worker may run the check its attempt is judged by
        // (the acceptance command) and the run's configured gate commands,
        // exactly as configured, besides the configuration's build and test
        // allowlist; `--shell-timeout` wins over the file's timeout.
        let worker_config = worker_config.with_configured_commands(
            std::iter::once(self.config.verify.as_str())
                .chain(self.config.gate_commands.iter().map(String::as_str)),
        );
        let worker_config = match self.config.shell_timeout {
            Some(timeout) => worker_config.with_shell_timeout(timeout),
            None => worker_config,
        };
        let mut parts = BTreeMap::new();
        for spec in &self.config.routes {
            let (route, model) = split_route(spec)?;
            // Route-qualified: two routes to one vendor are two prefixes.
            let (vendor, vendor_name) = if self.in_process.contains_key(&route) {
                (format!("in-process ({route})"), "in-process".to_owned())
            } else {
                let entry = kgraf_fleet::entry(&route).settings()?;
                (
                    format!("{} ({})", entry.vendor, entry.id),
                    entry.vendor.to_string(),
                )
            };
            parts.insert(
                format!("{route}:{model}"),
                RouteParts {
                    provider: self.provider(&route, &model)?,
                    vendor,
                    vendor_name,
                    model,
                },
            );
        }
        // With contract checks, every environment of the run (the route probes
        // and every attempt) carries `challenge_contract`, bound to the
        // orchestrator once it is built (`finish_build`).
        let line = self.config.contract_checks.then(ContractLine::new);
        let extra_tools: Vec<Arc<dyn kgraf_tools::Tool>> = line
            .iter()
            .map(|l| Arc::new(ChallengeTool::new(l.clone())) as Arc<dyn kgraf_tools::Tool>)
            .collect();
        // Every agent is an addressable session (decision 0067), behind the
        // `sessions` flag (default off): the line is bound to the
        // orchestrator's handle once it exists, exactly as `line` above is.
        let session_line = self.session_line();
        let dependency_refetch = self.dependency_refetch(&run);
        let assembler = Arc::new(AttemptAssembler {
            root: root.to_path_buf(),
            state: self.state.clone(),
            graph_path: self.graph_path.clone(),
            // This build's own index pass, just above, decides this: not a
            // stale reading from when the host opened.
            indexed: index_report.nodes > 0,
            ledger: self.ledger.clone(),
            config: worker_config,
            max_output_tokens: self.config.max_tokens,
            read_denies: read_denies.clone(),
            workspaces: workspaces.clone(),
            routes: parts,
            extra_tools,
            session_line: session_line.clone(),
            // One registry per run: every environment of this
            // run admits its prefix here, so another run in this process with
            // another tool list on the same route is not refused.
            prefixes: Arc::new(PrefixRegistry::new()),
            machine_gate: self.machine_gate.clone(),
            tbench_bridge_socket: self.config.tbench_bridge_socket.clone(),
            dependency_refetch,
        });
        Ok(BuildBase {
            run,
            integration,
            merge_back,
            workspaces,
            read_denies,
            assembler,
            line,
            session_line,
        })
    }

    /// The rest of [`Host::build`]: the route probes (async: every MCP server
    /// connects), the services, the orchestrator and its gate runner.
    async fn finish_build(
        &self,
        startup: &Startup,
        base: BuildBase,
        plan: Option<&ApprovedPlan>,
        notices: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<PreparedRun> {
        let root = self.config.root.as_path();
        let manager = &startup.manager;
        let BuildBase {
            run,
            integration,
            merge_back,
            workspaces,
            read_denies,
            assembler,
            line,
            session_line,
        } = base;
        let routes = route_envs(
            &assembler,
            manager,
            &self.config.routes,
            &self.context,
            run.as_str(),
            &self.in_process.keys().cloned().collect(),
            notices,
        )
        .await?;
        // A prefix that differs from the one the last run in this process used
        // for the same vendor and model is a cache miss: said, not hidden. The
        // process-wide registry records the last prefix in force per group
        // (`replace`); runs admit against their own registries only.
        for r in &routes {
            if let Some(change) = PrefixRegistry::global().replace(&r.env.prefix) {
                notices(Notice::PrefixChanged {
                    route: r.name.clone(),
                    change,
                });
            }
        }
        let route_shapes = routes
            .iter()
            .map(|r| RouteShape {
                name: r.name.clone(),
                rate_limit: self.rate_limit(r.name.split(':').next().unwrap_or_default()),
                context: r.context.clone(),
                max_output_tokens: r.env.settings.max_output_tokens,
                tools: r.env.tools.names().map(str::to_owned).collect(),
                approval: r.env.approval.digest(),
            })
            .collect();
        // Each party's handshake is stated by its own route's model (0030):
        // one caller per route, through the host's ledger.
        let handshake_routes: BTreeMap<String, (Arc<dyn Provider>, String)> = routes
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    (r.env.provider.clone(), r.model().to_owned()),
                )
            })
            .collect();
        let services = Services {
            graph: self.graph.clone(),
            ledger: self.ledger.clone(),
            prices: self.prices()?,
            routes,
            decomposer: Arc::new(ModelDecomposer(self.caller(&run)?)),
            grounder: Arc::new(ModelGrounder(self.caller(&run)?)),
            verifier: Arc::new(CommandVerifier {
                command: self.config.verify.clone(),
                repo: root.to_path_buf(),
                workspaces: workspaces.clone(),
                merge_back: merge_back.clone(),
                read_denies: read_denies.clone(),
                timeout: self.config.gate_timeout,
            }),
            workspaces: workspaces.clone(),
            clock: Arc::new(SystemClock),
            admission: self.admission.clone(),
            runs_dir: self.state.join("runs"),
            profiles: self.profiles.clone(),
            profiles_dir: self.state.clone(),
        };
        let envs: Arc<dyn AttemptEnvs> = match &self.asks {
            Some(asks) => Arc::new(ResolverOverride {
                inner: assembler,
                asks: asks.clone(),
            }),
            None => assembler,
        };
        // Decision 0067: a worker's question is
        // tried against the run record, and a connection no rule covers is
        // judged, each in one call on the planner's route through the host's
        // ledger, attributed to `orchestrator:<run>`. The orchestrator reserves
        // each call before it is made and settles it at its ledger row.
        // Wired for every run: without session tools no worker can
        // ask, so nothing is called.
        let orchestrator = Orchestrator::new(self.orchestrator_config.clone(), services)?
            .with_attempt_envs(envs)
            .with_question_answerer(Arc::new(ModelAnswerer(self.caller(&run)?)));
        let gate_commands = match plan {
            Some(plan) if self.config.gate_commands.is_empty() => plan.validation_commands(),
            _ => discover_validation_commands(root, &self.config.gate_commands)?,
        };
        let orchestrator = orchestrator.with_gate_runner(Arc::new(CommandGateRunner {
            commands: gate_commands.clone(),
            manager: manager.clone(),
            merge_back: merge_back.clone(),
            read_denies: read_denies.clone(),
            timeout: self.config.gate_timeout,
            output_cap: GATE_OUTPUT_CAP,
        }));
        let (orchestrator, check_scratch, stubs) = match &line {
            Some(line) => {
                let stubs = Arc::new(GitStubLander::new(manager.clone(), merge_back.clone()));
                let (checks, scratch) = self.contract_checks(
                    &run,
                    &integration,
                    &read_denies,
                    &handshake_routes,
                    &stubs,
                )?;
                let orchestrator = orchestrator.with_contract_checks(checks)?;
                line.bind(orchestrator.handle()).map_err(Error::failed)?;
                (orchestrator, Some(scratch), Some(stubs))
            }
            None => (orchestrator, None, None),
        };
        // 0079: the cross-vendor interface review asks a consumer's own route
        // model, through the host's ledger.
        let orchestrator = if self.config.interface_review {
            orchestrator.with_interface_reviewer(Arc::new(RoutedInterfaceReviewer {
                ledger: self.ledger.clone(),
                routes: handshake_routes
                    .iter()
                    .map(|(name, (provider, model))| {
                        (
                            name.clone(),
                            (
                                provider.clone(),
                                model.clone(),
                                self.config.planner_max_tokens,
                            ),
                        )
                    })
                    .collect(),
                run: run.to_string(),
            }))
        } else {
            orchestrator
        };
        // Bound next to the contract line, above: decision 0067's session
        // tools answer to this run's orchestrator too, whether or not
        // contract checks are on.
        if let Some(session_line) = &session_line {
            session_line
                .bind(orchestrator.handle())
                .map_err(Error::failed)?;
        }
        let shape = RunShape {
            planner: self.planner.clone(),
            planner_rate_limit: self.rate_limit(&self.planner.route),
            routes: route_shapes,
            limits: self.admission.gate().limits(),
            step_budget: self.orchestrator_config.step_budget,
            verify: self.config.verify.clone(),
            gate_commands: gate_commands.clone(),
            gate_timeout: self.config.gate_timeout,
            integration: integration.clone(),
            contract_checks: self.config.contract_checks,
            check_timeout: self.config.check_timeout,
            check_compile_timeout: self.config.check_compile_timeout,
        };
        Ok(PreparedRun {
            trace: self
                .state
                .join("runs")
                .join(run.as_str())
                .join("trace.jsonl"),
            end: RunEnd {
                run: run.clone(),
                workspaces,
                store: RunStore::new(self.graph.clone()),
                ledger: self.ledger.clone(),
                check_scratch,
                stubs,
            },
            run,
            orchestrator,
            integration,
            gate_commands,
            plan_id: plan.map(|p| p.plan_id().to_owned()),
            shape,
        })
    }

    fn rate_limit(&self, route: &str) -> Option<RateLimit> {
        self.limiters.get(route).map(|l| l.limit())
    }

    /// A fresh, unbound [`SessionLine`] when this host's config carries the
    /// `sessions` flag (decision 0067; default off), `None` otherwise. Its own
    /// method (rather than the one-line closure inline in `build_base`) so a
    /// test can call it directly on a `Host` built from `RunConfig::new`'s
    /// defaults without going through a whole build.
    pub(crate) fn session_line(&self) -> Option<SessionLine> {
        self.config.sessions.then(SessionLine::new)
    }

    /// [`Host::startup`] then [`Host::build`].
    pub async fn prepare(
        &self,
        run: RunId,
        plan: Option<&ApprovedPlan>,
        notices: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<PreparedRun> {
        let startup = self.startup(notices)?;
        self.build(&startup, run, plan, notices).await
    }

    /// [`Host::prepare`] from synchronous code running on a multi-thread tokio
    /// runtime (a factory's `build`), blocking this worker thread only. Refused,
    /// not panicking, anywhere else. Notices are kept for [`Host::take_notices`].
    ///
    /// Bounded by the build timeout, git included: the build runs as its own
    /// task, its synchronous part (startup cleanup, the integration branch,
    /// merge-back, the providers) on a blocking thread. Past the bound the task is
    /// aborted: the probe worktree is removed by its guard and each stdio MCP
    /// server's process group is killed as its connection drops. A git call
    /// still running on the blocking thread finishes there and is discarded.
    pub fn prepare_blocking(self: &Arc<Self>, run: RunId) -> Result<PreparedRun> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::Invalid("a blocking prepare needs a tokio runtime to run on".into())
        })?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return Err(Error::Invalid(
                "a blocking prepare needs a multi-thread tokio runtime (it blocks one worker)"
                    .into(),
            ));
        }
        let kept: Arc<Mutex<Vec<Notice>>> = Arc::new(Mutex::new(Vec::new()));
        // Decision 0087: the dependency gate, outside the build's time bound
        // (a first fetch can take minutes; the fetch has its own bound).
        let gate = tokio::task::block_in_place(|| {
            let sink = kept.clone();
            self.ensure_dependencies(&run, &mut move |n| {
                sink.lock().unwrap_or_else(|p| p.into_inner()).push(n)
            })
        });
        if let Err(e) = gate {
            let kept = std::mem::take(&mut *kept.lock().unwrap_or_else(|p| p.into_inner()));
            self.notices
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend(kept);
            return Err(e);
        }
        let host = self.clone();
        let sink = kept.clone();
        let task = handle.spawn(async move {
            let push = move |n: Notice| sink.lock().unwrap_or_else(|p| p.into_inner()).push(n);
            let blocking = host.clone();
            let mut early = push.clone();
            let (startup, base) = tokio::task::spawn_blocking(move || {
                let startup = blocking.startup(&mut early)?;
                let base = blocking.build_base(&startup, run, &mut early)?;
                Ok::<_, Error>((startup, base))
            })
            .await
            .map_err(|e| Error::failed(format!("the run's build task ended abnormally: {e}")))??;
            let mut late = push;
            host.finish_build(&startup, base, None, &mut late).await
        });
        let abort = task.abort_handle();
        let timeout = self.build_timeout;
        let prepared = tokio::task::block_in_place(|| {
            handle.block_on(async {
                match tokio::time::timeout(timeout, task).await {
                    Ok(Ok(prepared)) => prepared,
                    Ok(Err(e)) => Err(Error::failed(format!(
                        "the run's build task ended abnormally: {e}"
                    ))),
                    Err(_) => {
                        abort.abort();
                        Err(Error::failed(format!(
                            "the run's build did not finish within {}s (an MCP server or git \
                             not answering)",
                            timeout.as_secs()
                        )))
                    }
                }
            })
        });
        let kept = std::mem::take(&mut *kept.lock().unwrap_or_else(|p| p.into_inner()));
        self.notices
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(kept);
        prepared
    }

    /// Seed the admission's trailing hour with the ledger's spend of the last
    /// hour, so a new host (a reopened `run.json`, a restarted app) keeps the
    /// hourly cap's history instead of starting from zero. Reads only the last
    /// hour's calls (`Ledger::calls_finished_since`). Each call once
    /// (`BudgetBook::seed_hour`), so a resumed run charging the same call does
    /// not count it in the hour twice.
    ///
    /// A settled call counts at its price. An unreconciled call counts at the
    /// worst case, as a resume counts it; so does a call on a model the price
    /// table does not price (at the dearest priced model's worst case). Each
    /// estimate is a [`Notice::EstimatedSpend`] ([`Host::take_notices`]): never
    /// a failure that would keep the host closed for up to an hour, never a
    /// silent zero. Returns the amount seeded.
    ///
    /// The desktop shell calls this in `ProjectFactory::open`, for each host it
    /// opens; the command does not, so its behavior is unchanged.
    pub fn seed_trailing_hour(&self) -> Result<kgraf_core::Money> {
        const HOUR_MS: i64 = 60 * 60 * 1_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let prices = self.prices()?;
        let calls = self.ledger.calls_finished_since(now - HOUR_MS + 1).lift()?;
        let mut estimated = Vec::new();
        let mut total = kgraf_core::Money::ZERO;
        {
            let mut budget = self.admission.budget();
            for c in calls {
                let at = c.finished_ms.unwrap_or(c.started_ms);
                let priced = prices.get(&c.model).is_some();
                let cost = match (c.settlement, priced) {
                    (kgraf_provider::Settlement::Failed, _) => continue,
                    (
                        kgraf_provider::Settlement::Counted | kgraf_provider::Settlement::Cancelled,
                        true,
                    ) => prices.cost(&c.model, &c.usage).lift()?,
                    (settlement, _) => {
                        let amount = self.worst_case_call(&prices, &c.model)?;
                        estimated.push(Notice::EstimatedSpend {
                            call: c.id.to_string(),
                            model: c.model.clone(),
                            amount,
                            reason: if priced {
                                format!("{settlement:?}: its spend is not known")
                            } else {
                                "its model is not in the price table".to_owned()
                            },
                        });
                        amount
                    }
                };
                budget.seed_hour(c.id.as_str(), at, cost);
                total += cost;
            }
        }
        self.notices
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(estimated);
        Ok(total)
    }

    /// The most one call on `model` can cost, as a resume holds an
    /// unreconciled call: the effective window as input and the larger output
    /// ceiling (a worker's, or the planner's `planner_max_tokens`). A model the table does not
    /// price costs what the dearest priced model would.
    pub(crate) fn worst_case_call(
        &self,
        prices: &PriceTable,
        model: &str,
    ) -> Result<kgraf_core::Money> {
        let input = self.context.effective_window();
        let output = u64::from(self.config.max_tokens.max(self.config.planner_max_tokens));
        let cost =
            |m: &str| prices.cost(m, &kgraf_orchestrator::dear_usage(prices, m, input, output));
        if prices.get(model).is_some() {
            return cost(model).lift();
        }
        let mut dearest = kgraf_core::Money::ZERO;
        for m in prices.models() {
            let c = cost(m).lift()?;
            if c > dearest {
                dearest = c;
            }
        }
        if dearest == kgraf_core::Money::ZERO {
            return Err(Error::Invalid(format!(
                "model `{model}` is not priced and the price table prices nothing to bound it with"
            )));
        }
        Ok(dearest)
    }

    /// Notices of builds made through [`Host::prepare_blocking`], oldest first.
    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut *self.notices.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Prepare and start a run in the background: [`Host::prepare`] (with the
    /// plan's gates for [`RunStart::Plan`]), `port` attached, the run spawned on
    /// the current runtime.
    pub async fn start_run(
        &self,
        run: RunId,
        start: RunStart,
        port: Option<Arc<dyn MessagePort>>,
        notices: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<RunHandle> {
        let prepared = self.prepare(run, start.plan(), notices).await?;
        if let Some(port) = port {
            prepared.attach_port(port);
        }
        let summary = prepared.summary();
        let handle = prepared.handle();
        let task = tokio::spawn(prepared.run(start));
        Ok(RunHandle {
            summary,
            handle,
            task,
        })
    }

    /// The housekeeping of a run handed out by [`PreparedRun::into_orchestrator`],
    /// after it ended: the attempt-ref sweep and the unreconciled count. `None`
    /// for a run this host did not hand out (or already finished).
    pub fn finish_run(&self, run: &RunId) -> Option<Housekeeping> {
        self.fetch_grants
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run);
        if let Some(refetch) = self
            .refetchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run)
        {
            refetch.stop();
        }
        let end = self
            .ended
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run)?;
        Some(end.finish())
    }
}

/// Reads one environment variable (`kgraf_core::legacy::env_var` outside tests,
/// which also accepts a credential under its former `HAUSTORIA_*` name).
pub(crate) type EnvLookup = dyn Fn(&str) -> Option<String>;

/// The readiness of a route set: each route in the catalog (always; an
/// in-process route has none), its credential when `lookup` is given, its model
/// priced when `prices` is given.
pub(crate) fn check_routes(
    routes: &[RouteSpec],
    prices: Option<&PriceTable>,
    in_process: &BTreeMap<String, Arc<dyn Provider>>,
    lookup: Option<&EnvLookup>,
) -> Result<(), NotReady> {
    if routes.is_empty() {
        return Err(NotReady::NoRoutes);
    }
    let mut unpriced: Vec<String> = Vec::new();
    for r in routes {
        if !in_process.contains_key(&r.route) {
            let entry = kgraf_fleet::entry(&r.route).map_err(|e| NotReady::UnknownRoute {
                route: r.route.clone(),
                detail: e.to_string(),
            })?;
            if let Some(lookup) = lookup {
                kgraf_fleet::credential_from(entry, lookup).map_err(|e| {
                    NotReady::MissingCredential {
                        route: r.route.clone(),
                        detail: e.to_string(),
                    }
                })?;
            }
        }
        if let Some(prices) = prices
            && prices.get(&r.model).is_none()
            && !unpriced.contains(&r.model)
        {
            unpriced.push(r.model.clone());
        }
    }
    if unpriced.is_empty() {
        Ok(())
    } else {
        Err(NotReady::MissingPrices { models: unpriced })
    }
}

/// What [`Host::build_base`] made, for [`Host::finish_build`].
struct BuildBase {
    run: RunId,
    integration: String,
    merge_back: Arc<MergeBack>,
    workspaces: Arc<GitWorkspaces>,
    read_denies: Vec<PathBuf>,
    assembler: Arc<AttemptAssembler>,
    /// Bound to the orchestrator when the run checks contracts.
    line: Option<ContractLine>,
    /// Bound to the orchestrator when the run registers session tools
    /// (decision 0067).
    session_line: Option<SessionLine>,
}

/// Where the check scratch root may be, in order: under the system temporary
/// directory (per user on macOS, shared `/tmp` on Linux), then per-user
/// places: `$XDG_RUNTIME_DIR` and `~/.cache/kgraf`.
fn scratch_root_candidates() -> Vec<PathBuf> {
    let mut out = vec![std::env::temp_dir().join("kgraf-checks")];
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(run).join("kgraf-checks"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(home).join(".cache/kgraf/checks"));
    }
    out
}

/// Why `dir` may not hold check scratch, or `None` when it may: it must be a
/// real directory (not a symbolic link), owned by this user, and not writable
/// by group or others. A shared `/tmp/kgraf-checks` another user made
/// first, or left open, would let that user swap a run's directory.
#[cfg(unix)]
fn unsafe_scratch_root(dir: &Path) -> Option<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) => return Some(format!("{}: {e}", dir.display())),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Some(format!("{} is not a directory of its own", dir.display()));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        return Some(format!(
            "{} is owned by user {}, not by this user ({me})",
            dir.display(),
            meta.uid()
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Some(format!(
            "{} is writable by others (mode {mode:o})",
            dir.display()
        ));
    }
    foreign_cargo_file_above(dir)
}

/// A cargo file (`Cargo.toml`, `.cargo/config.toml`, `.cargo/config`) in
/// `dir` or above it that another user could have placed or could change:
/// not a regular file of its own, or it or its `.cargo` directory failing
/// the sandbox's own trust test ([`kgraf_sandbox::trust::untrusted_reason`]:
/// owned by this user or root, never writable by others, group-writable only
/// by this user's private group), so the two never disagree. Cargo run in a
/// check tree reads such a file on its way up: on Linux the sandbox refuses
/// it and every check fails; on macOS it is read, and a planted `runner`
/// would run as the owner. Such a root is skipped for
/// the next candidate.
#[cfg(unix)]
fn foreign_cargo_file_above(dir: &Path) -> Option<String> {
    use kgraf_sandbox::trust::untrusted_reason;
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    for ancestor in dir.ancestors() {
        for rel in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
            let file = ancestor.join(rel);
            let Ok(meta) = std::fs::symlink_metadata(&file) else {
                continue;
            };
            let problem = if !meta.file_type().is_file() {
                Some(format!("{} is not a regular file", file.display()))
            } else if let Some(why) = untrusted_reason(&meta) {
                Some(format!("{} {why}", file.display()))
            } else if rel.starts_with(".cargo") {
                let dot_cargo = ancestor.join(".cargo");
                match std::fs::symlink_metadata(&dot_cargo) {
                    Ok(m) if m.is_dir() => {
                        untrusted_reason(&m).map(|why| format!("{} {why}", dot_cargo.display()))
                    }
                    _ => Some(format!("{} is not a directory", dot_cargo.display())),
                }
            } else {
                None
            };
            if let Some(problem) = problem {
                return Some(format!(
                    "{} lies below {}, which another user could have placed or can change \
                     ({problem}), and cargo in a check would read it",
                    dir.display(),
                    file.display()
                ));
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn unsafe_scratch_root(dir: &Path) -> Option<String> {
    (!dir.is_dir()).then(|| format!("{} is not a directory", dir.display()))
}

/// The first candidate that is, or can be made, a safe scratch root (created
/// with mode 0700 when absent). Fails loud, naming each candidate's problem,
/// when none is: a check never runs in a directory another user can
/// change.
pub(crate) fn safe_scratch_root(candidates: &[PathBuf]) -> Result<PathBuf> {
    let mut problems = Vec::new();
    for dir in candidates {
        if std::fs::symlink_metadata(dir).is_err() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            if let Err(e) = builder.create(dir) {
                problems.push(format!("{}: {e}", dir.display()));
                continue;
            }
        }
        match unsafe_scratch_root(dir) {
            None => return Ok(dir.clone()),
            Some(problem) => problems.push(problem),
        }
    }
    Err(Error::Invalid(format!(
        "no safe place for contract-check scratch: {}",
        problems.join("; ")
    )))
}

/// The check scratch root of this process ([`safe_scratch_root`]).
pub(crate) fn check_scratch_root() -> Result<PathBuf> {
    safe_scratch_root(&scratch_root_candidates())
}

/// Where a run's contract checks make their trees: outside the repository
/// (the runner refuses a scratch inside its base) and outside `.kgraf`
/// (every check is denied reads there). Made with `mkdtemp` (`tempfile`,
/// MIT/Apache-2.0, already in the lock): a fresh name no other user can
/// predict, mode 0700. Named `<run>-<pid>-<random>` so a
/// later process can tell a dead owner's directory ([`sweep_check_scratch`]).
/// Removed when the run's [`RunEnd`] drops.
fn check_scratch(run: &RunId) -> Result<tempfile::TempDir> {
    let root = check_scratch_root()?;
    let prefix = format!("{}-{}-", run.as_str(), std::process::id());
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix);
    // Created with the mode, not chmod after: no moment where another user can
    // enter it. `tempfile` otherwise takes the umask (0755 in the test).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir_in(&root).with_step(|| {
        format!(
            "cannot create a check scratch directory in {}",
            root.display()
        )
    })
}

/// Remove the check scratch directories whose owner process is gone (a
/// crash, a kill): their name carries the owner's PID. A live PID, even one
/// reused by another program, keeps its directory: removing a live run's
/// trees would be worse than leaving a dead one's. Unix only; elsewhere a dead
/// run's directory stays in the temporary directory.
pub(crate) fn sweep_check_scratch() -> Vec<PathBuf> {
    // Only the unix branch below pushes to it.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut removed = Vec::new();
    #[cfg(unix)]
    {
        // Only a safe root is swept: an unsafe one is never read from.
        let Ok(root) = check_scratch_root() else {
            return removed;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return removed;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut parts = name.rsplitn(3, '-');
            let (_random, pid) = (parts.next(), parts.next());
            let Some(pid) = pid.and_then(|p| p.parse::<libc::pid_t>().ok()) else {
                continue;
            };
            if pid <= 0 || pid == std::process::id() as libc::pid_t {
                continue;
            }
            // SAFETY: signal 0 only probes whether the process exists.
            let gone = unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone && std::fs::remove_dir_all(entry.path()).is_ok() {
                removed.push(entry.path());
            }
        }
    }
    removed
}

/// What a run was built with, for a caller to show and for the test that holds
/// the command and the library to one construction.
#[derive(Clone, Debug, PartialEq)]
pub struct RunShape {
    pub planner: RouteSpec,
    pub planner_rate_limit: Option<RateLimit>,
    pub routes: Vec<RouteShape>,
    pub limits: Limits,
    pub step_budget: u32,
    pub verify: String,
    pub gate_commands: Vec<String>,
    pub gate_timeout: Duration,
    pub integration: String,
    /// Whether interface contracts are written, checked before dispatch and
    /// open to challenge; with the bounds of one check run.
    pub contract_checks: bool,
    pub check_timeout: Duration,
    pub check_compile_timeout: Duration,
}

/// One route as the orchestrator sees it: its probed environment.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteShape {
    pub name: String,
    pub rate_limit: Option<RateLimit>,
    pub context: ContextConfig,
    pub max_output_tokens: u32,
    pub tools: Vec<String>,
    /// `ApprovalChain::digest`: the hooks and policies that review each call.
    pub approval: String,
}

/// The lines the command prints before a run starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub run: RunId,
    pub integration: String,
    pub gate_commands: Vec<String>,
    pub trace: PathBuf,
}

/// A run built and not yet started.
pub struct PreparedRun {
    run: RunId,
    orchestrator: Orchestrator,
    integration: String,
    gate_commands: Vec<String>,
    trace: PathBuf,
    /// The approved plan whose gates the run was built with.
    plan_id: Option<String>,
    shape: RunShape,
    end: RunEnd,
}

impl PreparedRun {
    pub fn run_id(&self) -> &RunId {
        &self.run
    }

    pub fn summary(&self) -> RunSummary {
        RunSummary {
            run: self.run.clone(),
            integration: self.integration.clone(),
            gate_commands: self.gate_commands.clone(),
            trace: self.trace.clone(),
        }
    }

    pub fn shape(&self) -> &RunShape {
        &self.shape
    }

    /// Serve a message port (notes and status questions). Spawns its reader on
    /// the current tokio runtime.
    pub fn attach_port(&self, port: Arc<dyn MessagePort>) {
        self.orchestrator.attach_port(port);
    }

    /// Stop, steer and status for the run once it runs.
    pub fn handle(&self) -> OrchestratorHandle {
        self.orchestrator.handle()
    }

    /// Run to the end, then sweep the attempt refs of the worktrees this run
    /// worked in that no open contract still uses. A start that does not match
    /// what the run was built for is refused: a plan's gates would otherwise not
    /// be the gates the run passes.
    ///
    /// Records a `RunStarted`/`RunFinished` pair to the local diagnostics file
    /// (decision 0069), a no-op unless a binary called
    /// `kgraf_diag::init`. This is the one place both `kgraf orchestrate` and
    /// the desktop shell start a run from, so it is where "runs started and
    /// finished" and "the providers and models used" (`self.shape.routes`,
    /// already public route:model pairs, never a credential) are counted once
    /// per run rather than at every internal dispatch.
    pub async fn run(self, start: RunStart) -> Result<RunOutcome> {
        let run_id = self.run.to_string();
        kgraf_diag::record(kgraf_diag::DiagEvent::RunStarted {
            run: run_id.clone(),
        });
        let outcome = self.run_inner(start).await;
        kgraf_diag::record(kgraf_diag::DiagEvent::RunFinished {
            run: run_id,
            ok: outcome.is_ok(),
        });
        outcome
    }

    async fn run_inner(self, start: RunStart) -> Result<RunOutcome> {
        let report = match (start, self.plan_id.as_deref()) {
            (RunStart::Plan(plan), Some(built)) if built == plan.plan_id() => {
                self.orchestrator
                    .start_from_plan(self.run.clone(), *plan)
                    .await?
            }
            (RunStart::Plan(plan), built) => {
                return Err(Error::Invalid(format!(
                    "run {} was built for {}, not for plan {}",
                    self.run,
                    built.map_or("no plan".to_owned(), |p| format!("plan {p}")),
                    plan.plan_id()
                )));
            }
            (_, Some(built)) => {
                return Err(Error::Invalid(format!(
                    "run {} was built with the gates of plan {built}; start it with that plan",
                    self.run
                )));
            }
            (RunStart::Resume, None) => self.orchestrator.resume(self.run.clone()).await?,
            (RunStart::Instruction(text), None) => {
                if text.trim().is_empty() {
                    return Err(Error::Invalid("the instruction is empty".into()));
                }
                self.orchestrator.start(self.run.clone(), text).await?
            }
        };
        let housekeeping = self.end.finish();
        Ok(RunOutcome {
            report,
            housekeeping,
        })
    }

    /// The orchestrator alone, for a caller that starts it itself (an
    /// `OrchestratorFactory`). The ref sweep is then `host.finish_run(run)`, once
    /// the run ended.
    pub fn into_orchestrator(self, host: &Host) -> Orchestrator {
        host.ended
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(self.run.clone(), self.end);
        self.orchestrator
    }
}

/// A run's report and what was done after it.
#[derive(Debug)]
pub struct RunOutcome {
    pub report: RunReport,
    pub housekeeping: Housekeeping,
}

/// After a run: the attempt refs swept, and the calls whose spend is not yet
/// accounted for. A failure here never hides the report.
#[derive(Debug)]
pub struct Housekeeping {
    pub swept: Result<Vec<String>>,
    /// Calls with unaccounted spend (`kgraf unreconciled` lists them).
    pub unreconciled: Result<usize>,
}

/// What a run needs after its end.
struct RunEnd {
    run: RunId,
    workspaces: Arc<GitWorkspaces>,
    store: RunStore,
    ledger: Arc<Ledger>,
    /// The contract checks' scratch directory. Each check removes its own
    /// tree; dropping this removes what is left, on every path out (a run
    /// finished, a prepared run dropped unstarted, a factory run never
    /// finished), since `TempDir` removes itself on drop.
    #[allow(dead_code)]
    check_scratch: Option<tempfile::TempDir>,
    /// The run's stub lander (0081), when contract checks are on: at the
    /// end of the run it removes the worktrees and attempt refs a refused
    /// or failed land kept for inspection.
    stubs: Option<Arc<GitStubLander>>,
}

impl RunEnd {
    fn finish(self) -> Housekeeping {
        let swept = self
            .store
            .load(&self.run)
            .lift()
            .and_then(|record| self.workspaces.sweep_refs(record.as_ref()))
            .and_then(|mut swept| {
                if let Some(stubs) = &self.stubs {
                    swept.extend(stubs.sweep().map_err(Error::failed)?);
                }
                Ok(swept)
            });
        Housekeeping {
            swept,
            unreconciled: self.ledger.unreconciled().map(|v| v.len()).lift(),
        }
    }
}

/// A run started by [`Host::start_run`].
pub struct RunHandle {
    summary: RunSummary,
    handle: OrchestratorHandle,
    task: tokio::task::JoinHandle<Result<RunOutcome>>,
}

impl RunHandle {
    pub fn summary(&self) -> &RunSummary {
        &self.summary
    }

    /// Stop, steer and status while it runs.
    pub fn orchestrator(&self) -> &OrchestratorHandle {
        &self.handle
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Wait for the end.
    pub async fn wait(self) -> Result<RunOutcome> {
        self.task
            .await
            .map_err(|e| Error::failed(format!("the run's task ended abnormally: {e}")))?
    }
}

/// `kgraf_orchestrator::OrchestratorFactory` over a [`Host`]: `configured` is
/// [`Host::check_configured`], `ready(schedule_id)` is [`Host::check_ready`] of the
/// host's route set;
/// `build` is [`Host::prepare_blocking`] then [`PreparedRun::into_orchestrator`].
/// Call [`Host::finish_run`] when a run it built ends.
pub struct HostFactory {
    host: Arc<Host>,
}

impl HostFactory {
    /// A factory over `host` as it is: its admission's trailing hour holds only
    /// what this process spent. For a host of the desktop shell use
    /// [`HostFactory::seeded`].
    pub fn new(host: Arc<Host>) -> Self {
        Self { host }
    }

    /// A factory over `host` with its admission's trailing hour seeded from the
    /// ledger ([`Host::seed_trailing_hour`]), so the hourly cap counts what was
    /// spent in the last hour before this host existed. Every factory the
    /// desktop shell makes goes through here: the scheduler's
    /// (`ProjectFactory::open`) and, when it lands, the orchestrator box's.
    pub fn seeded(host: Arc<Host>) -> Result<(Self, kgraf_core::Money)> {
        let seeded = host.seed_trailing_hour()?;
        Ok((Self { host }, seeded))
    }

    pub fn host(&self) -> &Arc<Host> {
        &self.host
    }
}

#[async_trait::async_trait]
impl OrchestratorFactory for HostFactory {
    /// Whole-process: [`Host::check_configured`] (settings, catalog, prices).
    /// A failure here is reported without claiming; no schedule is disabled.
    async fn configured(&self) -> std::result::Result<(), ReadinessFailure> {
        self.host.check_configured()?;
        Ok(())
    }

    /// Per schedule: the credentials of the host's routes (every run of a host
    /// uses its routes). A missing one disables the schedule with that reason.
    async fn ready(&self, _schedule_id: &str) -> std::result::Result<(), ReadinessFailure> {
        self.host.check_credentials(&self.host.route_set())?;
        Ok(())
    }

    /// A settings failure (`Error::Invalid`, `NotReady`) is not transient; a
    /// failed step (git, IO, an MCP server, the build bound) is: the same
    /// configuration can build later.
    fn build(&self, run: &RunId) -> std::result::Result<Orchestrator, OrchestratorError> {
        self.host
            .prepare_blocking(run.clone())
            .map(|prepared| prepared.into_orchestrator(&self.host))
            .map_err(factory_error)
    }
}

/// A host error as the factory seam returns it: the orchestrator's own error
/// as it is, a missing credential as `MissingCredential` (the scheduler disables
/// the schedule with that reason), everything else as `Build` with its class.
pub(crate) fn factory_error(e: Error) -> OrchestratorError {
    match e {
        Error::Orchestrator(e) => e,
        Error::Credential(_) => OrchestratorError::MissingCredential(format!("{e:#}")),
        e => OrchestratorError::Build {
            transient: e.is_transient(),
            detail: format!("{e:#}"),
        },
    }
}
