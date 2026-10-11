//! The scheduler loop running in the desktop shell's background mode (decision
//! 0052). For one project, ties together:
//! - schedules on work nodes in the graph store ([`crate::schedule_store`], decision
//!   0052 section 3), with every run's history recorded on its node;
//! - the claim table, `kgraf_schedule::ScheduleStore`, at
//!   `.kgraf/schedule.db`;
//! - `kgraf_orchestrator::SchedulerLoop`, which claims due occurrences and
//!   starts them through a `ScheduledRuns`.
//!
//! **Runs are real.** [`ProjectFactory`] builds each scheduled run through
//! `kgraf-host`, the same construction `kgraf orchestrate` uses (routes and
//! providers behind their rate limiters, the ledger, git worktrees, merge-back, the
//! gate runner). Its settings come from `.kgraf/run.json` (the command's
//! per-project flags as JSON; `kgraf_host::RunConfig::load`); the price table,
//! `.kgraf/worker.json` and the credential environment variables are read as
//! the command reads them. The shell has no settings surface for routes yet, so the
//! file is written by hand. With no file, or an invalid one, `configured()` fails
//! with the reason, and `SchedulerLoop::tick` checks this *before* claiming anything
//!, so nothing is claimed or disabled for a fact true of the whole process: it
//! reports `TickEvent::NotRunnable` every tick, and the tray line shows the reason
//! (logged once per distinct reason). The file is read again on each check until
//! it opens, so adding it needs no restart; a later change of its content
//! reopens the host when no scheduled run is active, and until then the tray
//! line says the change waits. Each new host's hourly cap starts from the
//! ledger's spend of the last hour, so a reopen never resets it. After a run ends, its housekeeping
//! (the attempt-ref sweep) runs from the tick that reports the end. A build is bounded
//! (`kgraf_host::host::DEFAULT_BUILD_TIMEOUT`), so lease renewal and stop wait at most that long
//! for it.
//!
//! **Where a schedule lives.** A schedule is a property of a work node
//! (decision 0052 section 3). Each tick opens the project's graph for writing, moves
//! any schedule still in the old graph-meta layout onto a work node (logged, and
//! said on the tray line), records the tick's events as history rows on each
//! schedule's node (claimed and started, the outcome, the spend by run id, a disable
//! with its reason), and publishes the scheduled nodes to [`ScheduleBoard`] for the
//! galaxy's plan overlay. A history row that cannot be written is kept and retried
//! on the next tick, and lost outcomes are rebuilt from the claim table
//! ([`schedule_store::HistoryRecorder`]); the tray line says how many wait (counts
//! only), the log says which. The claim table, not history, decides whether a node
//! shows its schedule disabled.
//!
//! **What the tray shows.** The tray shows one aggregate line: a schedule
//! count and how many are failing, never a per-schedule list. Per-schedule detail is
//! on each schedule's work node in the graph.
//!
//! **What this thread never does**: it never creates a project's `graph.db`
//! just because `--project` pointed at it - a missing file reads as "no schedules",
//! not an error. An existing one is opened for writing, as the run host already
//! opens it, so run history can be recorded. The claim table (`schedule.db`) is this
//! thread's own responsibility to maintain, so it is created, but only once a
//! schedule actually exists to need it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use kgraf_core::RunId;
use kgraf_host::{Host, HostFactory, RunConfig};
use kgraf_orchestrator::{
    Orchestrator, OrchestratorError, OrchestratorFactory, ReadinessFailure, ScheduledRuns,
    SchedulerLoop, TickEvent, TrackedSchedule,
};
use kgraf_schedule::{ScheduleDefinition, ScheduleStore};

use crate::schedule_store;

/// How often the background thread re-reads schedules and evaluates them. Far
/// coarser than a worker round, since nothing here is latency-sensitive; a real
/// occurrence still fires within one interval of becoming due.
pub const TICK_INTERVAL: Duration = Duration::from_secs(20);

/// How often the shutdown flag is polled while waiting for the next tick
/// ("stop reacts promptly" - bounded to this, not to a whole `TICK_INTERVAL`).
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Where a project's scheduled-run settings live (see the module doc).
pub const RUN_CONFIG_FILE: &str = "run.json";

/// The scheduled runs' `OrchestratorFactory` for one project: a
/// `kgraf_host::HostFactory` over the project's [`Host`], opened from
/// `.kgraf/run.json` on first need, so every scheduled run of a host shares
/// its one `Admission`. The host is reopened when the file changes (its
/// modification time or its content) and no scheduled run is active; while one
/// is, the change waits, and [`ProjectFactory::refresh`] says so.
///
/// ```text
///   no host --configured(): load + open ok--> open (file fingerprint kept)
///   no host --configured(): load or open fails--> no host, MissingConfiguration(reason)
///   open --ready(id)/build(run)--> HostFactory
///   open --refresh: file changed, no active run--> no host (old one retired
///          until its ended runs' housekeeping is done)
///   open --refresh: file changed, a run active--> open, "applies after the active run ends"
/// ```
pub struct ProjectFactory {
    project: PathBuf,
    hosts: Mutex<Hosts>,
    /// Who answers approval asks of every run of an opened host (the box's desk).
    asks: Option<Arc<dyn kgraf_runtime::AskResolver>>,
    /// Box runs in flight (started, not yet finished): like scheduled runs, they
    /// hold a `run.json` reload back.
    box_runs: Mutex<std::collections::BTreeSet<RunId>>,
    /// The user's worktree setting (decision 0088), the only source of the
    /// policy for every run the app starts, box and scheduled alike:
    /// `run.json` cannot set it (`RunConfig::load` refuses the key). Applied
    /// to every host this factory opens and pushed to the one open now.
    /// `None` (a factory with no setting given) leaves the default, `Delete`.
    worktrees: Mutex<Option<kgraf_host::WorktreeRetention>>,
    /// The user's run limits (rounds per worker, the two output caps, the
    /// context window), shared by every project of the process. A host opened
    /// from `run.json` takes each one the file leaves out from here; a change
    /// reopens the host when no run is active, like a changed `run.json`.
    limits: kgraf_host::limits::SharedLimits,
    /// The user's live permission control (decision 0094): every host this
    /// factory opens runs under it, box and scheduled runs alike.
    permissions: Option<crate::permission_bar::PermissionUi>,
    /// Route to base URL for the readiness dial of each host this factory
    /// opens. Tests only: they point the `ollama` route at a listener they own.
    #[cfg(test)]
    local_endpoints: BTreeMap<String, String>,
}

/// The file a host was opened from: its content hash only. A touch that
/// changes nothing (an editor saving the same bytes) must not reopen the host,
/// which would start a new `Admission` for no reason. Hashed within this
/// process only, so the standard hasher serves.
type Fingerprint = u64;

#[derive(Default)]
struct Hosts {
    /// The open host, with the fingerprint of the file it came from (`None` for
    /// a host given by the caller, which is never reloaded).
    current: Option<(Arc<HostFactory>, Option<Fingerprint>)>,
    /// The user limits the current host was opened with.
    limits_applied: kgraf_host::limits::UserLimits,
    /// Hosts replaced after a change whose ended runs still await housekeeping.
    retired: Vec<Arc<HostFactory>>,
}

/// Shown while a changed `run.json` waits for the active run.
pub const RUN_CONFIG_PENDING: &str = "run.json changed; applies after the active run ends";

/// Shown while a changed run limit (Settings) waits for the active run.
pub const RUN_LIMITS_PENDING: &str = "run limits changed; apply after the active run ends";

impl ProjectFactory {
    /// A factory that opens the project's host from `.kgraf/run.json`.
    pub fn for_project(project: impl Into<PathBuf>) -> Self {
        Self {
            project: project.into(),
            hosts: Mutex::new(Hosts::default()),
            asks: None,
            box_runs: Mutex::new(std::collections::BTreeSet::new()),
            worktrees: Mutex::new(None),
            limits: kgraf_host::limits::SharedLimits::default(),
            permissions: None,
            #[cfg(test)]
            local_endpoints: BTreeMap::new(),
        }
    }

    /// Moves the readiness dial of a host this factory opened, in a test.
    #[cfg(test)]
    fn with_test_endpoints(&self, host: Host) -> kgraf_host::Result<Host> {
        self.local_endpoints
            .iter()
            .try_fold(host, |h, (r, u)| h.with_local_endpoint(r, u))
    }

    /// Outside tests the host is used as opened.
    #[cfg(not(test))]
    fn with_test_endpoints(&self, host: Host) -> kgraf_host::Result<Host> {
        Ok(host)
    }

    /// Test only: every host this factory opens dials `route` at `base_url`
    /// before a build, so a test needs no server on this machine.
    #[cfg(test)]
    fn with_local_endpoint(mut self, route: &str, base_url: &str) -> Self {
        self.local_endpoints
            .insert(route.to_owned(), base_url.to_owned());
        self
    }

    /// Every host this factory opens takes the run limits the user set from
    /// `limits`, for each one `run.json` does not name.
    pub fn with_run_limits(mut self, limits: kgraf_host::limits::SharedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Every host this factory opens runs under the user's permission control
    /// (decision 0094).
    pub fn with_permissions(mut self, permissions: crate::permission_bar::PermissionUi) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// The user's permission control, if one was given.
    pub fn permissions(&self) -> Option<crate::permission_bar::PermissionUi> {
        self.permissions.clone()
    }

    /// Every host this factory opens applies `retention` to its runs
    /// (decision 0088: the user's setting is the app's one source for it).
    pub fn with_worktree_retention(self, retention: kgraf_host::WorktreeRetention) -> Self {
        *self.worktrees.lock().unwrap_or_else(|p| p.into_inner()) = Some(retention);
        self
    }

    /// The user's setting changed: the open host applies it to the runs it
    /// builds from now on, and a host opened later starts with it.
    pub fn set_worktree_retention(&self, retention: kgraf_host::WorktreeRetention) {
        *self.worktrees.lock().unwrap_or_else(|p| p.into_inner()) = Some(retention);
        if let Some(host) = self.host() {
            host.set_worktree_retention(retention);
        }
    }

    /// Every host this factory opens sends approval asks to `asks` (the
    /// orchestrator box's desk) instead of the persisted-rule resolver.
    pub fn with_ask_resolver(mut self, asks: Arc<dyn kgraf_runtime::AskResolver>) -> Self {
        self.asks = Some(asks);
        self
    }

    /// The open host's `Admission`, which every run of the host shares; `None`
    /// until the host has been opened.
    pub fn admission(&self) -> Option<Arc<kgraf_orchestrator::Admission>> {
        self.current().map(|f| f.host().admission().clone())
    }

    /// This project's `Host`, if one is already open; `None` if it has never
    /// been opened yet. Read-only: never opens one itself. Decision 0067:
    /// the shell's capacity re-measure thread reads this to feed each open project's
    /// configured routes into `kgraf-capacity::compute`. A background
    /// measurement thread must not have the side effect of opening a
    /// project's host on its own; a project the user has not touched yet
    /// simply contributes no route inputs until it is.
    pub fn host(&self) -> Option<Arc<Host>> {
        self.current().map(|f| f.host().clone())
    }

    fn box_runs(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<RunId>> {
        self.box_runs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A box run is starting; it holds a reload back until [`Self::finish_box_run`].
    pub fn box_run_started(&self, run: &RunId) {
        self.box_runs().insert(run.clone());
    }

    /// After a box run ends: its housekeeping on whichever host built it, as
    /// lines for the box, and the hosts' notices.
    pub fn finish_box_run(&self, run: &RunId) -> Vec<String> {
        self.box_runs().remove(run);
        let factories: Vec<Arc<HostFactory>> = {
            let hosts = self.hosts();
            hosts
                .current
                .iter()
                .map(|(f, _)| f.clone())
                .chain(hosts.retired.iter().cloned())
                .collect()
        };
        let mut out = Vec::new();
        for f in &factories {
            let Some(done) = f.host().finish_run(run) else {
                continue;
            };
            match done.swept {
                Ok(swept) => out.push(format!("run {run}: swept {} attempt ref(s)", swept.len())),
                Err(e) => out.push(format!("run {run}: sweep failed, attempt refs left: {e:#}")),
            }
            match done.unreconciled {
                Ok(0) => {}
                Ok(n) => out.push(format!(
                    "run {run}: {n} call(s) with unaccounted spend; run `kgraf unreconciled`"
                )),
                Err(e) => out.push(format!("run {run}: the ledger could not be read: {e:#}")),
            }
        }
        self.hosts()
            .retired
            .retain(|f| f.host().unfinished_runs() > 0);
        out
    }

    /// The notices of builds made through this factory since the last call.
    pub fn take_notices(&self) -> Vec<String> {
        let factories: Vec<Arc<HostFactory>> = {
            let hosts = self.hosts();
            hosts
                .current
                .iter()
                .map(|(f, _)| f.clone())
                .chain(hosts.retired.iter().cloned())
                .collect()
        };
        factories
            .iter()
            .flat_map(|f| f.host().take_notices())
            .map(|n| format!("{n:?}"))
            .collect()
    }

    /// A factory over a host already opened (a settings surface, or a test with
    /// an in-process provider). It is never reloaded from a file, and not seeded
    /// here: a caller sharing the host with the orchestrator box seeds it once
    /// (`HostFactory::seeded`).
    pub fn with_host(host: Arc<Host>) -> Self {
        Self {
            project: host.root().to_path_buf(),
            hosts: Mutex::new(Hosts {
                current: Some((Arc::new(HostFactory::new(host)), None)),
                limits_applied: kgraf_host::limits::UserLimits::default(),
                retired: Vec::new(),
            }),
            asks: None,
            box_runs: Mutex::new(std::collections::BTreeSet::new()),
            worktrees: Mutex::new(None),
            limits: kgraf_host::limits::SharedLimits::default(),
            permissions: None,
            #[cfg(test)]
            local_endpoints: BTreeMap::new(),
        }
    }

    fn hosts(&self) -> std::sync::MutexGuard<'_, Hosts> {
        self.hosts.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn current(&self) -> Option<Arc<HostFactory>> {
        self.hosts().current.as_ref().map(|(f, _)| f.clone())
    }

    fn config_path(&self) -> PathBuf {
        self.project.join(".kgraf").join(RUN_CONFIG_FILE)
    }

    fn fingerprint(path: &Path) -> Option<Fingerprint> {
        use std::hash::{Hash, Hasher};
        let bytes = std::fs::read(path).ok()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        Some(h.finish())
    }

    /// Before a tick: reopen the host from a changed `run.json` when `active`
    /// (the scheduled runs still running) is zero. With a run active, the old
    /// host stays and the returned text says the change waits.
    pub fn refresh(&self, active: usize) -> Option<String> {
        // Box runs in flight hold the reload back too: replacing the host under
        // one would give later runs a second `Admission`.
        let active = active + self.box_runs().len();
        let path = self.config_path();
        let mut hosts = self.hosts();
        let file_changed = match &hosts.current {
            Some((_, Some(fp))) => Self::fingerprint(&path).as_ref() != Some(fp),
            _ => false,
        };
        // A host given by the caller (no file) is never reloaded, limits or not.
        let limits_changed = matches!(&hosts.current, Some((_, Some(_))))
            && hosts.limits_applied != self.limits.get();
        if !file_changed && !limits_changed {
            return None;
        }
        if active > 0 {
            return Some(
                if file_changed {
                    RUN_CONFIG_PENDING
                } else {
                    RUN_LIMITS_PENDING
                }
                .to_owned(),
            );
        }
        if let Some((old, _)) = hosts.current.take() {
            log::info!("{} changed; the next check opens it again", path.display());
            hosts.retired.push(old);
        }
        None
    }

    /// The host, opened now if it is not yet; the reason when it cannot be.
    fn open(&self) -> Result<Arc<HostFactory>, ReadinessFailure> {
        if let Some(f) = self.current() {
            return Ok(f);
        }
        let path = self.config_path();
        if !path.exists() {
            return Err(ReadinessFailure::MissingConfiguration(format!(
                "no scheduled-run settings: {} is absent (the `kgraf orchestrate` flags \
                 as JSON: routes, prices, cap_run, ...)",
                path.display()
            )));
        }
        // Taken before the load: a change during it is seen by the next refresh.
        let fingerprint = Self::fingerprint(&path);
        let user_worktrees = *self.worktrees.lock().unwrap_or_else(|p| p.into_inner());
        let user_limits = self.limits.get();
        let host = RunConfig::load_with(&self.project, &path, &user_limits)
            .and_then(|mut config| {
                if let Some(retention) = user_worktrees {
                    config.worktrees = retention;
                }
                config.check_configured()?;
                Host::open(config)
            })
            .and_then(|host| self.with_test_endpoints(host))
            .map(|host| match &self.asks {
                Some(asks) => host.with_ask_resolver(asks.clone()),
                None => host,
            })
            .map(|host| match &self.permissions {
                Some(p) => host.with_permissions(p.control.clone()),
                None => host,
            })
            .map_err(|e| {
                ReadinessFailure::MissingConfiguration(format!("{}: {e:#}", path.display()))
            })?;
        // A new host starts a new `Admission`: seed its trailing hour from the
        // ledger, so reopening (or restarting the app) never resets the hourly cap.
        let (factory, seeded) = HostFactory::seeded(Arc::new(host)).map_err(|e| {
            ReadinessFailure::MissingConfiguration(format!(
                "{}: the hourly cap's history cannot be read: {e:#}",
                path.display()
            ))
        })?;
        log::info!("scheduled runs: {seeded} spent in the last hour counts toward the hourly cap");
        let factory = Arc::new(factory);
        {
            let mut hosts = self.hosts();
            hosts.current = Some((factory.clone(), fingerprint));
            hosts.limits_applied = user_limits;
        }
        Ok(factory)
    }

    /// After a tick: the housekeeping of every scheduled run that tick reports
    /// ended (the attempt-ref sweep, the unreconciled count), logged, on
    /// whichever host built it. A failed sweep is said, never hidden. A retired
    /// host is dropped once none of its runs awaits this.
    pub fn finish(&self, events: &[TickEvent]) {
        let factories: Vec<Arc<HostFactory>> = {
            let hosts = self.hosts();
            hosts
                .current
                .iter()
                .map(|(f, _)| f.clone())
                .chain(hosts.retired.iter().cloned())
                .collect()
        };
        for factory in &factories {
            Self::finish_on(factory, events);
        }
        self.hosts()
            .retired
            .retain(|f| f.host().unfinished_runs() > 0);
    }

    /// A scheduled run's spend, read from the open host's ledger by run id and
    /// priced from its price table, for the run's history row on its node:
    /// settled micro-dollars, the call count, and the calls whose spend is not known
    /// counted apart. A reclaimed occurrence shares its run id with the
    /// abandoned attempt, so its spend includes that attempt's calls. `None` when no
    /// host is open, or when the ledger or price table cannot be read (logged; never
    /// shown as zero).
    pub fn run_spend(&self, run: &str) -> Option<kgraf_graph::RunSpend> {
        let factory = self.current()?;
        let host = factory.host();
        let read = || -> Result<kgraf_graph::RunSpend, String> {
            let calls = host
                .ledger()
                .calls_under_task(run, None)
                .map_err(|e| format!("the ledger cannot be read: {e}"))?;
            let prices = host
                .prices()
                .map_err(|e| format!("the price table cannot be read: {e:#}"))?;
            let ids: Vec<_> = calls.iter().map(|c| c.id.clone()).collect();
            let spend = host
                .ledger()
                .spend_of(&ids, &prices)
                .map_err(|e| format!("the ledger cannot be read: {e}"))?;
            let unpriced = spend.unknown.len();
            let text = if unpriced == 0 {
                format!("{} ({} call(s))", spend.total, calls.len())
            } else {
                format!(
                    "{} ({} call(s), {unpriced} not yet known)",
                    spend.total,
                    calls.len()
                )
            };
            Ok(kgraf_graph::RunSpend {
                micros: spend.total.micros(),
                calls: calls.len() as i64,
                unpriced: unpriced as i64,
                text,
            })
        };
        match read() {
            Ok(spend) => Some(spend),
            Err(e) => {
                log::warn!("run {run}: spend not recorded on its node: {e}");
                None
            }
        }
    }

    /// Tokens the open host's ledger attributes to `run` and its workers, by
    /// adapter. `None` when no host is open or the ledger cannot be read (logged).
    pub fn run_tokens(&self, run: &RunId) -> Option<crate::orchestrator_host::RunTokens> {
        let factory = self.current()?;
        match factory.host().ledger().calls_under_task(run.as_str(), None) {
            Ok(calls) => Some(crate::orchestrator_host::RunTokens::from_calls(
                calls.iter().map(|c| (c.adapter.as_str(), &c.usage)),
            )),
            Err(e) => {
                log::warn!("run {run}: the ledger cannot be read for its tokens: {e}");
                None
            }
        }
    }

    fn finish_on(factory: &HostFactory, events: &[TickEvent]) {
        for event in events {
            let (TickEvent::Completed {
                schedule_id,
                due_at,
            }
            | TickEvent::Failed {
                schedule_id,
                due_at,
                ..
            }) = event
            else {
                continue;
            };
            let run = ScheduledRuns::run_id(schedule_id, *due_at);
            let Some(done) = factory.host().finish_run(&run) else {
                continue;
            };
            match done.swept {
                Ok(swept) => log::info!("run {run}: swept {} attempt ref(s)", swept.len()),
                Err(e) => log::warn!("run {run}: sweep failed, attempt refs left: {e:#}"),
            }
            match done.unreconciled {
                Ok(0) => {}
                Ok(n) => log::warn!(
                    "run {run}: {n} call(s) with unaccounted spend; run `kgraf unreconciled`"
                ),
                Err(e) => log::warn!("run {run}: the ledger could not be read: {e:#}"),
            }
        }
        for notice in factory.host().take_notices() {
            match notice {
                // An estimate in place of a price is said where it is seen.
                kgraf_host::Notice::EstimatedSpend { .. } => {
                    log::warn!("hourly cap: {notice:?}")
                }
                _ => log::info!("scheduled run: {notice:?}"),
            }
        }
    }
}

#[async_trait::async_trait]
impl OrchestratorFactory for ProjectFactory {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        self.open()?.configured().await
    }

    async fn ready(&self, schedule_id: &str) -> Result<(), ReadinessFailure> {
        self.open()?.ready(schedule_id).await
    }

    fn build(&self, run: &RunId) -> Result<Orchestrator, OrchestratorError> {
        match self.current() {
            Some(f) => f.build(run),
            None => Err(OrchestratorError::Build {
                detail: "the project's host is not open: configured() must pass first".into(),
                transient: true,
            }),
        }
    }
}

/// One aggregate line the tray shows: a count, never a per-schedule
/// list. `kgraf schedule list` and the graph view are where the detail lives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleSummary {
    total: usize,
    failing: usize,
    /// Every failure path in this thread (opening the runtime, the claim table,
    /// `.kgraf`, reading definitions, building a schedule) sets this instead of
    /// leaving the previous, now-stale summary in place.
    error: Option<String>,
    /// Decision 0052's global switch is off. Distinct from "0
    /// schedules" - schedules exist and are simply not being evaluated right now.
    suspended: bool,
    /// A note after the count, such as a changed `run.json` waiting for the
    /// active run ([`RUN_CONFIG_PENDING`]).
    notice: Option<String>,
}

impl ScheduleSummary {
    pub fn line(&self) -> String {
        if let Some(err) = &self.error {
            return format!("schedules: error - {err}");
        }
        if self.suspended {
            return if self.total == 0 {
                "schedules: suspended (background mode is off)".to_owned()
            } else {
                format!(
                    "{} schedule(s); suspended (background mode is off)",
                    self.total
                )
            };
        }
        let line = match (self.total, self.failing) {
            (0, _) => "schedules: none".to_owned(),
            (n, 0) => format!("{n} schedule(s); see graph"),
            (n, f) => format!("{n} schedule(s), {f} failing; see graph"),
        };
        match &self.notice {
            Some(n) => format!("{line} - {n}"),
            None => line,
        }
    }
}

/// A live, lock-protected snapshot the tray reads each frame; the background thread
/// is the only writer.
#[derive(Default)]
pub struct ScheduleBoard {
    summary: Mutex<ScheduleSummary>,
    /// The scheduled work nodes for the galaxy's plan overlay: each schedule on
    /// its node, with its last run and a disable reason on the label. This is the
    /// graph's own content, not a list surface.
    nodes: Mutex<Vec<kgraf_galaxy::PlanNode>>,
    /// Failed scheduled runs, read from the graph's durable history each tick
    /// (`schedule_store::failed_runs`): (key, words).
    failed: Mutex<Vec<(String, String)>>,
    /// Called when the summary changes, so the UI redraws the tray line then and
    /// only then (no timer repaint while nothing changes).
    wake: Mutex<Option<Wake>>,
}

/// Called to wake the UI.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

impl ScheduleBoard {
    pub fn snapshot(&self) -> ScheduleSummary {
        self.summary
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn set(&self, summary: ScheduleSummary) {
        let changed = {
            let mut cur = self.summary.lock().unwrap_or_else(|p| p.into_inner());
            let changed = *cur != summary;
            *cur = summary;
            changed
        };
        if changed {
            let wake = self.wake.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(wake) = wake {
                wake();
            }
        }
    }

    /// The scheduled runs whose newest run row in the graph's history is a
    /// failure, one per schedule: (key, words). The key names the history row, so
    /// each attempt has its own and a dismissed one never hides the next. A later
    /// start or completion of the schedule supersedes it. Read from the graph each
    /// tick, so a failure recorded while the app was closed shows on the next
    /// launch (final check item 5). The window shows each as a pinned card.
    pub fn failed_runs(&self) -> Vec<(String, String)> {
        self.failed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Sets [`Self::failed_runs`]; wakes the UI when it changed.
    pub fn set_failed(&self, failed: Vec<(String, String)>) {
        let changed = {
            let mut cur = self.failed.lock().unwrap_or_else(|p| p.into_inner());
            let changed = *cur != failed;
            *cur = failed;
            changed
        };
        if changed {
            let wake = self.wake.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(wake) = wake {
                wake();
            }
        }
    }

    /// The scheduled work nodes as last derived from the graph.
    pub fn plan_nodes(&self) -> Vec<kgraf_galaxy::PlanNode> {
        self.nodes.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_nodes(&self, nodes: Vec<kgraf_galaxy::PlanNode>) {
        let changed = {
            let mut cur = self.nodes.lock().unwrap_or_else(|p| p.into_inner());
            let changed = *cur != nodes;
            *cur = nodes;
            changed
        };
        if changed {
            let wake = self.wake.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(wake) = wake {
                wake();
            }
        }
    }

    /// Wake the UI with `wake` whenever the summary changes.
    pub fn set_wake(&self, wake: Wake) {
        *self.wake.lock().unwrap_or_else(|p| p.into_inner()) = Some(wake);
    }

    fn set_error(&self, detail: impl Into<String>) {
        self.set(ScheduleSummary {
            error: Some(detail.into()),
            ..ScheduleSummary::default()
        });
    }
}

/// Pauses one scheduler from any thread (see [`SchedulerHandle::pause`]).
pub struct Pauser {
    enabled: Arc<AtomicBool>,
    tick_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Pauser {
    /// See [`SchedulerHandle::pause`]. Blocks up to `wait`: call it off the UI
    /// thread.
    pub fn pause(&self, wait: Duration) -> Option<SchedulerPause> {
        let was = self.enabled.swap(false, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Ok(guard) = self.tick_lock.clone().try_lock_owned() {
                return Some(SchedulerPause {
                    enabled: self.enabled.clone(),
                    was,
                    keep: false,
                    _guard: guard,
                });
            }
            if std::time::Instant::now() >= deadline {
                self.enabled.store(was, Ordering::SeqCst);
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// A paused scheduler (see [`SchedulerHandle::pause`]).
pub struct SchedulerPause {
    enabled: Arc<AtomicBool>,
    was: bool,
    keep: bool,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl SchedulerPause {
    /// Leave the scheduler off when this guard goes (the project closes).
    pub fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for SchedulerPause {
    fn drop(&mut self) {
        if !self.keep {
            self.enabled.store(self.was, Ordering::SeqCst);
        }
    }
}

/// Owns the background scheduler thread. Dropping it stops the thread (promptly,
/// polled every [`STOP_POLL_INTERVAL`]) and joins it, so the thread never outlives
/// the `ShellApp` that started it and never leaks past a dropped board.
pub struct SchedulerHandle {
    stop: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    board: Arc<ScheduleBoard>,
    join: Option<std::thread::JoinHandle<()>>,
    /// Held by the thread from its enabled check through its tick, and by
    /// [`SchedulerHandle::pause`]: a pause waits out a tick in progress.
    tick_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SchedulerHandle {
    /// Spawns the background scheduler thread for `project`.
    pub fn spawn(project: PathBuf, background_mode: bool) -> Self {
        let factory = Arc::new(ProjectFactory::for_project(&project));
        Self::spawn_with(project, background_mode, factory, None)
    }

    /// Spawns the scheduler over `factory`, shared with the orchestrator box so
    /// scheduled and box runs use one host and one `Admission`. `wake` runs when
    /// the tray line changes.
    pub fn spawn_with(
        project: PathBuf,
        background_mode: bool,
        factory: Arc<ProjectFactory>,
        wake: Option<Wake>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let enabled = Arc::new(AtomicBool::new(background_mode));
        let board = Arc::new(ScheduleBoard::default());
        if let Some(wake) = wake {
            board.set_wake(wake);
        }
        let (stop_t, enabled_t, board_t) = (stop.clone(), enabled.clone(), board.clone());
        let tick_lock = Arc::new(tokio::sync::Mutex::new(()));
        let lock_t = tick_lock.clone();
        let join = std::thread::Builder::new()
            .name("kgraf-scheduler".to_owned())
            .spawn(move || run_forever(&project, &stop_t, &enabled_t, &board_t, factory, &lock_t))
            .ok();
        Self {
            stop,
            enabled,
            board,
            join,
            tick_lock,
        }
    }

    pub fn board(&self) -> Arc<ScheduleBoard> {
        self.board.clone()
    }

    /// Stops new scheduled runs and waits out a tick in progress, up to
    /// `wait`: while the guard lives, nothing is claimed or admitted, so a
    /// caller can check the project is idle and close it without a scheduled
    /// run slipping in between. Dropping the guard restores the switch;
    /// [`SchedulerPause::keep`] leaves it off (the project closes). `None` when a
    /// tick did not end in time.
    pub fn pause(&self, wait: Duration) -> Option<SchedulerPause> {
        self.pauser().pause(wait)
    }

    /// What [`Self::pause`] needs, to pause from another thread: a close waits
    /// for a tick there, never on the UI thread (final check item 3).
    pub fn pauser(&self) -> Pauser {
        Pauser {
            enabled: self.enabled.clone(),
            tick_lock: self.tick_lock.clone(),
        }
    }

    /// Decision 0052's global background-mode switch: turning it off suspends every
    /// schedule (no claim is ever attempted while off), without stopping the thread
    /// itself - flipping it back on resumes immediately, on the next tick.
    pub fn set_background_mode(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Whether scheduled runs may start (the background-mode switch).
    pub fn background_mode(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
}

/// How long `Drop` waits for the scheduler thread before giving up on it ("join with a timeout":
/// app shutdown must never hang forever on a thread that, for whatever reason, does not stop
/// promptly).
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(join) = self.join.take() else {
            return;
        };
        // `JoinHandle::join` has no timeout of its own; a second thread that only
        // exists to signal completion is the standard way to bound the wait without
        // it, at the cost of the original thread staying detached (never observed
        // again by this process) if it truly never stops.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = join.join();
            let _ = tx.send(());
        });
        if rx.recv_timeout(SHUTDOWN_JOIN_TIMEOUT).is_err() {
            log::error!(
                "scheduler thread did not stop within {:?}; abandoning it rather than blocking shutdown",
                SHUTDOWN_JOIN_TIMEOUT
            );
        }
    }
}

fn run_forever(
    project: &Path,
    stop: &AtomicBool,
    enabled: &AtomicBool,
    board: &ScheduleBoard,
    factory: Arc<ProjectFactory>,
    tick_lock: &tokio::sync::Mutex<()>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            board.set_error(format!("cannot start the scheduler's async runtime: {e}"));
            return;
        }
    };
    // One runtime, kept alive for the thread's whole lifetime inside a single
    // `block_on`, not rebuilt every tick - a real `OrchestratorFactory`'s spawned
    // run tasks (`tokio::spawn`, inside `ScheduledRuns::start_run`) need the runtime
    // to keep polling *between* ticks to make progress at all; a runtime that only
    // exists for the duration of one `block_on(tick())` call and is then dropped
    // (or a fresh one built next tick) would starve anything it spawned the moment
    // that call returned.
    runtime.block_on(run_loop(project, stop, enabled, board, factory, tick_lock));
}

async fn run_loop(
    project: &Path,
    stop: &AtomicBool,
    enabled: &AtomicBool,
    board: &ScheduleBoard,
    factory: Arc<ProjectFactory>,
    tick_lock: &tokio::sync::Mutex<()>,
) {
    let state_dir = project.join(".kgraf");
    let claimant = format!("kgraf-app-{}", std::process::id());
    // The store and the loop are built once real schedules exist, and kept for
    // the rest of this thread's life - not reconstructed every tick, which would
    // discard `SchedulerLoop`'s own pending-run bookkeeping and its `ScheduledRuns`
    // concurrency cap on every single evaluation. `starter` is kept alongside
    // `loop_` (not only inside it) so `set_instructions` can be called on it every
    // tick, independently of whatever `loop_` itself is doing (an
    // empty, frozen instruction map meant a schedule added after the loop first
    // started ran with no instruction at all).
    let mut state: Option<LoopState> = None;
    // History rows not yet written, retried each tick; lost outcomes
    // are rebuilt from the claim table.
    let mut recorder = schedule_store::HistoryRecorder::default();
    // A schedule stays "failing" on the board until it actually
    // succeeds, not only for the tick its failure happened on - otherwise the tray
    // line flickers back to "healthy" the instant a failed schedule's next tick has
    // nothing new to report.
    let mut failing: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Each distinct "cannot run anything" reason is logged once, not every tick.
    let mut warned: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    while !stop.load(Ordering::Relaxed) {
        // Schedules live on work nodes in the project's graph. Opened for
        // writing facts once per tick (never created, never rebuilt), so each
        // run's history is recorded on its node; old meta-stored schedules are moved
        // onto nodes here.
        let opened = match schedule_store::open(project) {
            Ok(Some((graph, moved))) => {
                schedule_store::load_all(&graph).map(|s| Some((graph, s, moved)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        match opened {
            Err(e) => board.set_error(e.to_string()),
            Ok(None) => {
                failing.clear();
                board.set_nodes(Vec::new());
                board.set(ScheduleSummary {
                    suspended: !enabled.load(Ordering::Relaxed),
                    ..ScheduleSummary::default()
                });
            }
            Ok(Some((graph, schedules, moved))) => {
                let defs: Vec<ScheduleDefinition> =
                    schedules.into_iter().map(|s| s.definition).collect();
                let mut notes = migration_notes(&mut recorder, &moved);
                let mut events = Vec::new();
                let mut tick_attention = None;
                // Held from the enabled check through the tick: a pause
                // (`SchedulerHandle::pause`, a project closing) waits out a tick
                // in progress, and no tick starts after it (low item).
                let _tick = tick_lock.lock().await;
                if !enabled.load(Ordering::Relaxed) {
                    board.set(ScheduleSummary {
                        total: defs.iter().filter(|d| d.enabled).count(),
                        suspended: true,
                        ..ScheduleSummary::default()
                    });
                } else if defs.is_empty() {
                    failing.clear();
                    board.set(ScheduleSummary::default());
                } else {
                    match ensure_state(&mut state, &state_dir, &claimant, &factory) {
                        Ok((active_loop, starter)) => {
                            let tick =
                                tick_once(active_loop, starter, &factory, &defs, Utc::now()).await;
                            update_failing(&mut failing, &defs, &tick.build_failures, &tick.events);
                            events = tick.events.clone();
                            tick_attention = Some(tick);
                        }
                        Err(e) => board.set_error(e),
                    }
                }
                let spend = |run: &str| factory.run_spend(run);
                // 1. This tick's rows and the retry queue, with no claim-table lock
                //    held: a busy graph never keeps the loop's
                //    lease renewals waiting.
                let mut report = recorder.write(
                    &graph,
                    schedule_store::records_of(&events, &spend, Utc::now()),
                );
                // 2. Read the claim table: the loop's own connection while it runs,
                //    else the file on disk opened read-only when one exists (never
                //    created here: a suspended background mode must not create it). The
                //    lock is held for reads only.
                let spare = match &state {
                    Some(_) => None,
                    None => open_existing_claims(&state_dir),
                };
                let (lost, reasons) = {
                    let guard = state
                        .as_ref()
                        .map(|s| s.store.lock().unwrap_or_else(|p| p.into_inner()));
                    let claims: Option<&ScheduleStore> = guard.as_deref().or(spare.as_ref());
                    let lost = match claims {
                        Some(c) if !report.stopped_busy => {
                            Some(recorder.find_lost(&graph, c, &spend))
                        }
                        _ => None,
                    };
                    let reasons = schedule_store::views(&graph).and_then(|views| {
                        let reasons = schedule_store::disabled_reasons(claims, &views)?;
                        Ok((views, reasons))
                    });
                    (lost, reasons)
                };
                // 3. Write what was lost, again with no lock held.
                match lost {
                    Some(Ok(rows)) => report.absorb_rebuilt(recorder.write(&graph, rows)),
                    Some(Err(e)) => report.errors.push(e),
                    None => {}
                }
                // Failures come from the durable history, written above, so one
                // recorded by an earlier process or while this app was closed shows.
                match schedule_store::failed_runs(&graph) {
                    Ok(failed) => board.set_failed(failed),
                    Err(e) => notes.push(format!("cannot read failed scheduled runs: {e}")),
                }
                notes.extend(history_notes(&report));
                if let Some(tick) = &tick_attention {
                    let attention = attention(tick, &mut warned, &notes);
                    update_board(&defs, &failing, board, attention);
                }
                let nodes = reasons.map(|(views, reasons)| {
                    schedule_store::plan_nodes(&views, &|id: &str| reasons.get(id).cloned())
                });
                match nodes {
                    Ok(nodes) => board.set_nodes(nodes),
                    Err(e) => board.set_error(e.to_string()),
                }
            }
        }
        sleep_until_next_tick_or_stop(stop).await;
    }

    // A graceful shutdown drains whatever already finished (so a real result
    // is recorded, not thrown away) and gives back whatever is still genuinely
    // pending, immediately and visibly, rather than leaving it to expire on its own
    // lease - this instance may be the one restarting next, or another instance may
    // be running; either way there is no reason to make it wait.
    if let Some(st) = state.as_mut() {
        let events = st.loop_.release_pending(Utc::now());
        for event in &events {
            log::info!("scheduler shutdown: {event:?}");
        }
        // What the drain settled is recorded on the nodes too; what cannot be
        // written now is rebuilt from the claim table on the next start.
        if let Ok(Some((graph, _))) = schedule_store::open(project) {
            let spend = |run: &str| factory.run_spend(run);
            let report = recorder.record_tick(
                &graph,
                schedule_store::records_of(&events, &spend, Utc::now()),
                None,
                &spend,
            );
            for e in report.errors.iter().chain(&report.dropped) {
                log::warn!("scheduler shutdown: {e}");
            }
        }
    }
}

/// The scheduler's claim table, loop and run starter, built once a schedule exists.
struct LoopState {
    loop_: SchedulerLoop,
    starter: Arc<ScheduledRuns>,
    /// The loop's own claim table connection, shared for reading run outcomes and
    /// disable reasons.
    store: Arc<Mutex<ScheduleStore>>,
}

/// The claim table on disk opened read-only, when one exists: no schema batch, no
/// write. Never creates it.
fn open_existing_claims(state_dir: &Path) -> Option<ScheduleStore> {
    let path = state_dir.join("schedule.db");
    if !path.exists() {
        return None;
    }
    match ScheduleStore::open_read_only(&path) {
        Ok(store) => Some(store),
        Err(e) => {
            log::warn!("cannot open the claim table {}: {e}", path.display());
            None
        }
    }
}

/// The tray's words for the schedules moved or refused this tick: counts only,
/// never names. Each is logged by name. A refused row is said once per
/// process.
fn migration_notes(
    recorder: &mut schedule_store::HistoryRecorder,
    moved: &[kgraf_schedule::Migration],
) -> Vec<String> {
    let said = recorder.migrations_to_report(moved);
    let mut moved_n = 0;
    let mut refused_n = 0;
    for m in &said {
        log::warn!("{m}");
        match m {
            kgraf_schedule::Migration::Refused { .. } => refused_n += 1,
            _ => moved_n += 1,
        }
    }
    let mut notes = Vec::new();
    if moved_n > 0 {
        notes.push(format!(
            "{moved_n} schedule(s) moved onto work nodes; see graph"
        ));
    }
    if refused_n > 0 {
        notes.push(format!(
            "{refused_n} old schedule(s) not moved and not running; see log"
        ));
    }
    notes
}

/// The tray's words for a tick's history writes: counts only. The
/// detail goes to the log.
fn history_notes(report: &schedule_store::TickRecord) -> Vec<String> {
    for e in report.errors.iter().chain(&report.dropped) {
        log::error!("{e}");
    }
    if report.rebuilt > 0 {
        log::warn!(
            "{} run outcome(s) rebuilt on work nodes from the claim table",
            report.rebuilt
        );
    }
    let mut notes = Vec::new();
    if report.pending > 0 {
        notes.push(format!(
            "{} history row(s) not yet written; retrying",
            report.pending
        ));
    }
    if !report.dropped.is_empty() {
        notes.push(format!(
            "{} history row(s) had no work node; see log",
            report.dropped.len()
        ));
    }
    notes
}

/// One evaluation: the current instructions, the tick, and the housekeeping of
/// the runs it reports ended. Returns the tick's events and the definitions that
/// no longer build.
async fn tick_once(
    active_loop: &mut SchedulerLoop,
    starter: &ScheduledRuns,
    factory: &ProjectFactory,
    defs: &[ScheduleDefinition],
    now: chrono::DateTime<Utc>,
) -> Tick {
    starter.set_instructions(
        defs.iter()
            .map(|d| (d.id.clone(), d.instruction.clone()))
            .collect(),
    );
    let pending = factory.refresh(starter.active());
    let (tracked, build_failures) = track_schedules(defs);
    let events = active_loop.tick(&tracked, now).await;
    factory.finish(&events);
    Tick {
        events,
        build_failures,
        pending,
    }
}

/// What one evaluation produced.
struct Tick {
    events: Vec<TickEvent>,
    /// Definitions that no longer build.
    build_failures: std::collections::BTreeSet<String>,
    /// A changed `run.json` waiting for the active run.
    pending: Option<String>,
}

/// What the tray line says beyond the counts.
#[derive(Debug, PartialEq, Eq)]
enum Attention {
    /// Nothing can run (`TickEvent::NotRunnable`): the reason replaces the count.
    NotRunnable(String),
    /// A note after the count.
    Notice(String),
}

/// The tray's attention for a tick. A not-runnable reason is logged once per
/// distinct text while it lasts (`warned` keeps the current one); when it
/// clears, or another replaces it, it is forgotten, so its return is logged
/// again.
fn attention(
    tick: &Tick,
    warned: &mut std::collections::BTreeSet<String>,
    notes: &[String],
) -> Option<Attention> {
    let not_runnable = tick.events.iter().find_map(|e| match e {
        TickEvent::NotRunnable { detail, .. } => Some(detail.clone()),
        _ => None,
    });
    warned.retain(|w| not_runnable.as_ref() == Some(w));
    if let Some(detail) = not_runnable {
        if warned.insert(detail.clone()) {
            log::warn!("scheduled runs cannot start: {detail}");
        }
        return Some(Attention::NotRunnable(detail));
    }
    // A history row that could not be written, or a schedule moved from the old
    // store, is said after the count, ahead of a waiting run.json change.
    let mut all: Vec<String> = notes.to_vec();
    all.extend(tick.pending.clone());
    (!all.is_empty()).then(|| Attention::Notice(all.join("; ")))
}

async fn sleep_until_next_tick_or_stop(stop: &AtomicBool) {
    let mut waited = Duration::ZERO;
    while waited < TICK_INTERVAL {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let step = STOP_POLL_INTERVAL.min(TICK_INTERVAL - waited);
        tokio::time::sleep(step).await;
        waited += step;
    }
}

/// Builds the claim table, the `ScheduledRuns`, and the loop the first time a
/// schedule actually exists (no `.kgraf/schedule.db` for a project nobody has
/// scheduled anything in), and hands back a reference to both from then on. The
/// instruction map inside the returned `ScheduledRuns` starts empty; the caller sets
/// it (`set_instructions`) every tick from the current definitions.
fn ensure_state<'a>(
    state: &'a mut Option<LoopState>,
    state_dir: &Path,
    claimant: &str,
    factory: &Arc<ProjectFactory>,
) -> Result<(&'a mut SchedulerLoop, &'a Arc<ScheduledRuns>), String> {
    if state.is_none() {
        std::fs::create_dir_all(state_dir)
            .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;
        let store = ScheduleStore::open(&state_dir.join("schedule.db"))
            .map_err(|e| format!("cannot open the claim table: {e}"))?;
        let starter = Arc::new(ScheduledRuns::new(factory.clone(), BTreeMap::new(), 1));
        let on_time_window = TimeDelta::from_std(TICK_INTERVAL * 2)
            .unwrap_or(kgraf_orchestrator::DEFAULT_ON_TIME_WINDOW);
        let store = Arc::new(Mutex::new(store));
        let loop_ = SchedulerLoop::new(store.clone(), starter.clone(), claimant.to_owned())
            .with_on_time_window(on_time_window);
        *state = Some(LoopState {
            loop_,
            starter,
            store,
        });
    }
    match state.as_mut() {
        Some(st) => Ok((&mut st.loop_, &st.starter)),
        None => Err("the scheduler state was not built".to_owned()),
    }
}

/// Builds this tick's `TrackedSchedule` list, and separately names every definition
/// whose own `to_schedule()` failed (these must show as failing on the
/// board, not just be logged and silently dropped from the tick - the previous
/// version's caller never learned they existed).
fn track_schedules(
    defs: &[ScheduleDefinition],
) -> (Vec<TrackedSchedule>, std::collections::BTreeSet<String>) {
    let mut tracked = Vec::new();
    let mut build_failures = std::collections::BTreeSet::new();
    for d in defs {
        match d.to_schedule() {
            Ok(schedule) => tracked.push(TrackedSchedule {
                schedule,
                enabled: d.enabled,
                // Whole milliseconds on load, like every schedule instant.
                created_at: d.created_at_millis(),
            }),
            Err(e) => {
                log::error!("schedule `{}` no longer builds: {e}", d.id);
                build_failures.insert(d.id.clone());
            }
        }
    }
    (tracked, build_failures)
}

fn describe_event(event: &TickEvent) -> (String, bool) {
    match event {
        TickEvent::Started { schedule_id, .. }
        | TickEvent::StartedAfterDeferral { schedule_id, .. }
        | TickEvent::Reclaimed { schedule_id, .. } => (schedule_id.clone(), false),
        TickEvent::Completed { schedule_id, .. } => (schedule_id.clone(), false),
        TickEvent::Failed { schedule_id, .. }
        | TickEvent::Disabled { schedule_id, .. }
        | TickEvent::NotRunnable { schedule_id, .. } => (schedule_id.clone(), true),
        TickEvent::ClockAnomaly { schedule_id, .. } => (schedule_id.clone(), false),
        // A capacity refusal is not this schedule's
        // own failure - it says nothing about whether its configuration works, so
        // it must not mark it failing on the board any more than a `Started` or
        // `Reclaimed` firing would.
        TickEvent::Deferred { schedule_id, .. } => (schedule_id.clone(), false),
        // A lost lease is an anomaly about this instance's own claim,
        // not proof the run itself failed - the same reasoning `ClockAnomaly` already
        // gets.
        TickEvent::LeaseLost { schedule_id, .. } => (schedule_id.clone(), false),
        // A drop is the same "missed, not late, not a failure" outcome
        // `Skip` already applies elsewhere.
        TickEvent::Dropped { schedule_id, .. } => (schedule_id.clone(), false),
        // A bookkeeping warning about spend attribution, not about
        // whether the run itself succeeded.
        TickEvent::RunIdNotRecorded { schedule_id, .. } => (schedule_id.clone(), false),
    }
}

/// Updates the persistent failing-id set: a schedule stays failing
/// until it actually completes successfully, not only for the tick its own failure
/// happened on, and a schedule removed from the definitions entirely is dropped from
/// the set too (it cannot be reported on any longer).
fn update_failing(
    failing: &mut std::collections::BTreeSet<String>,
    defs: &[ScheduleDefinition],
    build_failures: &std::collections::BTreeSet<String>,
    events: &[TickEvent],
) {
    let known: std::collections::BTreeSet<&str> = defs.iter().map(|d| d.id.as_str()).collect();
    failing.retain(|id| known.contains(id.as_str()));
    for id in build_failures {
        failing.insert(id.clone());
    }
    for event in events {
        let (id, is_failure) = describe_event(event);
        if is_failure {
            failing.insert(id);
        } else if matches!(event, TickEvent::Completed { .. }) {
            failing.remove(&id);
        }
    }
}

fn update_board(
    defs: &[ScheduleDefinition],
    failing: &std::collections::BTreeSet<String>,
    board: &ScheduleBoard,
    attention: Option<Attention>,
) {
    let total = defs.iter().filter(|d| d.enabled).count();
    let (error, notice) = match attention {
        Some(Attention::NotRunnable(d)) => (Some(d), None),
        Some(Attention::Notice(n)) => (None, Some(n)),
        None => (None, None),
    };
    board.set(ScheduleSummary {
        total,
        failing: failing.len(),
        error,
        suspended: false,
        notice,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the failed checks of every run under the project `root` said:
    /// the detail of each `verdict` event of its traces that did not pass,
    /// first lines each, once. A test that expects a run to complete prints
    /// it, since a blocked run's reason says only how many attempts failed,
    /// so a failure the platform caused (a check program that could not
    /// start) names its cause.
    fn failed_checks(root: &Path) -> String {
        let mut out: Vec<String> = Vec::new();
        let runs = std::fs::read_dir(root.join(".kgraf/runs"))
            .into_iter()
            .flatten()
            .flatten();
        for run in runs {
            let trace = std::fs::read_to_string(run.path().join("trace.jsonl")).unwrap_or_default();
            for line in trace.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let event = v.get("event").unwrap_or(&v);
                if event.get("event").and_then(|e| e.as_str()) != Some("verdict")
                    || event.get("passed").and_then(|p| p.as_bool()) != Some(false)
                {
                    continue;
                }
                let detail = event.get("detail").and_then(|d| d.as_str()).unwrap_or("");
                let head: Vec<&str> = detail.lines().filter(|l| !l.is_empty()).take(4).collect();
                let head = head.join(" | ");
                if !out.contains(&head) {
                    out.push(head);
                }
            }
        }
        if out.is_empty() {
            "failed checks: none in the traces".to_owned()
        } else {
            format!("failed checks:\n{}", out.join("\n"))
        }
    }

    #[test]
    fn failed_checks_name_each_failed_verdict_of_every_run_once() {
        let dir = tempfile::tempdir().expect("dir");
        let run = dir.path().join(".kgraf/runs/r1");
        std::fs::create_dir_all(&run).expect("mkdir");
        let failed = r#"{"seq":1,"event":{"event":"verdict","passed":false,"detail":"`test -f made.txt` failed: $ test -f made.txt\n\n[exit code: 0xc0000142]"}}"#;
        let passed = r#"{"seq":2,"event":{"event":"verdict","passed":true,"detail":"ok"}}"#;
        std::fs::write(
            run.join("trace.jsonl"),
            format!("{failed}\n{failed}\n{passed}\n"),
        )
        .expect("trace");
        assert_eq!(
            failed_checks(dir.path()),
            "failed checks:\n`test -f made.txt` failed: $ test -f made.txt | [exit code: 0xc0000142]"
        );
        let empty = tempfile::tempdir().expect("dir");
        assert_eq!(
            failed_checks(empty.path()),
            "failed checks: none in the traces"
        );
    }

    #[test]
    fn the_aggregate_line_never_names_a_schedule() {
        let summary = ScheduleSummary {
            total: 3,
            failing: 1,
            error: None,
            suspended: false,
            notice: None,
        };
        let line = summary.line();
        assert!(line.contains('3') && line.contains('1'), "{line}");
        assert!(
            !line.contains("s1") && !line.contains("nightly"),
            "the tray line must be an aggregate, never a schedule id: {line}"
        );
    }

    #[test]
    fn an_error_replaces_the_count_entirely_so_nothing_looks_stale() {
        let summary = ScheduleSummary {
            total: 5,
            failing: 0,
            error: Some("cannot open the claim table".to_owned()),
            suspended: false,
            notice: None,
        };
        let line = summary.line();
        assert!(line.contains("error"), "{line}");
        assert!(
            !line.contains('5'),
            "an error state must not also show a stale-looking count: {line}"
        );
    }

    #[test]
    fn zero_schedules_and_all_disabled_both_read_as_none() {
        assert_eq!(ScheduleSummary::default().line(), "schedules: none");
    }

    #[test]
    fn suspended_is_shown_distinctly_from_zero_schedules() {
        let summary = ScheduleSummary {
            total: 2,
            suspended: true,
            ..ScheduleSummary::default()
        };
        let line = summary.line();
        assert!(line.contains("suspended"), "{line}");
        assert!(line.contains('2'), "{line}");
    }

    /// Decision 0052's global switch: with background mode off from the
    /// start, the real background thread must never claim anything - proved here by
    /// checking the claim table file is never even created, not just by reading a
    /// flag this same code also sets. A schedule exists, stored the old way (a
    /// graph-meta row) so there is something to *not* claim; the thread still moves
    /// it onto a work node and puts that node on the graph while suspended.
    #[test]
    fn background_mode_off_from_the_start_never_claims_anything() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().to_path_buf();
        let state_dir = project.join(".kgraf");
        std::fs::create_dir_all(&state_dir).expect("mkdir");
        let graph =
            kgraf_graph::GraphStore::open(&state_dir.join("graph.db"), &project).expect("graph");
        let def =
            ScheduleDefinition::one_shot("nightly", "do it", Utc::now(), "UTC").expect("valid");
        graph
            .set_meta(
                &kgraf_schedule::schedule_key(&def.id),
                &serde_json::to_string(&def).unwrap(),
            )
            .unwrap();
        graph
            .set_meta(
                kgraf_schedule::INDEX_KEY,
                &serde_json::to_string(&vec![def.id.clone()]).unwrap(),
            )
            .unwrap();
        drop(graph);

        let handle = SchedulerHandle::spawn(project.clone(), false);
        // The loop's first iteration runs with no initial sleep. Poll until it
        // has moved the schedule onto its node (a deadline, not a fixed
        // sleep, so a slow machine does not fail the test).
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while (!handle.board().snapshot().suspended || handle.board().plan_nodes().is_empty())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let summary = handle.board().snapshot();
        assert!(summary.suspended, "got {summary:?}");
        assert!(
            !state_dir.join("schedule.db").exists(),
            "background mode off must never create the claim table at all"
        );
        let nodes = handle.board().plan_nodes();
        assert_eq!(nodes.len(), 1, "{nodes:?}");
        assert_eq!(nodes[0].key, "work:schedules/nightly");
        drop(handle);
        let graph =
            kgraf_graph::GraphStore::open(&state_dir.join("graph.db"), &project).expect("graph");
        assert!(
            graph
                .meta(&kgraf_schedule::schedule_key("nightly"))
                .expect("meta")
                .is_none(),
            "the old row was moved onto a work node"
        );
        assert_eq!(
            graph.schedule("nightly").expect("read").map(|s| s.node_id),
            Some("schedules/nightly".to_owned())
        );
    }

    #[test]
    fn update_board_counts_only_enabled_definitions() {
        let mut off = ScheduleDefinition::recurring("off", "x", "0 0 3 * * *", "UTC").unwrap();
        off.enabled = false;
        let on = ScheduleDefinition::recurring("on", "x", "0 0 4 * * *", "UTC").unwrap();
        let board = ScheduleBoard::default();
        update_board(&[off, on], &std::collections::BTreeSet::new(), &board, None);
        assert_eq!(board.snapshot().total, 1);
    }

    /// A reason nothing can run replaces the count on the tray line, and is
    /// logged once per distinct text; a waiting `run.json` change is a note
    /// after the count.
    #[test]
    fn a_not_runnable_reason_reaches_the_tray_line() {
        let def = ScheduleDefinition::recurring("s1", "x", "0 0 3 * * *", "UTC").unwrap();
        let tick = Tick {
            events: vec![TickEvent::NotRunnable {
                schedule_id: "s1".into(),
                detail: "missing configuration: .kgraf/run.json is absent".into(),
            }],
            build_failures: Default::default(),
            pending: None,
        };
        let mut warned = std::collections::BTreeSet::new();
        let board = ScheduleBoard::default();
        let a = attention(&tick, &mut warned, &[]);
        update_board(std::slice::from_ref(&def), &Default::default(), &board, a);
        let line = board.snapshot().line();
        assert!(line.contains("run.json is absent"), "{line}");
        assert_eq!(warned.len(), 1);
        let _ = attention(&tick, &mut warned, &[]);
        assert_eq!(warned.len(), 1, "the same reason is logged once");
        let cleared = Tick {
            events: vec![],
            build_failures: Default::default(),
            pending: None,
        };
        assert_eq!(attention(&cleared, &mut warned, &[]), None);
        assert!(warned.is_empty(), "a cleared reason is forgotten");
        let _ = attention(&tick, &mut warned, &[]);
        assert_eq!(warned.len(), 1, "its return is logged again");
        let waiting = Tick {
            events: vec![],
            build_failures: Default::default(),
            pending: Some(RUN_CONFIG_PENDING.into()),
        };
        let a = attention(&waiting, &mut warned, &[]);
        update_board(&[def], &Default::default(), &board, a);
        let line = board.snapshot().line();
        assert!(
            line.starts_with("1 schedule(s)") && line.ends_with(RUN_CONFIG_PENDING),
            "{line}"
        );
    }

    #[test]
    fn a_failure_persists_in_the_failing_set_until_a_completion() {
        let def = ScheduleDefinition::recurring("s1", "x", "0 0 3 * * *", "UTC").unwrap();
        let mut failing = std::collections::BTreeSet::new();
        let due = Utc::now();
        update_failing(
            &mut failing,
            std::slice::from_ref(&def),
            &std::collections::BTreeSet::new(),
            &[TickEvent::Failed {
                schedule_id: "s1".into(),
                due_at: due,
                detail: "x".into(),
                disabled: false,
            }],
        );
        assert!(failing.contains("s1"));
        // A tick with nothing to report must not clear it.
        update_failing(
            &mut failing,
            std::slice::from_ref(&def),
            &std::collections::BTreeSet::new(),
            &[],
        );
        assert!(failing.contains("s1"), "must persist until a completion");
        // Only a `Completed` clears it.
        update_failing(
            &mut failing,
            &[def],
            &std::collections::BTreeSet::new(),
            &[TickEvent::Completed {
                schedule_id: "s1".into(),
                due_at: due,
            }],
        );
        assert!(!failing.contains("s1"));
    }

    #[test]
    fn a_schedule_that_fails_to_build_shows_as_failing() {
        let def = ScheduleDefinition::recurring("s1", "x", "0 0 3 * * *", "UTC").unwrap();
        let mut failing = std::collections::BTreeSet::new();
        let mut build_failures = std::collections::BTreeSet::new();
        build_failures.insert("s1".to_owned());
        update_failing(&mut failing, &[def], &build_failures, &[]);
        assert!(failing.contains("s1"));
    }

    /// An in-process model, no network. The planner's decomposition gets a plan
    /// of one contract writing `made.txt`; its grounding check gets "serves"; a
    /// worker's first round adds the file with `apply_patch`, its second answers.
    #[derive(Default)]
    struct FakeModel {
        prompts: Mutex<Vec<&'static str>>,
        /// Set when a classification call's user text carries the literal
        /// `ghp_` prefix (fix, 2026-09-28's scrubbing test): proof the
        /// model read the recovered real text, not the scrubbed marker a
        /// persisted copy would carry.
        saw_secret: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl kgraf_provider::Provider for FakeModel {
        fn adapter(&self) -> &'static str {
            "fake"
        }
        fn protocol(&self) -> kgraf_core::Protocol {
            kgraf_core::Protocol::Additive
        }
        async fn complete(
            &self,
            request: &kgraf_provider::Request,
            token: &kgraf_provider::CallToken,
        ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
            use kgraf_provider::{Content, Role, StopReason};
            let system: String = request
                .system
                .iter()
                .filter_map(Content::as_text)
                .collect::<Vec<_>>()
                .join("\n");
            let user: String = request
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .flat_map(|m| m.content.iter())
                .filter_map(Content::as_text)
                .collect::<Vec<_>>()
                .join("\n");
            let answered = request.messages.iter().any(|m| m.role == Role::Assistant);
            let (kind, content, stop) = if system.contains("kGraf worker") {
                if answered {
                    (
                        "worker-done",
                        vec![Content::text("done")],
                        StopReason::EndTurn,
                    )
                } else {
                    let patch =
                        "*** Begin Patch\n*** Add File: made.txt\n+scheduled\n*** End Patch";
                    (
                        "worker-patch",
                        vec![Content::ToolUse {
                            id: "p1".into(),
                            name: "apply_patch".into(),
                            input: serde_json::json!({ "patch": patch }),
                            signature: None,
                        }],
                        StopReason::ToolUse,
                    )
                }
            } else if system.contains("Split the user's instruction") {
                let plan = serde_json::json!({"contracts": [{
                    "id": "c1",
                    "instruction": "Create made.txt containing the word scheduled.",
                    "acceptance": "made.txt exists",
                    "writes": [{"path": "made.txt"}],
                    "depends_on": [],
                    "tools": ["apply_patch"],
                    "attempts": 1
                }]});
                (
                    "decompose",
                    vec![Content::text(plan.to_string())],
                    StopReason::EndTurn,
                )
            } else if system.contains("still serves") {
                (
                    "ground",
                    vec![Content::text(r#"{"serves":true,"reason":"on track"}"#)],
                    StopReason::EndTurn,
                )
            } else if system.contains("routing an incoming question") {
                // No candidates in these tests' empty graph,
                // so the cheap match never fires; the one ledgered
                // classification call decides "offer" for a question naming
                // a whole separate project, "answer" otherwise.
                if user.contains("ghp_") {
                    *self.saw_secret.lock().unwrap_or_else(|e| e.into_inner()) = true;
                }
                let route = if user.contains("an entirely separate project") {
                    "offer"
                } else {
                    "answer"
                };
                (
                    "question-route",
                    vec![Content::text(format!(r#"{{"route":"{route}"}}"#))],
                    StopReason::EndTurn,
                )
            } else if system.contains("Answer it directly and concisely") {
                if user.contains("ghp_") {
                    *self.saw_secret.lock().unwrap_or_else(|e| e.into_inner()) = true;
                }
                (
                    "question-answer",
                    vec![Content::text("kGraf is a local desktop harness.")],
                    StopReason::EndTurn,
                )
            } else {
                (
                    "other",
                    vec![Content::text("unexpected")],
                    StopReason::EndTurn,
                )
            };
            self.prompts
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(kind);
            let usage = kgraf_core::TokenUsage {
                uncached_input: 100,
                text_output: 10,
                ..Default::default()
            };
            token.settle(&usage, None)?;
            Ok(kgraf_provider::Response {
                finish_reason: None,
                model: request.model.clone(),
                content,
                stop_reason: stop,
                usage,
                request_id: None,
            })
        }
        fn capabilities(&self) -> kgraf_provider::capability::Capabilities<'_> {
            kgraf_provider::capability::Capabilities::none()
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Sets a repo-local commit identity (fix 3: these tests must never
    /// depend on the running user's global `~/.gitconfig`). `-c user.name=`
    /// on the `commit` call alone is not enough: it covers that one
    /// invocation, but `worker::workspace::owner_identity` later reads
    /// `git config --get user.name` back out of this same repository (to
    /// stamp merge-back commits with the user's identity), which sees only
    /// what is actually persisted to `.git/config`. Writing it there, once,
    /// right after `init`, is what makes these tests pass in an environment
    /// (a bare `HOME`, a CI sandbox) that has no global git identity at all.
    fn set_repo_identity(dir: &Path) {
        git(dir, &["config", "user.name", "t"]);
        git(dir, &["config", "user.email", "t@t"]);
    }

    /// A due schedule starts a real run through `ProjectFactory` and
    /// `kgraf-host`: the claim, the host's construction (git worktree,
    /// route probe, verifier, merge-back), the planner and worker calls through
    /// the ledger, the acceptance command, and the merge into the run's
    /// integration branch. Only the model is fake (in-process, no network).
    /// A git repository with a price table and a host whose only model is
    /// [`FakeModel`]: the real run construction, no network.
    fn hosted_repo() -> (
        tempfile::TempDir,
        PathBuf,
        Arc<ProjectFactory>,
        Arc<FakeModel>,
    ) {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        git(&root, &["init", "-q", "-b", "main"]);
        set_repo_identity(&root);
        std::fs::write(root.join("README.md"), "# t\n").expect("write");
        std::fs::write(root.join(".gitignore"), ".kgraf/\n").expect("write");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let prices = dir.path().join("prices.json");
        std::fs::write(
            &prices,
            r#"{"m": {"uncached_input": 1000000, "cache_read": 1000000, "cache_write": 1000000, "output": 1000000, "reasoning": 1000000}}"#,
        )
        .expect("prices");
        let mut config = RunConfig::new(&root, vec!["fake:m".into()], &prices);
        config.verify = "test -f made.txt".into();
        let model = Arc::new(FakeModel::default());
        let host = Host::open(config)
            .expect("open")
            .with_route_provider("fake", model.clone())
            .expect("in-process route");
        let factory = Arc::new(ProjectFactory::with_host(Arc::new(host)));

        (dir, root, factory, model)
    }

    /// With the real event the orchestrator sends: with one run
    /// slot, the second of two due schedules is claimed and deferred, then starts
    /// once the first ends. Its node records the deferral once, then a start (not a
    /// second deferral), shows in flight while it runs, then records its outcome.
    #[test]
    fn a_deferred_occurrence_that_starts_is_a_start_on_its_node() {
        let (_dir, root, factory, _model) = hosted_repo();
        let graph =
            kgraf_graph::GraphStore::open(&root.join(".kgraf/graph.db"), &root).expect("graph");
        // `Utc::now()` has sub-millisecond digits; schedule instants are cut to
        // whole milliseconds, so the deferral is kept, not given up.
        let now = Utc::now();
        let defs: Vec<ScheduleDefinition> = ["first", "second"]
            .iter()
            .map(|id| {
                let mut def = ScheduleDefinition::one_shot(
                    *id,
                    "Create made.txt",
                    now - TimeDelta::seconds(5),
                    "UTC",
                )
                .expect("valid");
                def.created_at = now - TimeDelta::seconds(10);
                kgraf_schedule::node_store::add(
                    &graph,
                    &def,
                    kgraf_schedule::NodeChoice::FromInstruction,
                    now,
                )
                .expect("add");
                def
            })
            .collect();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let mut recorder = schedule_store::HistoryRecorder::default();
        let mut in_flight_seen = false;
        let events = runtime.block_on(async {
            let mut state = None;
            let (active_loop, starter) =
                ensure_state(&mut state, &root.join(".kgraf"), "test", &factory).expect("state");
            let mut seen = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(180);
            while std::time::Instant::now() < deadline {
                let events = tick_once(active_loop, starter, &factory, &defs, Utc::now())
                    .await
                    .events;
                let spend = |r: &str| factory.run_spend(r);
                let report = recorder.record_tick(
                    &graph,
                    schedule_store::records_of(&events, &spend, Utc::now()),
                    None,
                    &spend,
                );
                assert!(report.errors.is_empty(), "{report:?}");
                if events.iter().any(|e| {
                    matches!(e, TickEvent::StartedAfterDeferral { schedule_id, .. }
                        if schedule_id == "second")
                }) {
                    let views = schedule_store::views(&graph).expect("views");
                    let nodes = schedule_store::plan_nodes(&views, &|_: &str| None);
                    in_flight_seen = nodes.iter().any(|n| {
                        n.key == "work:schedules/second"
                            && n.state == kgraf_galaxy::WorkState::InFlight
                    });
                }
                let second_ended = events.iter().any(|e| {
                    matches!(e, TickEvent::Completed { schedule_id, .. }
                        | TickEvent::Failed { schedule_id, .. } if schedule_id == "second")
                });
                seen.extend(events);
                if second_ended {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            seen
        });
        assert!(
            events.iter().any(|e| matches!(e,
                TickEvent::Deferred { schedule_id, deferred_count: 1, .. } if schedule_id == "second")),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e,
                TickEvent::StartedAfterDeferral { schedule_id, .. } if schedule_id == "second")),
            "the orchestrator reports the start as its own event: {events:?}"
        );
        assert!(in_flight_seen, "the node showed the run in flight");
        let kinds: Vec<(String, String)> = graph
            .schedule_events("second", 50)
            .expect("history")
            .into_iter()
            .rev()
            .map(|e| (e.event, e.detail))
            .collect();
        let names: Vec<&str> = kinds.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            ["added", "deferred", "started", "completed"],
            "{kinds:?}\n{}",
            failed_checks(&root)
        );
        assert!(
            kinds[2].1.starts_with("claimed and started after ")
                && kinds[2].1.ends_with("deferral(s)"),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_due_schedule_runs_to_completion_through_the_host() {
        let (_dir, root, factory, model) = hosted_repo();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let events = runtime.block_on(async {
            let mut state = None;
            let (active_loop, starter) =
                ensure_state(&mut state, &root.join(".kgraf"), "test", &factory).expect("state");
            let now = Utc::now();
            let mut def = ScheduleDefinition::one_shot(
                "nightly",
                "Create made.txt",
                now - TimeDelta::seconds(5),
                "UTC",
            )
            .expect("valid");
            def.created_at = now - TimeDelta::seconds(10);
            let mut seen = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(90);
            while std::time::Instant::now() < deadline {
                let events = tick_once(
                    active_loop,
                    starter,
                    &factory,
                    std::slice::from_ref(&def),
                    Utc::now(),
                )
                .await
                .events;
                let ended = events
                    .iter()
                    .any(|e| matches!(e, TickEvent::Completed { .. } | TickEvent::Failed { .. }));
                seen.extend(events);
                if ended {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            seen
        });
        assert!(
            events.iter().any(
                |e| matches!(e, TickEvent::Started { schedule_id, .. } if schedule_id == "nightly")
            ),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TickEvent::Completed { .. })),
            "the run completed: {events:?}\n{}",
            failed_checks(&root)
        );
        let kinds = model.prompts.lock().expect("lock").clone();
        for kind in ["decompose", "worker-patch", "worker-done"] {
            assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
        }
        assert!(!kinds.contains(&"other"), "{kinds:?}");
        // The work is on the run's integration branch; the checkout is untouched.
        let branch = git(
            &root,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/kgraf",
            ],
        );
        assert!(branch.starts_with("kgraf/sched-nightly-"), "{branch}");
        assert_eq!(
            git(&root, &["show", &format!("{branch}:made.txt")]),
            "scheduled"
        );
        assert!(!root.join("made.txt").exists());
        // Every call went through the host's ledger, attributed to the run.
        let host = factory.current().expect("host").host().clone();
        let calls = host.ledger().calls().expect("calls");
        assert!(calls.len() >= 3, "{}", calls.len());
        let run = branch.trim_start_matches("kgraf/");
        assert!(
            calls
                .iter()
                .any(|c| c.agent_id == format!("orchestrator:{run}")),
            "the planner's calls are the run's"
        );
        assert!(host.ledger().unreconciled().expect("ledger").is_empty());
        // The run's history lands on the schedule's work node, with its spend
        // read from this host's ledger by the run id, priced from its table.
        let graph =
            kgraf_graph::GraphStore::open(&root.join(".kgraf/graph.db"), &root).expect("graph");
        let def = ScheduleDefinition::recurring("nightly", "Create made.txt", "0 0 3 * * *", "UTC")
            .expect("valid");
        kgraf_schedule::node_store::add(
            &graph,
            &def,
            kgraf_schedule::NodeChoice::FromInstruction,
            Utc::now(),
        )
        .expect("add");
        let spend = |r: &str| factory.run_spend(r);
        let report = schedule_store::HistoryRecorder::default().record_tick(
            &graph,
            schedule_store::records_of(&events, &spend, Utc::now()),
            None,
            &spend,
        );
        assert!(
            report.errors.is_empty() && report.dropped.is_empty(),
            "{report:?}"
        );
        let done = graph
            .schedule_events("nightly", 50)
            .expect("history")
            .into_iter()
            .find(|e| e.event == "completed")
            .expect("a completed row");
        assert_eq!(done.node_id, "schedules/nightly");
        assert_eq!(done.run_id.as_deref(), Some(run));
        let spend = done.spend.expect("spend recorded");
        assert_eq!(spend.calls, calls.len() as i64, "{spend:?}");
        assert_eq!(spend.unpriced, 0, "{spend:?}");
        assert!(spend.micros > 0, "{spend:?}");
        assert!(
            spend.text.starts_with('$')
                && spend.text.contains(&format!("({} call(s))", calls.len())),
            "{spend:?}"
        );
        // The tick that reported the end did the run's housekeeping.
        assert!(
            host.finish_run(&RunId::new(run)).is_none(),
            "already finished"
        );
    }

    /// With no settings file, the factory is not configured and says which file
    /// is missing; nothing is claimed (the loop checks `configured()` first).
    #[test]
    fn without_run_json_the_project_is_not_configured_and_says_where() {
        let dir = tempfile::tempdir().expect("dir");
        let factory = ProjectFactory::for_project(dir.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let err = runtime.block_on(factory.configured()).expect_err("no file");
        assert!(
            matches!(&err, ReadinessFailure::MissingConfiguration(m) if m.contains("run.json")),
            "{err}"
        );
        assert!(
            matches!(
                factory.build(&RunId::new("x")),
                Err(OrchestratorError::Build { .. })
            ),
            "no host, no build"
        );
        std::fs::create_dir_all(dir.path().join(".kgraf")).expect("mkdir");
        std::fs::write(
            dir.path().join(".kgraf").join(RUN_CONFIG_FILE),
            r#"{"routes": [], "prices": "p.json", "cap_run": 1}"#,
        )
        .expect("write");
        let err = runtime
            .block_on(factory.configured())
            .expect_err("no routes");
        assert!(
            matches!(&err, ReadinessFailure::MissingConfiguration(m) if m.contains("no route")),
            "{err}"
        );
    }

    /// A git repository with one commit and `.kgraf/run.json` on the local
    /// `ollama` route (no credential; nothing is called by a build), with its
    /// price table beside it.
    fn configured_project(cap_run: f64) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        set_repo_identity(root);
        std::fs::write(root.join("README.md"), "# t\n").expect("write");
        std::fs::write(root.join(".gitignore"), ".kgraf/\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "init"]);
        let state = root.join(".kgraf");
        std::fs::create_dir_all(&state).expect("state");
        std::fs::write(
            state.join("prices.json"),
            r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
        )
        .expect("prices");
        write_run_json(root, cap_run, 0);
        dir
    }

    /// `run.json` with `cap_run`, its modification time moved `later` seconds
    /// past the previous one (so a change is seen whatever the clock resolution).
    fn write_run_json(root: &Path, cap_run: f64, later: u64) {
        let path = root.join(".kgraf").join(RUN_CONFIG_FILE);
        let before = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        std::fs::write(
            &path,
            format!(
                r#"{{"routes": ["ollama:llama3"], "prices": "prices.json", "cap_run": {cap_run}}}"#
            ),
        )
        .expect("run.json");
        if let Some(before) = before {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(before + Duration::from_secs(later)))
                .expect("mtime");
        }
    }

    /// A listener this test owns, and the `ollama` base URL that reaches it. A
    /// build dials each local route first; the listener never answers, and a
    /// connection is all the dial needs. Keep the listener alive while the
    /// factory builds.
    fn stub_ollama() -> (std::net::TcpListener, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("stub listener");
        let url = format!("http://{}/v1", listener.local_addr().expect("stub address"));
        (listener, url)
    }

    fn multi_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// The path the shell uses: `run.json` loaded by `ProjectFactory`, the host
    /// opened from it, readiness passed, and a build that succeeds.
    #[test]
    fn a_valid_run_json_builds_through_the_project_factory() {
        let dir = configured_project(1.0);
        let (_stub, stub_url) = stub_ollama();
        let factory =
            ProjectFactory::for_project(dir.path()).with_local_endpoint("ollama", &stub_url);
        multi_thread().block_on(async {
            factory.configured().await.expect("configured");
            factory.ready("nightly").await.expect("ready");
            let built = factory.build(&RunId::new("from-file"));
            assert!(built.is_ok(), "{:?}", built.err());
        });
        let branch = git(
            dir.path(),
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/kgraf",
            ],
        );
        assert_eq!(branch, "kgraf/from-file", "the build made the run's branch");
    }

    /// Decision 0088: in the app, the user's setting is the one source of the
    /// worktree policy. It reaches the host the factory opens and the host
    /// already open; a factory given no setting leaves the default; and a
    /// `run.json` that tries to set it is refused, saying where the choice is
    /// made, never silently overridden.
    #[test]
    fn the_users_worktree_setting_is_the_apps_one_source_of_the_policy() {
        use kgraf_host::WorktreeRetention::{Delete, Keep};
        let dir = configured_project(1.0);
        let rt = multi_thread();

        let plain = ProjectFactory::for_project(dir.path());
        rt.block_on(plain.configured()).expect("configured");
        assert_eq!(plain.host().expect("host").worktree_retention(), Delete);

        let keeping = ProjectFactory::for_project(dir.path()).with_worktree_retention(Keep);
        rt.block_on(keeping.configured()).expect("configured");
        let host = keeping.host().expect("host");
        assert_eq!(host.worktree_retention(), Keep, "the setting reaches it");
        keeping.set_worktree_retention(Delete);
        assert_eq!(host.worktree_retention(), Delete, "and a change reaches it");

        let path = dir.path().join(".kgraf").join(RUN_CONFIG_FILE);
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        json["worktrees"] = "keep".into();
        std::fs::write(&path, json.to_string()).expect("write");
        let refused = ProjectFactory::for_project(dir.path()).with_worktree_retention(Delete);
        let err = rt
            .block_on(refused.configured())
            .expect_err("the key is refused");
        assert!(
            matches!(&err, ReadinessFailure::MissingConfiguration(m)
                if m.contains("worktrees") && m.contains("Delete worker worktrees after use")),
            "{err}"
        );
    }

    /// The user's run limits reach the host the factory opens for every limit
    /// `run.json` leaves out, and a change reopens the host once no run is active
    /// (the mechanism: the opened host's own configuration changes).
    #[test]
    fn the_users_run_limits_reach_the_host_and_a_change_reopens_it() {
        use kgraf_host::limits::{SharedLimits, UserLimits};
        let dir = configured_project(1.0);
        let shared = SharedLimits::new(UserLimits {
            rounds: Some(61),
            max_checkpoints: Some(4),
            planner_max_tokens: Some(40_000),
            ..UserLimits::default()
        });
        let factory = ProjectFactory::for_project(dir.path()).with_run_limits(shared.clone());
        let rt = multi_thread();
        rt.block_on(factory.configured()).expect("configured");
        let cfg = |f: &ProjectFactory| {
            let h = f.host().expect("open");
            (
                h.config().steps,
                h.config().planner_max_tokens,
                h.config().max_tokens,
                h.config().max_checkpoints,
            )
        };
        assert_eq!(
            cfg(&factory),
            (61, 40_000, 16_384, 4),
            "run.json names none of them"
        );
        assert_eq!(factory.refresh(0), None, "unchanged: nothing to do");

        shared.set(UserLimits {
            rounds: Some(62),
            max_checkpoints: Some(5),
            ..UserLimits::default()
        });
        assert_eq!(factory.refresh(1).as_deref(), Some(RUN_LIMITS_PENDING));
        assert_eq!(cfg(&factory).0, 61, "an active run keeps its host");
        assert_eq!(factory.refresh(0), None);
        rt.block_on(factory.configured()).expect("reopened");
        assert_eq!(
            cfg(&factory),
            (62, 32_768, 16_384, 5),
            "the new limits apply"
        );
    }

    /// The warning setting reaches the project's host: off without it, on with
    /// it, and a change reopens the host like a limit does.
    #[test]
    fn the_rule_conflict_setting_reaches_the_host() {
        use kgraf_host::limits::{SharedLimits, UserLimits};
        let dir = configured_project(1.0);
        let shared = SharedLimits::new(UserLimits::default());
        let factory = ProjectFactory::for_project(dir.path()).with_run_limits(shared.clone());
        let rt = multi_thread();
        rt.block_on(factory.configured()).expect("configured");
        assert!(!factory.host().expect("open").config().warn_rule_conflicts);
        shared.set(UserLimits {
            warn_rule_conflicts: Some(true),
            ..UserLimits::default()
        });
        assert_eq!(
            factory.refresh(0),
            None,
            "no run: reopened at once or lazily"
        );
        rt.block_on(factory.configured()).expect("reopened");
        assert!(factory.host().expect("open").config().warn_rule_conflicts);
    }

    /// An MCP server in `worker.json` that does not answer fails the build
    /// as transient; through `ScheduledRuns` that counts toward the pause
    /// threshold and does not disable the schedule.
    #[test]
    fn an_unreachable_mcp_server_is_transient_not_disabling() {
        let dir = configured_project(1.0);
        std::fs::write(
            dir.path().join(".kgraf").join("worker.json"),
            r#"{"mcp_servers": [{"name": "gone", "transport": {"type": "streamable_http", "url": "http://127.0.0.1:1/mcp"}}]}"#,
        )
        .expect("worker.json");
        let (_stub, stub_url) = stub_ollama();
        let factory = Arc::new(
            ProjectFactory::for_project(dir.path()).with_local_endpoint("ollama", &stub_url),
        );
        let mut instructions = BTreeMap::new();
        instructions.insert("nightly".to_owned(), "do it".to_owned());
        let starter = ScheduledRuns::new(factory.clone(), instructions, 1);
        let out = multi_thread().block_on(async {
            factory.configured().await.expect("configured");
            kgraf_schedule::RunStarter::start_run(&starter, "nightly", Utc::now()).await
        });
        assert!(
            matches!(
                &out,
                Ok(kgraf_schedule::RunStartOutcome::Failed(
                    kgraf_schedule::FailureClass::Transient { detail }
                )) if detail.contains("gone")
            ),
            "{out:?}"
        );
        assert_eq!(starter.active(), 0);
    }

    /// A changed `run.json` reopens the host once no scheduled run is
    /// active; while one is, the old host stays and the change is said to wait.
    #[test]
    fn a_changed_run_json_reopens_the_host_when_no_run_is_active() {
        let dir = configured_project(1.0);
        let factory = ProjectFactory::for_project(dir.path());
        let rt = multi_thread();
        rt.block_on(factory.configured()).expect("configured");
        let cap = |f: &ProjectFactory| f.current().expect("open").host().config().cap_run;
        assert_eq!(cap(&factory), Some(1.0));
        assert_eq!(factory.refresh(0), None, "unchanged: nothing to do");
        write_run_json(dir.path(), 2.0, 5);
        assert_eq!(factory.refresh(1).as_deref(), Some(RUN_CONFIG_PENDING));
        assert_eq!(cap(&factory), Some(1.0), "the active run's host stays");
        assert_eq!(factory.refresh(0), None);
        assert!(factory.current().is_none(), "retired");
        rt.block_on(factory.configured()).expect("reopened");
        assert_eq!(cap(&factory), Some(2.0), "the new settings apply");
        // The same bytes saved again (a later mtime) change nothing.
        let open = factory.current().expect("open");
        write_run_json(dir.path(), 2.0, 10);
        assert_eq!(factory.refresh(0), None);
        assert!(
            Arc::ptr_eq(&open, &factory.current().expect("still open")),
            "content unchanged: the host (and its Admission) stays"
        );
    }

    /// A box run in flight holds a `run.json` reload back exactly as a
    /// scheduled run does (the tray says it applies after the active run ends),
    /// so no second host, and no second `Admission`, appears under it.
    #[test]
    fn a_box_run_in_flight_blocks_the_reload() {
        use crate::orchestrator_host::RunHousekeeping;
        let dir = configured_project(1.0);
        let factory = ProjectFactory::for_project(dir.path());
        let rt = multi_thread();
        rt.block_on(factory.configured()).expect("configured");
        let admission = factory.admission().expect("open");
        let run = RunId::new("box-1");
        RunHousekeeping::run_started(&factory, &run);
        write_run_json(dir.path(), 2.0, 5);
        assert_eq!(
            factory.refresh(0).as_deref(),
            Some(RUN_CONFIG_PENDING),
            "no scheduled run, but the box run holds the reload"
        );
        assert!(
            Arc::ptr_eq(&factory.admission().expect("open"), &admission),
            "the box run's host and Admission stay"
        );
        RunHousekeeping::finish_run(&factory, &run);
        assert_eq!(factory.refresh(0), None);
        assert!(
            factory.current().is_none(),
            "retired once the box run ended"
        );
    }

    /// A new host keeps the hourly cap's history. The ledger's spend of the
    /// last hour is seeded into each new host's admission, on first open and on
    /// a reopen after `run.json` changes, so reopening never resets the cap.
    #[test]
    fn a_reopened_host_keeps_the_last_hours_spend() {
        let dir = configured_project(1.0);
        let state = dir.path().join(".kgraf");
        let usage = kgraf_core::TokenUsage {
            uncached_input: 5_000_000,
            text_output: 1_000_000,
            ..Default::default()
        };
        {
            let ledger = kgraf_provider::Ledger::open(&state.join("ledger.db")).expect("ledger");
            let token = ledger
                .begin(
                    "fake",
                    kgraf_core::Protocol::Additive,
                    "llama3",
                    "orchestrator:earlier",
                    "earlier",
                )
                .expect("begin");
            token.settle(&usage, None).expect("settle");
        }
        let factory = ProjectFactory::for_project(dir.path());
        let rt = multi_thread();
        let hour = |f: &ProjectFactory| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_millis() as i64;
            f.current()
                .expect("open")
                .host()
                .admission()
                .budget()
                .spent_last_hour(now)
        };
        rt.block_on(factory.configured()).expect("configured");
        let expected = factory
            .current()
            .expect("open")
            .host()
            .prices()
            .expect("prices")
            .cost("llama3", &usage)
            .expect("priced");
        assert!(expected > kgraf_core::Money::ZERO);
        assert_eq!(hour(&factory), expected, "seeded on first open");
        write_run_json(dir.path(), 3.0, 5);
        assert_eq!(factory.refresh(0), None);
        rt.block_on(factory.configured()).expect("reopened");
        assert_eq!(hour(&factory), expected, "not reset by the reopen");
    }

    /// The board wakes the UI when its line changes and only then, so a
    /// visible idle window with a tray draws no timer frames.
    #[test]
    fn the_board_wakes_the_ui_only_when_it_changes() {
        use std::sync::atomic::AtomicUsize;
        let board = ScheduleBoard::default();
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        board.set_wake(Arc::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        }));
        let summary = ScheduleSummary {
            total: 2,
            ..ScheduleSummary::default()
        };
        board.set(summary.clone());
        assert_eq!(woken.load(Ordering::SeqCst), 1);
        for _ in 0..5 {
            board.set(summary.clone());
        }
        assert_eq!(
            woken.load(Ordering::SeqCst),
            1,
            "an unchanged board wakes nobody"
        );
        board.set_error("broken");
        assert_eq!(woken.load(Ordering::SeqCst), 2);
    }

    /// A git repository with one commit, and a price table beside it.
    fn repo_with_prices() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        git(&root, &["init", "-q", "-b", "main"]);
        set_repo_identity(&root);
        std::fs::write(root.join("README.md"), "# t\n").expect("write");
        std::fs::write(root.join(".gitignore"), ".kgraf/\n").expect("write");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let prices = dir.path().join("prices.json");
        std::fs::write(
            &prices,
            r#"{"m": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
        )
        .expect("prices");
        (dir, root, prices)
    }

    /// Drives the box's backend until `done`.
    fn drive(
        host: &mut crate::orchestrator_host::OrchestratorHost,
        model: &mut crate::orchestrator_box::BoxModel,
        what: &str,
        done: impl Fn(&crate::orchestrator_host::OrchestratorHost) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while !done(host) {
            assert!(
                std::time::Instant::now() < deadline,
                "never: {what}: {:?}",
                model.lines()
            );
            model.absorb(host);
            std::thread::sleep(Duration::from_millis(20));
        }
        model.absorb(host);
    }

    /// Item 6: the box adapter on kgraf-host. `/run <instruction>` in the box
    /// starts a real run through the host (an in-process model behind the route's
    /// limiter and the host's ledger), the host's housekeeping runs when it ends,
    /// and the box's desk is the host's ask resolver.
    #[test]
    fn a_run_typed_in_the_box_runs_through_the_host_and_is_housekept() {
        use crate::orchestrator_box::{BackendState, BoxBackend, BoxModel, Line};
        use crate::orchestrator_host::{HostParts, OrchestratorHost};
        let (_dir, root, prices) = repo_with_prices();
        let mut config = RunConfig::new(&root, vec!["fake:m".into()], &prices);
        config.verify = "test -f made.txt".into();
        let model = Arc::new(FakeModel::default());
        let desk = Arc::new(kgraf_orchestrator::AskDesk::new(None, Arc::new(|| {})));
        let host = Host::open(config)
            .expect("open")
            .with_route_provider("fake", model.clone())
            .expect("in-process route")
            .with_ask_resolver(desk.clone());
        let host = Arc::new(host);
        let factory = Arc::new(ProjectFactory::with_host(host.clone()));
        let parts = HostParts {
            factory: factory.clone(),
            housekeeping: factory.clone(),
            desk,
            router: None,
        };
        let mut backend = OrchestratorHost::new(Some(parts), Arc::new(|| {})).expect("backend");
        assert!(
            Arc::ptr_eq(&backend.admission().expect("open"), host.admission()),
            "the box binds the host's one admission"
        );
        let mut model_box = BoxModel::default();
        model_box.input = "Create made.txt".into();
        model_box.submit(&mut backend, None);
        assert!(
            matches!(backend.state(), BackendState::Idle { .. }),
            "a plain line starts nothing"
        );
        model_box.input = "/run Create made.txt".into();
        model_box.submit(&mut backend, None);
        drive(&mut backend, &mut model_box, "the run starts", |h| {
            h.running().is_some()
        });
        let run = backend.running().expect("running").run().clone();
        drive(&mut backend, &mut model_box, "the run ends", |h| {
            h.running().is_none()
        });
        let lines = model_box.lines();
        assert!(
            lines
                .iter()
                .any(|l| matches!(l, Line::Notice(n) if n.contains("ended completed"))),
            "{lines:?}\n{}",
            failed_checks(&root)
        );
        assert!(
            lines
                .iter()
                .any(|l| matches!(l, Line::Notice(n) if n.contains("swept"))),
            "the host's housekeeping ran and was shown: {lines:?}"
        );
        assert!(
            host.finish_run(&run).is_none(),
            "finish_run was already called for the run"
        );
        let kinds = model.prompts.lock().expect("lock").clone();
        assert!(kinds.contains(&"worker-done"), "{kinds:?}");
    }

    /// A plain line typed with no run in this
    /// window is routed, end to end, through a real `Host` and ledger. No
    /// roadmap candidate exists in this project's empty graph, so the cheap
    /// match finds nothing and the one ledgered classification call decides
    /// "answer"; a second ledgered call is the orchestrator's own answer,
    /// shown in the box. Nothing is dispatched (route 2 never starts a run).
    #[test]
    fn a_plain_line_with_no_run_is_routed_and_the_orchestrator_answers_it() {
        use crate::orchestrator_box::{BackendState, BoxBackend, BoxModel, Line};
        use crate::orchestrator_host::{HostParts, OrchestratorHost};
        let (_dir, root, prices) = repo_with_prices();
        let config = RunConfig::new(&root, vec!["fake:m".into()], &prices);
        let model = Arc::new(FakeModel::default());
        let desk = Arc::new(kgraf_orchestrator::AskDesk::new(None, Arc::new(|| {})));
        let host = Host::open(config)
            .expect("open")
            .with_route_provider("fake", model.clone())
            .expect("in-process route")
            .with_ask_resolver(desk.clone());
        let host = Arc::new(host);
        let factory = Arc::new(ProjectFactory::with_host(host.clone()));
        let parts = HostParts {
            factory: factory.clone(),
            housekeeping: factory.clone(),
            desk,
            router: None,
        };
        let mut backend = OrchestratorHost::new(Some(parts), Arc::new(|| {})).expect("backend");
        let mut model_box = BoxModel::default();
        model_box.input = "what does kGraf mean by a lease".into();
        model_box.submit(&mut backend, None);
        assert!(
            matches!(backend.state(), BackendState::Idle { .. }),
            "routing never starts a run for route 2"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while !model_box
            .lines()
            .iter()
            .any(|l| matches!(l, Line::Orchestrator(_)))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "never answered: {:?}",
                model_box.lines()
            );
            model_box.absorb(&mut backend);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            model_box
                .lines()
                .iter()
                .any(|l| matches!(l, Line::Orchestrator(t) if t.contains("local desktop harness")))
        );
        assert!(matches!(backend.state(), BackendState::Idle { .. }));
        let kinds = model.prompts.lock().expect("lock").clone();
        assert!(kinds.contains(&"question-route"), "{kinds:?}");
        assert!(kinds.contains(&"question-answer"), "{kinds:?}");

        // route 2 must be recorded on the graph, as a
        // closed item with no agent dispatched. Mutation check: a body that
        // said `"dispatched":true` or omitted the method would be wrong; both
        // are asserted against, not just presence of the row.
        let items = host
            .with_graph(|g| g.work_nodes(kgraf_orchestrator::QUESTIONS_PLAN_ID))
            .expect("graph reachable")
            .expect("nodes");
        let recorded = items
            .iter()
            .find(|n| n.body.contains("answered_directly"))
            .unwrap_or_else(|| panic!("no answered item recorded: {items:?}"));
        assert!(
            recorded.body.contains("local desktop harness"),
            "{}",
            recorded.body
        );
        assert!(
            recorded.body.contains("\"dispatched\":false"),
            "{}",
            recorded.body
        );
        assert!(recorded.body.contains("\"method\""), "{}", recorded.body);
    }

    /// A secret-shaped token in a
    /// routed question must never reach the box or the graph raw, while the
    /// model that classifies and answers still reads the real value
    /// (`model_copy`). Mutation check: if the scrub were dropped, `shown`
    /// and `recorded.body` would contain the token and this fails; if the
    /// model were made to read the scrubbed copy instead of the recovered
    /// one, `FakeModel::saw_secret` never flips and this also fails.
    #[test]
    fn a_secret_in_the_question_is_scrubbed_but_the_model_still_reads_it() {
        use crate::orchestrator_box::{BoxModel, Line};
        use crate::orchestrator_host::{HostParts, OrchestratorHost};
        let (_dir, root, prices) = repo_with_prices();
        let config = RunConfig::new(&root, vec!["fake:m".into()], &prices);
        let model = Arc::new(FakeModel::default());
        let desk = Arc::new(kgraf_orchestrator::AskDesk::new(None, Arc::new(|| {})));
        let host = Host::open(config)
            .expect("open")
            .with_route_provider("fake", model.clone())
            .expect("in-process route")
            .with_ask_resolver(desk.clone());
        let host = Arc::new(host);
        let factory = Arc::new(ProjectFactory::with_host(host.clone()));
        let parts = HostParts {
            factory: factory.clone(),
            housekeeping: factory.clone(),
            desk,
            router: None,
        };
        let mut backend = OrchestratorHost::new(Some(parts), Arc::new(|| {})).expect("backend");
        let token = format!("ghp_{}", "a".repeat(36));
        let mut model_box = BoxModel::default();
        model_box.input = format!("does kGraf ever log something like {token}");
        model_box.submit(&mut backend, None);
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while !model_box
            .lines()
            .iter()
            .any(|l| matches!(l, Line::Orchestrator(_)))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "never answered: {:?}",
                model_box.lines()
            );
            model_box.absorb(&mut backend);
            std::thread::sleep(Duration::from_millis(20));
        }
        let shown = model_box
            .lines()
            .iter()
            .find_map(|l| match l {
                Line::Orchestrator(t) => Some(t.clone()),
                _ => None,
            })
            .expect("answered");
        assert!(
            !shown.contains(&token),
            "the box must never show the raw token: {shown}"
        );
        let items = host
            .with_graph(|g| g.work_nodes(kgraf_orchestrator::QUESTIONS_PLAN_ID))
            .expect("graph")
            .expect("nodes");
        let recorded = items
            .iter()
            .find(|n| n.body.contains("answered_directly"))
            .expect("recorded");
        assert!(
            !recorded.body.contains(&token),
            "the graph must never store the raw token: {}",
            recorded.body
        );
        assert!(
            *model.saw_secret.lock().expect("lock"),
            "the model must still read the recovered real text"
        );
    }

    /// the offer (step 3) must be recorded on the
    /// origin project's graph when made, and its resolution when the user
    /// answers it. This exercises the decline path end to end through a real
    /// `Host`; the accept path's resolution (naming the new project) is
    /// `kgraf_orchestrator::question_route`'s own test
    /// (`route_3_a_very_large_off_topic_question_is_only_offered_not_dispatched`),
    /// since accepting opens a *different* project the origin `Host` here
    /// cannot see. Mutation check: an offer recorded with no `graph_id` (the
    /// write having failed) would make `decline_question_offer` skip the
    /// resolution write silently; asserting the resolved body's content, not
    /// just that declining succeeds, catches that.
    #[test]
    fn an_offer_is_recorded_when_made_and_again_when_declined() {
        use crate::orchestrator_box::{BackendState, BoxBackend, BoxModel};
        use crate::orchestrator_host::{HostParts, OrchestratorHost};
        let (_dir, root, prices) = repo_with_prices();
        let config = RunConfig::new(&root, vec!["fake:m".into()], &prices);
        let model = Arc::new(FakeModel::default());
        let desk = Arc::new(kgraf_orchestrator::AskDesk::new(None, Arc::new(|| {})));
        let host = Host::open(config)
            .expect("open")
            .with_route_provider("fake", model.clone())
            .expect("in-process route")
            .with_ask_resolver(desk.clone());
        let host = Arc::new(host);
        let factory = Arc::new(ProjectFactory::with_host(host.clone()));
        let parts = HostParts {
            factory: factory.clone(),
            housekeeping: factory.clone(),
            desk,
            router: None,
        };
        let mut backend = OrchestratorHost::new(Some(parts), Arc::new(|| {})).expect("backend");
        let mut model_box = BoxModel::default();
        model_box.input = "build me an entirely separate project for payroll".into();
        model_box.submit(&mut backend, None);
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while backend.pending_offers().is_empty() {
            assert!(std::time::Instant::now() < deadline, "never offered");
            model_box.absorb(&mut backend);
            std::thread::sleep(Duration::from_millis(20));
        }
        let offer = backend.pending_offers().remove(0);
        let graph_id = offer
            .graph_id
            .clone()
            .expect("the offer's own write succeeded");
        let items = host
            .with_graph(|g| g.work_nodes(kgraf_orchestrator::QUESTIONS_PLAN_ID))
            .expect("graph")
            .expect("nodes");
        let made = items
            .iter()
            .find(|n| n.id == graph_id)
            .expect("recorded when made");
        assert!(made.body.contains("offer_pending"), "{}", made.body);

        backend
            .decline_question_offer(offer.id)
            .expect("declines and dispatches under Questions");
        assert!(matches!(backend.state(), BackendState::Starting));
        let items = host
            .with_graph(|g| g.work_nodes(kgraf_orchestrator::QUESTIONS_PLAN_ID))
            .expect("graph")
            .expect("nodes");
        let resolved = items
            .iter()
            .find(|n| n.id == graph_id)
            .expect("still there");
        assert!(resolved.body.contains("declined"), "{}", resolved.body);
        assert_eq!(
            items.iter().filter(|n| n.id == graph_id).count(),
            1,
            "resolution updates the same item, never a new one"
        );
    }

    /// Item 6: with no valid `.kgraf/run.json`, `/run` says exactly what is
    /// missing, and starts nothing.
    #[test]
    fn without_run_json_the_box_says_exactly_what_is_missing() {
        use crate::orchestrator_box::{BoxModel, Line};
        use crate::orchestrator_host::{HostParts, OrchestratorHost};
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
        let desk = Arc::new(kgraf_orchestrator::AskDesk::new(None, Arc::new(|| {})));
        let factory =
            Arc::new(ProjectFactory::for_project(dir.path()).with_ask_resolver(desk.clone()));
        let parts = HostParts {
            factory: factory.clone(),
            housekeeping: factory.clone(),
            desk,
            router: None,
        };
        let mut backend = OrchestratorHost::new(Some(parts), Arc::new(|| {})).expect("backend");
        assert!(backend.admission().is_none(), "no host yet");
        let mut model_box = BoxModel::default();
        model_box.input = "/run make a".into();
        model_box.submit(&mut backend, None);
        let error = |m: &BoxModel| {
            m.lines().iter().find_map(|l| match l {
                Line::Error(e) if e.contains("did not start") => Some(e.clone()),
                _ => None,
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while error(&model_box).is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                model_box.lines()
            );
            model_box.absorb(&mut backend);
            std::thread::sleep(Duration::from_millis(10));
        }
        let e = error(&model_box).expect("error");
        assert!(e.contains("run.json is absent"), "{e}");
        assert!(backend.running().is_none());

        // An invalid file: the reason names the file and the fault.
        std::fs::write(dir.path().join(".kgraf").join(RUN_CONFIG_FILE), "{not json")
            .expect("write");
        let mut model_box = BoxModel::default();
        model_box.input = "/run make a".into();
        model_box.submit(&mut backend, None);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while error(&model_box).is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                model_box.lines()
            );
            model_box.absorb(&mut backend);
            std::thread::sleep(Duration::from_millis(10));
        }
        let e = error(&model_box).expect("error");
        assert!(e.contains(RUN_CONFIG_FILE), "{e}");
        // Nothing was built: no host, so no admission, no ledger and no provider,
        // hence no provider call.
        assert!(backend.admission().is_none(), "no admission");
        assert!(factory.current().is_none(), "no host");
        assert!(
            !dir.path().join(".kgraf").join("ledger.db").exists(),
            "no ledger was opened, so no call was accounted or made"
        );
        assert!(backend.running().is_none());
    }
}
