//! Builds and starts orchestrator runs. `kgraf orchestrate` is a thin layer
//! over this crate, and the desktop shell hosts runs through it: the tray's
//! scheduled runs and the orchestrator box.
//!
//! The construction moved here from `kgraf-cli`'s `orchestrate_cmd.rs` with no
//! change in behavior: the same steps in the same order, the same messages. See
//! [`host`] for the steps. The `worker` module moved with it, because each attempt's
//! environment is assembled through it.
//!
//! # API
//!
//! - [`RunConfig`]: the command's per-project arguments (routes, planner, prices,
//!   caps, limits, context, verify and gate commands, rate limits). The command
//!   fills it from its flags; [`RunConfig::load`] reads it from a JSON file. The
//!   price table, `.kgraf/worker.json` and the credential environment variables
//!   are read from the same places for both.
//! - [`RunConfig::check_configured`] and [`Host::check_ready`]: typed readiness
//!   ([`NotReady`]: no routes, invalid setting, unknown route, missing credential,
//!   unreadable prices, missing prices). They build nothing and call nothing.
//! - [`Host::open`]: the process-wide part, with the one `Admission` all runs share.
//! - [`Host::start_run`] with a [`RunStart`] (an instruction, a resume, or an approved
//!   plan) returns a [`RunHandle`]. [`Host::prepare`] returns a [`PreparedRun`] for a
//!   caller that starts the run itself.
//! - [`HostFactory`]: `kgraf_orchestrator::OrchestratorFactory` over a host.
//!
//! # Adapters
//!
//! ## Scheduled runs: implemented
//!
//! `kgraf_orchestrator::OrchestratorFactory` (`configured()`,
//! `ready(schedule_id)`, `build(&RunId)`) is implemented by [`HostFactory`]:
//!
//! - `configured()`: [`Host::check_configured`]: the settings, every route in the
//!   catalog, every model in the price table. The price table is read again when
//!   its file changes. A failure is `TickEvent::NotRunnable`: nothing is claimed,
//!   no schedule is disabled.
//! - `ready(schedule_id)`: [`Host::check_credentials`] of `host.route_set()`: the
//!   one fact that can change between runs of a configured host. A missing
//!   credential disables that schedule with the reason. Every run of a host uses
//!   the host's routes; a schedule with other routes needs a host of its own.
//! - `build(run)`: [`Host::prepare_blocking`] then
//!   [`PreparedRun::into_orchestrator`], bounded by [`host::DEFAULT_BUILD_TIMEOUT`]
//!   ([`Host::with_build_timeout`]) so a hung MCP server cannot hold the tick.
//!   `ScheduledRuns::start_run` calls it on the scheduler's multi-thread runtime,
//!   where blocking one worker is allowed; anywhere else it is refused, not a
//!   panic. The build runs as its own task (its git part on a blocking thread),
//!   so the bound holds for git too; past it the task is aborted, the route probe
//!   worktree is removed by a guard, and a stdio MCP server's process group dies
//!   with its connection. A failure is `OrchestratorError::Build { detail,
//!   transient }`: transient for a failed step (git, IO, an MCP server, the
//!   bound), which the scheduler counts toward its pause threshold; not transient
//!   for a settings failure (`Error::Settings`, `Error::Invalid`, `NotReady`: an
//!   invalid `worker.json`, no usable commit identity, an invalid
//!   `package.json`, a provider the catalog cannot configure), which disables the
//!   schedule with its reason.
//! - After the run ends, [`Host::finish_run`] does its attempt-ref sweep and
//!   unreconciled count.
//!
//! The desktop shell's `scheduler_bg::ProjectFactory` wraps it: it opens the host
//! from `.kgraf/run.json` ([`RunConfig::load`]) on first need, reports a
//! missing or invalid file from `configured()` (shown on the tray line, logged
//! once while it lasts), reopens the host when the file's content changes and
//! no scheduled run is active, and calls `finish_run` for each
//! `TickEvent::Completed` or `Failed` (run id `ScheduledRuns::run_id`).
//!
//! **Hourly cap seeding.** Each host `ProjectFactory::open` opens goes through
//! [`HostFactory::seeded`], which seeds the new `Admission`'s trailing hour from
//! the ledger's last hour ([`Host::seed_trailing_hour`]): a reopen or an app
//! restart never resets the hourly cap. A call whose spend is not known (an
//! unreconciled call, a model the price table does not price) counts at a
//! worst-case estimate, reported as [`Notice::EstimatedSpend`]. The command
//! (`kgraf orchestrate`) does not seed; its behavior is unchanged.
//!
//! ## The orchestrator box (not merged): mapping
//!
//! Its `HostParts` needs a factory, the process's `Admission`, and the `AskDesk`
//! that answers approval asks. On its branch the factory trait has a single
//! `ready()`; merged with `main` it takes `configured()` and `ready(schedule_id)`,
//! and `InteractiveRun::begin` calls both (a box run has no schedule id; any
//! string, such as the run id, serves, since [`HostFactory`] ignores it).
//!
//! ```text
//! let desk = Arc::new(AskDesk::new(rules, wake));
//! let host = Arc::new(Host::open(config)?.with_ask_resolver(desk.clone()));
//! let (factory, _seeded) = HostFactory::seeded(host.clone())?;
//! HostParts {
//!     factory: Arc::new(factory),
//!     admission: host.admission().clone(),
//!     desk,
//! }
//! ```
//!
//! - `build(run)` runs inside `InteractiveRun::begin` on the box host's
//!   two-worker runtime, so `prepare_blocking` is allowed.
//! - [`Host::with_ask_resolver`] puts the desk in every attempt environment
//!   (`AskResolver`). The approval chain (the shell and MCP hooks) is unchanged;
//!   only who answers an ask changes.
//! - Seeding: the box does not share the scheduler's `ProjectFactory` today (its
//!   branch builds its own factory), so its factory must be made with
//!   [`HostFactory::seeded`], as above, to count the last hour's spend.
//! - The settings cap applies to `host.admission()`, which every run of the host
//!   shares. For one `Admission` across scheduled and box runs, give the
//!   scheduler's `ProjectFactory::with_host` and the box the same `Arc<Host>`,
//!   seeded once.
//! - On `RunEvent::Finished`, call `host.finish_run(&run)`.
//! - [`Host::take_notices`] returns the notices of builds made through the factory
//!   (cleaned worktrees, stranded worktrees, MCP registrations), for the box to
//!   show.
//!
//! ## In-process providers
//!
//! `Host::with_route_provider` (feature `in-process-provider`, tests only)
//! serves a route with a fake model, still behind the route's limiter and
//! through the host's ledger. It is refused for a route the catalog knows and
//! for a route no configured `route:model` uses.
//!
//! # Rules kept
//!
//! - Fail loud: every step returns [`Error`]; nothing is substituted. A
//!   limit on an unused route, an unknown config field, a shared attempt
//!   environment that cannot take the host's ask resolver: each is refused.
//! - Every token is attributed: providers come from `kgraf_fleet` (or, in
//!   tests, a fake for a route of its own), wrapped in the route's limiter, and
//!   every call goes through the host's `Ledger`.
//! - Provider-specific logic stays in the adapter crates behind `kgraf_fleet`.
//!
//! # Contract checks (off by default)
//!
//! [`RunConfig::contract_checks`] (`--contract-checks` turns it on; off until a
//! live run with a real provider shows it working and measures it) gives
//! every run the orchestrator's interface contracts: the decomposition writes
//! them, the sandboxed check runner (the gates' confinement, over the run's
//! integration branch, compile and test runs bounded apart) checks each before
//! its first party is dispatched, the handshake and any challenge review are
//! planner calls through the ledger, and every worker environment carries
//! `challenge_contract`. A check program that is not installed holds the
//! contract with the program named.

mod assembler;
mod attempt_processes;
pub mod config;
pub mod deps;
mod earlier_delivery;
pub mod error;
mod gates;
pub mod host;
pub mod land;
pub mod limits;
mod local_version;
pub mod migrate;
pub mod reasoning;
pub mod rule_conflicts;
mod scoped_check;
mod verifier;
pub mod worker;
mod workspaces;

pub use config::{ModelRouteSpec, RouteSpec, RunConfig, WorktreeRetention};
pub use error::{Error, NotReady, Result, ServerTooOld};
pub use gates::{discover_validation_commands, pytest_importable};
pub use host::{
    CorpusRequest, Host, HostFactory, Housekeeping, Notice, PreparedRun, Retained,
    RouteCapacityInput, RouteShape, RunHandle, RunOutcome, RunShape, RunStart, RunSummary, Startup,
};
pub use migrate::{MigrationReport, migrate_state_dir};
pub use workspaces::{clean, state_dir};

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod deps_gate_tests;
#[cfg(test)]
mod e2e_rt;
#[cfg(test)]
mod tests;
