//! `kgraf schedule add|list|remove|enable|run-now` (decision 0052).
//!
//! **Persistence.** A schedule is a property of a work node (decision 0052 section
//! 3): `kgraf_schedule::node_store` stores its definition in the graph's
//! `work_schedules` table against the node, and appends every run event to that
//! node's history. `add --node <id>` attaches to an existing node; without `--node`,
//! a node is created from the instruction (`schedules/<id>`). Every subcommand
//! first moves any schedule still stored in the old graph-meta layout onto a work
//! node, and prints a notice for each one it moves or cannot move.
//!
//! **The claim table** (due occurrences, fencing, consecutive failures) is the
//! separate SQLite file `kgraf_schedule::ScheduleStore` already owns
//! (`.kgraf/schedule.db`), unchanged; `list` reads both and shows them together.
//!
//! **`run-now`** is a manual, immediate, single invocation. It reuses
//! `orchestrate_cmd::run` directly (the same live, ledgered code path `kgraf
//! orchestrate` uses) with the stored instruction written to a temporary file, and
//! its start and outcome are recorded on the schedule's node. It respects the claim
//! table: a schedule the table has disabled is refused unless `--force`, and it
//! takes the schedule's manual lock (`ScheduleStore::claim_manual_run`) for the
//! length of the run, so the scheduler loop does not start the same schedule
//! beside it and a second `run-now` is refused. It records no occurrence; its run
//! id and outcome go in the claim table's `manual_runs`, so `list` counts it among
//! the schedule's runs and its spend. The
//! scheduler loop that uses the claim table lives in
//! `kgraf-orchestrator::scheduler_loop` and is driven from `kgraf-shell`'s
//! background mode.

use anyhow::{Context, Result, bail};
use kgraf_core::PriceTable;
use kgraf_graph::GraphStore;
use kgraf_provider::Ledger;
use kgraf_schedule::node_store::{self, EventKind, NodeChoice, RunRecord};
use kgraf_schedule::{ManualClaim, MissedRunPolicy, ScheduleDefinition, ScheduleStore};
use std::path::{Path, PathBuf};

use crate::orchestrate_cmd::{self, OrchestrateArgs};

pub struct AddArgs {
    pub id: String,
    pub instruction: PathBuf,
    pub zone: String,
    pub cron: Option<String>,
    pub at: Option<String>,
    pub missed_policy: String,
    /// The work node to attach to; `None` creates one from the instruction.
    pub node: Option<String>,
}

pub struct RunNowArgs {
    pub id: String,
    pub routes: Vec<String>,
    pub planner: Option<String>,
    /// `--model-route kind=route:model` (decision 0089).
    pub model_routes: Vec<String>,
    pub prices: PathBuf,
    pub max_items: usize,
    pub max_agents: usize,
    pub steps: u32,
    /// The user setting or the default (`run-now` has no flag for it).
    pub max_checkpoints: u32,
    pub max_tokens: u32,
    /// The orchestrator's output cap: the user setting or the default
    /// (`run-now` has no flag for it).
    pub planner_max_tokens: u32,
    /// `--warn-rule-conflicts`, else the user setting, else off.
    pub warn_rule_conflicts: bool,
    /// The answer wait: the user setting, else derived per call (`run-now`
    /// has no flag for it).
    pub answer_wait_secs: Option<u32>,
    /// The `[reasoning]` user setting (`run-now` has no flag for it).
    pub reasoning: kgraf_provider::ReasoningLevels,
    pub window: u64,
    /// `--verify`; without it, the first validation command the project's build
    /// files declare.
    pub verify: Option<String>,
    /// `--force`: run although the claim table has the schedule disabled.
    pub force: bool,
    /// `--shell-timeout` (decision 0084).
    pub shell_timeout: Option<std::time::Duration>,
    /// `--fetch-deps` / `--no-fetch-deps` (decision 0087).
    pub fetch_deps: kgraf_host::deps::RunFetch,
    /// `--keep-worktrees` (decision 0088), this command's only source for it:
    /// `run-now` does not read `.kgraf/run.json`.
    pub worktrees: kgraf_host::WorktreeRetention,
}

/// Opens the graph and moves any schedule still in the old graph-meta layout onto
/// a work node, printing one notice per schedule (the move is visible, never
/// silent). A schedule that cannot move keeps its old row and is named.
fn open(root: &Path) -> Result<GraphStore> {
    let graph = crate::open_graph_facts(root)?;
    for m in node_store::migrate_legacy(&graph, chrono::Utc::now())
        .context("cannot move old schedules onto work nodes")?
    {
        eprintln!("notice: {m}");
    }
    Ok(graph)
}

fn open_claims(root: &Path) -> Result<ScheduleStore> {
    ScheduleStore::open(&crate::state_dir(root)?.join("schedule.db"))
        .context("cannot open the claim table")
}

/// Records one event on the schedule's node. A failure is printed, not fatal: the
/// action the user asked for already happened, and hiding the gap would be worse.
fn record_or_warn(graph: &GraphStore, rec: &RunRecord<'_>) {
    if let Err(e) = node_store::record(graph, rec, chrono::Utc::now()) {
        eprintln!(
            "warning: could not record `{}` on schedule `{}`'s work node: {e}",
            rec.kind.as_str(),
            rec.schedule_id
        );
    }
}

/// A cheap, non-cryptographic uniqueness suffix for a run id: the low bits of a
/// monotonic clock reading XORed with the process id, so two `run-now` invocations of
/// the same schedule in the same second (the `unix_seconds` component's own
/// resolution) do not collide. Not a `rand` dependency: nothing here needs
/// unpredictability, only "different from the last one".
fn rand_suffix() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ std::process::id()
}

fn missed_policy(name: &str) -> Result<MissedRunPolicy> {
    match name {
        "skip" => Ok(MissedRunPolicy::Skip),
        "run-once" => Ok(MissedRunPolicy::RunOnceOnWake),
        other => bail!("unknown --missed-policy `{other}`; give `skip` or `run-once`"),
    }
}

pub fn add(root: &Path, args: AddArgs) -> Result<()> {
    let instruction = std::fs::read_to_string(&args.instruction)
        .with_context(|| format!("cannot read {}", args.instruction.display()))?;
    if instruction.trim().is_empty() {
        bail!(
            "the instruction file {} is empty",
            args.instruction.display()
        );
    }
    let policy = missed_policy(&args.missed_policy)?;

    let def = match (&args.cron, &args.at) {
        (Some(_), Some(_)) => bail!("give --cron or --at, not both"),
        (None, None) => bail!("give --cron PATTERN (recurring) or --at TIME (one-shot)"),
        (Some(cron), None) => {
            ScheduleDefinition::recurring(args.id.clone(), instruction, cron, &args.zone)
                .context("the recurrence rule or time zone was refused")?
        }
        (None, Some(at)) => {
            let instant = chrono::DateTime::parse_from_rfc3339(at)
                .with_context(|| {
                    format!("`--at {at}` is not RFC 3339 (example: 2026-09-27T03:00:00Z)")
                })?
                .with_timezone(&chrono::Utc);
            ScheduleDefinition::one_shot(args.id.clone(), instruction, instant, &args.zone)
                .context("the time zone was refused")?
        }
    }
    .with_missed_run_policy(policy);

    let graph = open(root)?;
    // `node_store::add` re-validates the definition (decision 0052: a bad zone is
    // refused at creation, and this is the point past which it is durable), refuses
    // an existing id, and refuses a `--node` the graph does not hold.
    let choice = match &args.node {
        Some(node) => NodeChoice::Existing(node),
        None => NodeChoice::FromInstruction,
    };
    let node = node_store::add(&graph, &def, choice, chrono::Utc::now())
        .context("the schedule was not added")?;
    match &args.node {
        Some(_) => println!("schedule `{}` added on work node `{node}`", args.id),
        None => println!(
            "schedule `{}` added on work node `{node}`, created from the instruction",
            args.id
        ),
    }
    Ok(())
}

/// Re-enables a schedule the claim table disabled (a schedule paused after a readiness failure, or
/// after too many consecutive transient failures, stayed paused forever with no path back). Does
/// not touch `enabled` in the definition itself (that is the separate, user-set on/off switch
/// `schedule add`/a future `schedule disable` would toggle); this clears the claim table's own
/// disable state.
pub fn enable(root: &Path, id: &str) -> Result<()> {
    let graph = open(root)?;
    if graph
        .schedule(id)
        .context("cannot read the schedule")?
        .is_none()
    {
        bail!("no schedule named `{id}`");
    }
    let claims = open_claims(root)?;
    claims.enable(id).context("cannot clear the disable")?;
    // The node's label clears its disable reason from this row.
    record_or_warn(
        &graph,
        &RunRecord {
            schedule_id: id,
            kind: EventKind::Enabled,
            run_id: None,
            due_at: None,
            detail: "kgraf schedule enable",
            spend: None,
        },
    );
    println!("schedule `{id}` enabled");
    Ok(())
}

/// The price table `schedule list --prices` reads, if one was given.
fn list_price_table(prices: Option<&Path>) -> Result<Option<PriceTable>> {
    prices.map(crate::load_price_table).transpose()
}

pub fn list(root: &Path, prices: Option<&Path>) -> Result<()> {
    let graph = open(root)?;
    let schedules = node_store::load_all(&graph).context("cannot read the schedules")?;
    if schedules.is_empty() {
        println!("no schedules");
        return Ok(());
    }
    let views = node_store::views(&graph).context("cannot read the schedules' history")?;
    let claims = open_claims(root)?;
    // The number of scheduled runs and their total spend are reported, not
    // only their outcomes" - a schedule that never fails is still worth knowing ran
    // (and cost something), not just silently absent from a failures-only view.
    let ledger = crate::open_ledger(root).ok();
    let price_table = list_price_table(prices)?;
    for entry in &schedules {
        let def = &entry.definition;
        let kind = def.when();
        let status = claims
            .schedule_status(&def.id)
            .context("cannot read schedule status")?;
        let state = if !def.enabled {
            "disabled (by user)".to_owned()
        } else if let Some(reason) = &status.disabled_reason {
            format!("disabled: {reason}")
        } else if status.consecutive_failures > 0 {
            format!(
                "enabled, {} consecutive failure(s)",
                status.consecutive_failures
            )
        } else {
            "enabled".to_owned()
        };
        // Clock-started and run-now runs together.
        let counts = claims
            .run_counts(&def.id)
            .context("cannot read the schedule's run counts")?;
        let (done, failed) = (counts.done, counts.failed);
        println!("{:<20} {kind:<44} {state}", def.id);
        println!("  work node: {}", entry.node_id);
        if let Some(run) = views
            .iter()
            .find(|v| v.schedule_id == def.id)
            .and_then(|v| v.last_run.as_ref())
        {
            let word: &str = match EventKind::parse(&run.event) {
                Some(kind) => kind.word(),
                None => &run.event,
            };
            let run_id = run
                .run_id
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            let detail = if run.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", run.detail)
            };
            println!("  last run: {word}{run_id}{detail}");
        }
        let spend = ledger
            .as_ref()
            .map(|l| spend_for_schedule(l, &claims, &def.id, price_table.as_ref()))
            .transpose()?;
        match spend {
            Some(money) => println!("  runs: {done} done, {failed} failed; spend {money}"),
            None => println!("  runs: {done} done, {failed} failed; spend unavailable (no ledger)"),
        }
        if counts.unsettled > 0 {
            println!(
                "  {} run-now run(s) with no outcome: still going, or ended before recording one",
                counts.unsettled
            );
        }
    }
    Ok(())
}

/// Sums this schedule's spend by looking up the exact
/// run ids the claim table itself recorded for it
/// (`ScheduleStore::run_ids_for_schedule`, written by `SchedulerLoop::start_and_track`
/// the moment each run actually started) and asking the ledger about each one with
/// [`Ledger::calls_under_task`]'s exact-or-hierarchical match, never a hand-rolled
/// prefix guess against `schedule_id` itself.
///
/// **Fixes the `foo` / `foo-bar` collision.** The previous version matched
/// `task_id.starts_with("sched-{schedule_id}-")`, so schedule `foo`'s prefix
/// (`sched-foo-`) was itself a prefix of schedule `foo-bar`'s run ids
/// (`sched-foo-bar-...`) - `foo`'s spend silently included every run `foo-bar` ever
/// made. Looking a stored run id up by exact match rules that out structurally: no
/// string comparison between two different schedules' ids ever happens here at all.
///
/// With a price table, spend is reported in dollars, settled calls only
/// ([`Ledger::spend_of`]), with unreconciled or unpriced calls counted apart rather
/// than folded in at zero. Without one, raw token counts are reported
/// instead, matching how `kgraf spend` already treats cost as optional.
fn spend_for_schedule(
    ledger: &Ledger,
    claims: &ScheduleStore,
    schedule_id: &str,
    prices: Option<&PriceTable>,
) -> Result<String> {
    let run_ids = claims
        .run_ids_for_schedule(schedule_id)
        .context("cannot read this schedule's run ids")?;
    spend_of_runs(ledger, &run_ids, prices)
}

/// The spend of the calls under `run_ids` (exact or hierarchical task match), in
/// dollars with a price table or as token counts without one (see
/// [`spend_for_schedule`]).
fn spend_of_runs(
    ledger: &Ledger,
    run_ids: &[String],
    prices: Option<&PriceTable>,
) -> Result<String> {
    let mut calls = Vec::new();
    for run_id in run_ids {
        calls.extend(
            ledger
                .calls_under_task(run_id, None)
                .context("cannot read the ledger")?,
        );
    }

    match prices {
        Some(table) => {
            let ids: Vec<_> = calls.iter().map(|c| c.id.clone()).collect();
            let total = calls.len();
            let spend = ledger.spend_of(&ids, table).context("cannot derive cost")?;
            if spend.unknown.is_empty() {
                Ok(format!("{} ({total} call(s), settled)", spend.total))
            } else {
                Ok(format!(
                    "{} ({} call(s) settled, {} unreconciled/unpriced)",
                    spend.total,
                    total - spend.unknown.len(),
                    spend.unknown.len()
                ))
            }
        }
        None => {
            let mut usage = kgraf_core::TokenUsage::default();
            for call in &calls {
                usage.uncached_input += call.usage.uncached_input;
                usage.cache_read += call.usage.cache_read;
                usage.cache_write += call.usage.cache_write;
                usage.text_output += call.usage.text_output;
                usage.reasoning_output += call.usage.reasoning_output;
                if let Some(chars) = call.usage.unsplit_reasoning_chars {
                    *usage.unsplit_reasoning_chars.get_or_insert(0) += chars;
                }
            }
            Ok(format!(
                "{} call(s), in {} (cache r {} w {}) out {} (reasoning {}) (pass --prices for dollars)",
                calls.len(),
                usage.uncached_input,
                usage.cache_read,
                usage.cache_write,
                usage.text_output,
                usage.reasoning_label()
            ))
        }
    }
}

pub fn remove(root: &Path, id: &str) -> Result<()> {
    let graph = open(root)?;
    let Some(node) =
        node_store::remove(&graph, id, chrono::Utc::now()).context("cannot remove the schedule")?
    else {
        // An old graph-meta row the move refused is deleted here.
        if node_store::remove_legacy(&graph, id).context("cannot remove the old row")? {
            println!("old graph-meta schedule `{id}` deleted; it was not on a work node");
            return Ok(());
        }
        bail!("no schedule named `{id}`");
    };
    // Clears the claim table's bookkeeping for this id too - otherwise a later
    // `schedule add` reusing the same id would inherit a stranger's disable state,
    // failure count, and claimed-occurrence history from a schedule that, as far as
    // the new definition is concerned, never existed.
    let claims = open_claims(root)?;
    claims
        .remove_schedule(id)
        .context("cannot clear the claim table's bookkeeping for this schedule")?;
    println!(
        "schedule `{id}` removed and its claim-table bookkeeping cleared; \
         its run history stays on work node `{node}`"
    );
    Ok(())
}

/// Runs a stored schedule's instruction immediately, through the same live
/// `orchestrate` path `kgraf orchestrate` uses (readiness, admission, spend caps
/// and the ledger all apply exactly as they do there).
pub fn run_now(root: &Path, args: RunNowArgs) -> Result<()> {
    run_now_with(root, args, orchestrate_cmd::run)
}

/// How long a manual run's lock lasts without a renewal, and how often the
/// holder renews it. A holder that died stops blocking the loop within the lease.
const MANUAL_LEASE: std::time::Duration = std::time::Duration::from_secs(120);
const MANUAL_RENEW: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(50)
} else {
    std::time::Duration::from_secs(30)
};

/// The manual lock on a schedule, renewed by a thread until it is dropped.
struct ManualRunLock {
    path: PathBuf,
    schedule_id: String,
    holder: String,
    stop: Option<std::sync::mpsc::Sender<()>>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
    /// Set by the heartbeat when a renewal finds the lock no longer ours. From
    /// then on the scheduler may claim the same schedule while the run goes on.
    lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ManualRunLock {
    /// Take the lock, or say why not: disabled (unless `force`) or busy.
    fn take(root: &Path, schedule_id: &str, holder: &str, force: bool) -> Result<Self> {
        let path = crate::state_dir(root)?.join("schedule.db");
        let claims = ScheduleStore::open(&path).context("cannot open the claim table")?;
        match claims
            .claim_manual_run(schedule_id, holder, chrono::Utc::now(), MANUAL_LEASE, force)
            .context("cannot take the schedule's run lock")?
        {
            ManualClaim::Claimed { .. } => {}
            ManualClaim::Disabled { reason } => bail!(
                "schedule `{schedule_id}` is disabled in the claim table ({reason}); \
                 `kgraf schedule enable {schedule_id}` clears that, or pass --force to run it once"
            ),
            ManualClaim::Busy { holder } => bail!(
                "schedule `{schedule_id}` is already running: {holder}; wait for it to finish"
            ),
        }
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let (beat_path, beat_id, beat_holder) =
            (path.clone(), schedule_id.to_owned(), holder.to_owned());
        let lost = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let beat_lost = std::sync::Arc::clone(&lost);
        let heartbeat = std::thread::spawn(move || {
            // Its own connection: a connection is not shared across threads. An
            // open that fails is reported and retried on the next beat: a lock
            // that is never renewed lapses, and the run must say so.
            let mut claims = None;
            // Ends when the sender is dropped or sends.
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                wait.recv_timeout(MANUAL_RENEW)
            {
                if claims.is_none() {
                    match ScheduleStore::open(&beat_path) {
                        Ok(opened) => claims = Some(opened),
                        Err(e) => {
                            eprintln!(
                                "warning: could not open the claim table to renew the run lock \
                                 on `{beat_id}`: {e}"
                            );
                            continue;
                        }
                    }
                }
                let Some(claims) = claims.as_ref() else {
                    continue;
                };
                match claims.renew_manual_run(
                    &beat_id,
                    &beat_holder,
                    chrono::Utc::now(),
                    MANUAL_LEASE,
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        beat_lost.store(true, std::sync::atomic::Ordering::SeqCst);
                        eprintln!(
                            "warning: the run lock on `{beat_id}` was lost; the run's result \
                             will be reported as a failure"
                        );
                        return;
                    }
                    Err(e) => {
                        eprintln!("warning: could not renew the run lock on `{beat_id}`: {e}")
                    }
                }
            }
        });
        Ok(Self {
            path,
            schedule_id: schedule_id.to_owned(),
            holder: holder.to_owned(),
            stop: Some(stop),
            heartbeat: Some(heartbeat),
            lost,
        })
    }

    /// Whether the lock was lost while it was held.
    fn was_lost(&self) -> bool {
        self.lost.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for ManualRunLock {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        let released = ScheduleStore::open(&self.path)
            .and_then(|claims| claims.release_manual_run(&self.schedule_id, &self.holder));
        if let Err(e) = released {
            eprintln!(
                "warning: could not release the run lock on `{}`: {e}",
                self.schedule_id
            );
        }
    }
}

/// The verify command for a manual run: the flag, else the first validation
/// command the project's build files declare. A project with neither has none,
/// and the run does not start with a command nobody chose.
fn verify_command(root: &Path, flag: Option<String>) -> Result<String> {
    if let Some(command) = flag {
        return Ok(command);
    }
    let found = kgraf_host::discover_validation_commands(root, &[])
        .context("cannot look for the project's test command")?;
    let first = found.into_iter().next().context(
        "no --verify command was given and the project's build files declare none; \
         pass --verify",
    )?;
    println!("verify     {first} (from the project's build files)");
    Ok(first)
}

/// `run_now` with the run itself injected, so a test can stand in for a live run.
fn run_now_with(
    root: &Path,
    args: RunNowArgs,
    orchestrate: impl FnOnce(&Path, OrchestrateArgs) -> Result<()>,
) -> Result<()> {
    let graph = open(root)?;
    let schedules = node_store::load_all(&graph).context("cannot read the schedules")?;
    let def = schedules
        .iter()
        .map(|s| &s.definition)
        .find(|d| d.id == args.id)
        .with_context(|| format!("no schedule named `{}`", args.id))?;
    // A manual run-now still refuses a schedule the user disabled, for the same
    // reason a disabled schedule never fires from the background loop: a disable is
    // a statement that this instruction should not run as configured.
    if !def.enabled {
        bail!(
            "schedule `{}` is turned off (its `enabled` flag is false); `schedule remove` \
             and re-`add` it, or edit the definition, to turn it back on before running it",
            args.id
        );
    }

    let verify = verify_command(root, args.verify.clone())?;
    let tmp =
        tempfile::NamedTempFile::new().context("cannot create a temporary instruction file")?;
    std::fs::write(tmp.path(), &def.instruction)
        .context("cannot write the temporary instruction file")?;
    // A pre-epoch clock must fail loudly, not silently collapse to a
    // fixed "0" run id that could then collide with another run-now started the same
    // way; a random suffix rules out two run-now invocations in the same second
    // colliding, and `sanitize_id` keeps the id safe as a git ref / file-name
    // component the way every other run id in this codebase already is.
    let unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the system clock reads before the Unix epoch; cannot form a run id from it")?
        .as_secs();
    let suffix: u32 = rand_suffix();
    let run_id =
        kgraf_orchestrator::sanitize_id(&format!("sched-{}-{unix_seconds}-{suffix:08x}", args.id));

    // The schedule's lock, held until this function returns: a disabled schedule
    // is refused here, and so is one that is already running.
    let lock = ManualRunLock::take(root, &args.id, &format!("run-now:{run_id}"), args.force)?;

    // `run-now` never claims an occurrence
    // (this file's own module doc explains why), so without this its spend would be
    // invisible to `schedule list` entirely - undercounting the reported figure (the
    // number of scheduled runs and their total spend). Recorded before
    // the run starts, not after, so a run that is interrupted mid-way (its own calls
    // already settled in the ledger by then) still has its spend found under this
    // schedule rather than only a run that happens to finish cleanly. A failure here
    // is bookkeeping, not fatal to the run the user explicitly asked for - printed
    // visibly rather than silently swallowed, and the run proceeds either
    // way.
    if let Err(e) = ScheduleStore::open(&crate::state_dir(root)?.join("schedule.db"))
        .and_then(|claims| claims.record_manual_run(&args.id, &run_id))
    {
        eprintln!(
            "warning: could not record this run-now's id against `{}`'s spend: {e}",
            args.id
        );
    }

    // A manual run is recorded on the schedule's node too, start and outcome.
    let record = |kind: EventKind, detail: &str, spend: Option<&kgraf_graph::RunSpend>| {
        record_or_warn(
            &graph,
            &RunRecord {
                schedule_id: &args.id,
                kind,
                run_id: Some(&run_id),
                due_at: None,
                detail,
                spend,
            },
        )
    };
    record(EventKind::Started, "manual run-now", None);
    let prices_path = args.prices.clone();
    let result = orchestrate(
        root,
        OrchestrateArgs {
            routes: args.routes,
            planner: args.planner,
            model_routes: args.model_routes,
            instruction: Some(tmp.path().to_path_buf()),
            resume: None,
            run: Some(run_id.clone()),
            prices: args.prices,
            max_items: args.max_items,
            max_agents: args.max_agents,
            steps: args.steps,
            max_checkpoints: args.max_checkpoints,
            max_tokens: args.max_tokens,
            window: args.window,
            point: None,
            verify,
            interactive: false,
            gate_commands: Vec::new(),
            gate_timeout: std::time::Duration::from_secs(1800),
            // Contract checks as a run has them by default: off.
            contract_checks: kgraf_host::config::defaults::contract_checks(),
            interface_review: kgraf_host::config::defaults::interface_review(),
            worktrees: args.worktrees,
            // Sessions as a run has them by default: on (decision 0067).
            sessions: kgraf_host::config::defaults::sessions(),
            check_timeout: kgraf_host::config::defaults::check_timeout(),
            check_compile_timeout: kgraf_host::config::defaults::check_compile_timeout(),
            shell_timeout: args.shell_timeout,
            fetch_deps: args.fetch_deps,
            planner_max_tokens: args.planner_max_tokens,
            // The `[reasoning]` user setting, as every run takes it.
            reasoning: args.reasoning,
            retry_gate: None,
            waive_gate: None,
            gate_note: None,
            // A scheduled run-now always runs from a stored instruction, never a
            // corpus reading - this is a fixed, already-written
            // instruction, not something to interpret fresh each time.
            corpus: false,
            approve: None,
            accept_commands: Vec::new(),
            corrections: Vec::new(),
            interpret_budget: None,
            // No rate limits of its own: a scheduled run-now shares the same
            // provider routes an interactive `orchestrate` would, and inherits
            // whatever limits are already configured process-wide.
            rate_limits: Vec::new(),
            // Routing policy as a run has it by default (decision 0075):
            // rotation. `kgraf schedule` does not (yet) carry its own
            // `--routing`; a scheduled run-now follows whatever an
            // interactive `orchestrate` would default to.
            routing: kgraf_orchestrator::RoutingPolicy::default(),
            // A scheduled run-now is never the Terminal-Bench pilot's own
            // invocation (that pilot always calls `kgraf orchestrate`
            // directly); this stays unset here.
            tbench_bridge_socket: None,
            // Decision 0094: a scheduled run has nobody at a command line:
            // it runs under the user-level permissions file alone, and what
            // would ask is denied.
            permissions: Default::default(),
            approve_plan: false,
            warn_rule_conflicts: args.warn_rule_conflicts,
            answer_wait_secs: args.answer_wait_secs,
        },
    );
    let spend = match run_spend(root, &run_id, &prices_path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("warning: the spend of run {run_id} could not be read: {e:#}");
            None
        }
    };
    let spend = spend.as_ref();
    // A lock lost mid-run means the scheduler may have started the same schedule
    // beside this run, and nothing here can stop the run already going. It is
    // reported as a failure, whatever the run itself returned, and the run's own
    // outcome is kept in the message.
    let result = if lock.was_lost() {
        let own = match &result {
            Ok(()) => "the run itself completed".to_owned(),
            Err(e) => format!("the run itself failed: {e:#}"),
        };
        Err(anyhow::anyhow!(
            "the run lock on schedule `{}` was lost while run {run_id} was going, so the \
             scheduler may have started the same schedule too ({own}); check the schedule's \
             history before trusting either run",
            args.id
        ))
    } else {
        result
    };
    match &result {
        Ok(()) => record(EventKind::Completed, "manual run-now", spend),
        Err(e) => record(EventKind::Failed, &format!("{e:#}"), spend),
    }
    // The claim table's run counts are what `schedule list` reports; a run-now
    // claims no occurrence, so its outcome is set on its `manual_runs` row. A
    // failure here is bookkeeping, printed, and does not change the run's result.
    match ScheduleStore::open(&crate::state_dir(root)?.join("schedule.db"))
        .and_then(|claims| claims.record_manual_outcome(&args.id, &run_id, result.is_ok()))
    {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "warning: run {run_id} has no row in `{}`'s claim table, so `schedule list` \
             does not count it",
            args.id
        ),
        Err(e) => eprintln!(
            "warning: could not record run {run_id}'s outcome in `{}`'s run counts: {e}",
            args.id
        ),
    }
    result
}

/// One run's spend, priced from `prices`, read from the ledger by run id: the
/// numbers the node's history row stores (settled micro-dollars, calls, calls whose
/// spend is not known) and the text its label shows.
fn run_spend(root: &Path, run_id: &str, prices: &Path) -> Result<kgraf_graph::RunSpend> {
    let ledger = crate::open_ledger(root)?;
    run_spend_in(&ledger, run_id, prices)
}

/// [`run_spend`] over an open ledger.
fn run_spend_in(ledger: &Ledger, run_id: &str, prices: &Path) -> Result<kgraf_graph::RunSpend> {
    let table = crate::load_price_table(prices)?;
    let calls = ledger
        .calls_under_task(run_id, None)
        .context("cannot read the ledger")?;
    let ids: Vec<_> = calls.iter().map(|c| c.id.clone()).collect();
    let spend = ledger
        .spend_of(&ids, &table)
        .context("cannot derive cost")?;
    Ok(kgraf_graph::RunSpend {
        micros: spend.total.micros(),
        calls: calls.len() as i64,
        unpriced: spend.unknown.len() as i64,
        text: spend_of_runs(ledger, &[run_id.to_owned()], Some(&table))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::{Protocol, TokenUsage};
    use kgraf_schedule::ClaimOutcome;

    #[test]
    fn run_spend_never_prices_an_azure_deployment_by_a_bare_model_entry() {
        let dir = tempfile::tempdir().expect("dir");
        let (ledger, path) = crate::tests::azure_call_with_bare_price_file(dir.path());
        let spend = run_spend_in(&ledger, "run-1", &path).expect("spend");
        assert_eq!(spend.micros, 0);
        assert_eq!(spend.calls, 1);
        assert_eq!(spend.unpriced, 1);
    }

    #[test]
    fn schedule_list_never_prices_an_azure_deployment_by_a_bare_model_entry() {
        let dir = tempfile::tempdir().expect("dir");
        let (ledger, path) = crate::tests::azure_call_with_bare_price_file(dir.path());
        let table = list_price_table(Some(&path))
            .expect("table")
            .expect("given");
        let report = spend_of_runs(&ledger, &["run-1".to_owned()], Some(&table)).expect("spend");
        assert!(report.contains("1 unreconciled/unpriced"), "got {report}");
        assert!(!report.starts_with("$1.000000"), "got {report}");
    }
    use std::time::Duration;

    #[test]
    fn two_concurrent_adds_never_lose_an_entry() {
        // two concurrent `schedule add` calls
        // must never lose one another's entry. Each add inserts its own
        // `work_schedules` row and upserts the shared `schedules` plan root inside
        // one IMMEDIATE transaction per write, so the two serialize on SQLite's
        // write lock instead of racing a read-modify-write.
        let dir = tempfile::tempdir().expect("dir");
        let db_path = dir.path().join("graph.db");
        // Both connections open the schema once before threads race, so neither
        // thread's `CREATE TABLE IF NOT EXISTS` races the other's first write.
        drop(GraphStore::open(&db_path, dir.path()).expect("graph"));

        let make_def = |id: &str| {
            ScheduleDefinition::one_shot(
                id,
                "do it",
                chrono::Utc::now() + chrono::Duration::hours(1),
                "UTC",
            )
            .expect("valid")
        };
        let spawn = |def: ScheduleDefinition| {
            let db = db_path.clone();
            let root = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let graph = GraphStore::open(&db, &root).expect("graph");
                node_store::add(
                    &graph,
                    &def,
                    NodeChoice::FromInstruction,
                    chrono::Utc::now(),
                )
                .expect("add");
            })
        };
        let a = spawn(make_def("a"));
        let b = spawn(make_def("b"));
        a.join().expect("thread a");
        b.join().expect("thread b");

        let graph = GraphStore::open(&db_path, dir.path()).expect("graph");
        let mut ids: Vec<String> = node_store::load_all(&graph)
            .expect("load")
            .into_iter()
            .map(|s| s.definition.id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["a".to_owned(), "b".to_owned()],
            "both concurrent adds must survive - neither may be lost to the other"
        );
    }

    fn add_args(dir: &Path, id: &str, node: Option<&str>) -> AddArgs {
        let instruction = dir.join(format!("{id}.txt"));
        std::fs::write(&instruction, "Keep the build green\n").expect("write");
        AddArgs {
            id: id.to_owned(),
            instruction,
            zone: "UTC".to_owned(),
            cron: Some("0 0 3 * * *".to_owned()),
            at: None,
            missed_policy: "run-once".to_owned(),
            node: node.map(str::to_owned),
        }
    }

    /// `schedule add --node` attaches to that node; an unknown node is refused
    /// with nothing written; `remove` detaches but the node keeps its history.
    #[test]
    fn add_on_a_node_then_remove_keeps_the_history_on_the_node() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        drop(crate::open_graph_for_index(root).expect("graph"));
        let err = add(root, add_args(root, "nightly", Some("p1/w1"))).expect_err("no node");
        assert!(
            format!("{err:#}").contains("no work node `p1/w1`"),
            "{err:#}"
        );
        {
            let graph = crate::open_graph_for_index(root).expect("graph");
            assert!(graph.schedules().expect("read").is_empty());
            graph
                .put_work(
                    &[
                        kgraf_graph::WorkNode {
                            id: "p1".into(),
                            plan_id: "p1".into(),
                            kind: kgraf_graph::WorkKind::Plan,
                            parent_id: None,
                            title: "plan".into(),
                            body: "{}".into(),
                            created_ms: 1,
                        },
                        kgraf_graph::WorkNode {
                            id: "p1/w1".into(),
                            plan_id: "p1".into(),
                            kind: kgraf_graph::WorkKind::Item,
                            parent_id: Some("p1".into()),
                            title: "green build".into(),
                            body: "{}".into(),
                            created_ms: 1,
                        },
                    ],
                    &[],
                )
                .expect("put");
        }
        add(root, add_args(root, "nightly", Some("p1/w1"))).expect("add");
        remove(root, "nightly").expect("remove");
        let graph = crate::open_graph_for_index(root).expect("graph");
        assert!(graph.schedule("nightly").expect("read").is_none());
        let history: Vec<String> = graph
            .node_schedule_events("p1/w1", 10)
            .expect("history")
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(history, ["removed", "added"]);
    }

    /// A schedule stored the old way (a graph-meta row) is moved onto
    /// a created work node by the next subcommand, and runs from there.
    #[test]
    fn an_old_meta_schedule_is_moved_onto_a_node_by_the_next_subcommand() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        let old = ScheduleDefinition::recurring("legacy", "Old task", "0 0 3 * * *", "UTC")
            .expect("valid");
        {
            let graph = crate::open_graph_for_index(root).expect("graph");
            graph
                .set_meta(
                    &kgraf_schedule::schedule_key("legacy"),
                    &serde_json::to_string(&old).expect("json"),
                )
                .expect("meta");
        }
        list(root, None).expect("list");
        let graph = crate::open_graph_for_index(root).expect("graph");
        let all = node_store::load_all(&graph).expect("load");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].node_id, "schedules/legacy");
        assert_eq!(all[0].definition, old);
        assert!(
            graph
                .meta(&kgraf_schedule::schedule_key("legacy"))
                .expect("meta")
                .is_none()
        );
    }

    /// An old row the move refused is deleted by `schedule remove`.
    #[test]
    fn remove_deletes_an_old_row_the_move_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        {
            let graph = crate::open_graph_for_index(root).expect("graph");
            graph
                .set_meta(&kgraf_schedule::schedule_key("broken"), "{not json")
                .expect("meta");
        }
        list(root, None).expect("list");
        remove(root, "broken").expect("remove");
        let graph = crate::open_graph_for_index(root).expect("graph");
        assert!(
            graph
                .meta(&kgraf_schedule::schedule_key("broken"))
                .expect("meta")
                .is_none()
        );
        assert!(remove(root, "broken").is_err(), "gone");
    }

    fn claim_and_name_run(
        claims: &ScheduleStore,
        schedule_id: &str,
        due: chrono::DateTime<chrono::Utc>,
        run_id: &str,
    ) {
        let generation = match claims
            .claim_occurrence(schedule_id, due, "a", due, Duration::from_secs(900))
            .expect("claim")
        {
            ClaimOutcome::Claimed { generation, .. } => generation,
            ClaimOutcome::NotClaimed => panic!("expected the claim to succeed"),
        };
        claims
            .set_run_id(schedule_id, due, "a", generation, run_id)
            .expect("set_run_id");
    }

    fn usage(uncached_input: u64) -> TokenUsage {
        TokenUsage {
            uncached_input,
            ..TokenUsage::default()
        }
    }

    #[test]
    fn spend_for_schedule_never_collides_a_prefix_with_a_longer_sibling_id() {
        // the defect the previous
        // `task_id.starts_with("sched-{schedule_id}-")` match had - `foo`'s prefix
        // (`sched-foo-`) is itself a prefix of `foo-bar`'s run id
        // (`sched-foo-bar-...`), so `foo`'s reported spend silently included every
        // call `foo-bar` ever made. Looking up the exact, stored run id per schedule
        // rules this out structurally: no string comparison between the two
        // schedules' ids happens anywhere in `spend_for_schedule`.
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let due = chrono::Utc::now();

        claim_and_name_run(&claims, "foo", due, "sched-foo-1000");
        claim_and_name_run(&claims, "foo-bar", due, "sched-foo-bar-2000");

        // `foo`'s own call, plus a worker subtask under it - `calls_under_task`'s
        // hierarchy match (`task/...`) is what is meant to find the latter.
        ledger
            .begin("test", Protocol::Additive, "m", "a", "sched-foo-1000")
            .expect("begin")
            .settle(&usage(10), None)
            .expect("settle");
        ledger
            .begin(
                "test",
                Protocol::Additive,
                "m",
                "worker",
                "sched-foo-1000/contract",
            )
            .expect("begin")
            .settle(&usage(20), None)
            .expect("settle");
        // `foo-bar`'s own call - must never be counted toward `foo`'s spend.
        ledger
            .begin("test", Protocol::Additive, "m", "a", "sched-foo-bar-2000")
            .expect("begin")
            .settle(&usage(999), None)
            .expect("settle");

        let foo = spend_for_schedule(&ledger, &claims, "foo", None).expect("foo spend");
        assert!(
            foo.starts_with("2 call(s)") && foo.contains("in 30"),
            "foo's spend must be exactly its own two calls (10 + 20 input tokens), got {foo}"
        );

        let foo_bar = spend_for_schedule(&ledger, &claims, "foo-bar", None).expect("foo-bar spend");
        assert!(
            foo_bar.starts_with("1 call(s)") && foo_bar.contains("in 999"),
            "foo-bar's spend must be exactly its own one call, got {foo_bar}"
        );
    }

    #[test]
    fn spend_for_schedule_looks_up_a_run_id_with_characters_a_prefix_guess_would_mangle() {
        // "a sanitized id": `ScheduledRuns::run_id` runs every schedule id through
        // `sanitize_id` before forming a run id, so a schedule id containing a
        // character that function rewrites (anything outside
        // `[A-Za-z0-9_.-]`, folded to `-`) would, under the old prefix-guessing
        // scheme, need `spend_for_schedule` to reimplement that same rewrite just to
        // find its own calls. Reading the run id back verbatim from the claim table
        // needs no such reimplementation: whatever `ScheduledRuns::run_id` actually
        // produced is exactly what was stored.
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let due = chrono::Utc::now();
        let schedule_id = "nightly.report";
        let run_id =
            kgraf_orchestrator::sanitize_id(&format!("sched-{schedule_id}-{}", due.timestamp()));
        claim_and_name_run(&claims, schedule_id, due, &run_id);
        ledger
            .begin("test", Protocol::Additive, "m", "a", &run_id)
            .expect("begin")
            .settle(&usage(7), None)
            .expect("settle");

        let report = spend_for_schedule(&ledger, &claims, schedule_id, None).expect("spend");
        assert!(
            report.starts_with("1 call(s)") && report.contains("in 7"),
            "got {report}"
        );
    }

    #[test]
    fn spend_for_schedule_with_a_price_table_reports_dollars_and_separates_unreconciled() {
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let due = chrono::Utc::now();
        claim_and_name_run(&claims, "nightly", due, "sched-nightly-1");

        ledger
            .begin("test", Protocol::Additive, "priced", "a", "sched-nightly-1")
            .expect("begin")
            .settle(&usage(1_000_000), None)
            .expect("settle");
        // An unreconciled call under the same run: left open, never settled.
        ledger
            .begin("test", Protocol::Additive, "priced", "a", "sched-nightly-1")
            .expect("begin");

        let mut prices = PriceTable::new();
        prices.set(
            "priced",
            kgraf_core::ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );

        let report =
            spend_for_schedule(&ledger, &claims, "nightly", Some(&prices)).expect("priced spend");
        assert!(
            report.starts_with("$1.000000"),
            "settled spend must be priced in dollars, got {report}"
        );
        assert!(
            report.contains("1 unreconciled"),
            "the still-open call must be shown apart, not folded into the total, got {report}"
        );
    }

    fn run_now_args(id: &str, force: bool) -> RunNowArgs {
        RunNowArgs {
            id: id.to_owned(),
            routes: vec!["ollama:llama3".to_owned()],
            planner: None,
            model_routes: Vec::new(),
            prices: PathBuf::from("no-such-prices.json"),
            max_items: 1,
            max_agents: 1,
            steps: 1,
            max_checkpoints: kgraf_host::limits::DEFAULT_MAX_CHECKPOINTS,
            max_tokens: 16,
            planner_max_tokens: 8_192,
            warn_rule_conflicts: false,
            answer_wait_secs: None,
            reasoning: kgraf_provider::ReasoningLevels::new(),
            window: 1000,
            verify: Some("true".to_owned()),
            force,
            shell_timeout: None,
            fetch_deps: kgraf_host::deps::RunFetch::Stored,
            worktrees: kgraf_host::WorktreeRetention::default(),
        }
    }

    fn scheduled(root: &Path) {
        drop(crate::open_graph_for_index(root).expect("graph"));
        add(root, add_args(root, "nightly", None)).expect("add");
    }

    fn events(root: &Path) -> Vec<String> {
        let graph = crate::open_graph_for_index(root).expect("graph");
        let mut all: Vec<String> = graph
            .schedule_events("nightly", 20)
            .expect("events")
            .into_iter()
            .map(|e| e.event)
            .collect();
        all.reverse();
        all
    }

    /// A run that ends blocked is an error from the run, and `run-now` records
    /// it as failed, not completed.
    #[test]
    fn a_blocked_run_is_recorded_as_failed() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        scheduled(root);
        let blocked = |_: &Path, _: OrchestrateArgs| -> Result<()> {
            let report =
                crate::orchestrate_cmd::test_report(kgraf_orchestrator::RunStatus::Blocked {
                    reason: "needs the user".into(),
                });
            crate::orchestrate_cmd::ensure_completed(&report)
        };
        let error = run_now_with(root, run_now_args("nightly", false), blocked)
            .expect_err("a blocked run fails");
        assert!(format!("{error:#}").contains("needs the user"), "{error:#}");
        let recorded = events(root);
        assert_eq!(
            recorded.last().map(String::as_str),
            Some("failed"),
            "{recorded:?}"
        );
        assert!(!recorded.iter().any(|e| e == "completed"), "{recorded:?}");
        assert_eq!(
            open_claims(root)
                .expect("claims")
                .run_counts("nightly")
                .expect("counts"),
            kgraf_schedule::RunCounts {
                done: 0,
                failed: 1,
                unsettled: 0
            },
            "the failed run-now is counted among the schedule's runs"
        );
    }

    #[test]
    fn a_completed_run_is_recorded_as_completed() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        scheduled(root);
        run_now_with(root, run_now_args("nightly", false), |_, _| Ok(())).expect("ok");
        assert_eq!(events(root).last().map(String::as_str), Some("completed"));
        // `schedule list` counts runs from the claim table; a run-now claims no
        // occurrence, so its outcome must reach the table another way or the list
        // shows `0 done, 0 failed` beside a completed last run.
        assert_eq!(
            open_claims(root)
                .expect("claims")
                .run_counts("nightly")
                .expect("counts"),
            kgraf_schedule::RunCounts {
                done: 1,
                failed: 0,
                unsettled: 0
            }
        );
    }

    /// While the run goes, it is counted unsettled: started, not yet done.
    #[test]
    fn a_running_run_now_is_counted_unsettled_until_it_ends() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        scheduled(root);
        run_now_with(root, run_now_args("nightly", false), |root, _| {
            let counts = open_claims(root)?.run_counts("nightly")?;
            assert_eq!(counts.unsettled, 1, "{counts:?}");
            assert_eq!(counts.done + counts.failed, 0, "{counts:?}");
            Ok(())
        })
        .expect("ok");
        assert_eq!(
            open_claims(root)
                .expect("claims")
                .run_counts("nightly")
                .expect("counts")
                .done,
            1
        );
    }

    fn disable(root: &Path) {
        let claims = open_claims(root).expect("claims");
        let now = chrono::Utc::now();
        let generation = match claims
            .claim_occurrence("nightly", now, "loop", now, Duration::from_secs(60))
            .expect("claim")
        {
            ClaimOutcome::Claimed { generation, .. } => generation,
            ClaimOutcome::NotClaimed => panic!("expected the claim to succeed"),
        };
        claims
            .record_outcome(
                "nightly",
                now,
                "loop",
                generation,
                &kgraf_schedule::RunOutcome::Failed(kgraf_schedule::FailureClass::Disabling(
                    kgraf_schedule::DisableReason::ConfigurationMissing {
                        detail: "no such route".into(),
                    },
                )),
            )
            .expect("disable");
    }

    /// A schedule the claim table disabled is refused, and `--force` runs it.
    #[test]
    fn a_claim_table_disable_refuses_run_now_unless_forced() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        scheduled(root);
        disable(root);
        let mut ran = false;
        let error = run_now_with(root, run_now_args("nightly", false), |_, _| {
            ran = true;
            Ok(())
        })
        .expect_err("disabled");
        assert!(!ran, "the run must not start");
        let text = format!("{error:#}");
        assert!(
            text.contains("disabled") && text.contains("--force"),
            "{text}"
        );

        let mut ran = false;
        run_now_with(root, run_now_args("nightly", true), |_, _| {
            ran = true;
            Ok(())
        })
        .expect("forced");
        assert!(ran);
    }

    /// While a run-now is running, a second one is refused and the scheduler
    /// loop cannot claim the schedule; afterwards both work again.
    #[test]
    fn a_running_run_now_holds_the_schedule_and_releases_it() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        scheduled(root);
        let inner = std::cell::RefCell::new(None);
        run_now_with(root, run_now_args("nightly", false), |_, _| {
            let second = run_now_with(root, run_now_args("nightly", false), |_, _| Ok(()));
            let claims = open_claims(root).expect("claims");
            let now = chrono::Utc::now();
            let loop_claim = claims
                .claim_occurrence(
                    "nightly",
                    now - chrono::Duration::seconds(1),
                    "loop",
                    now,
                    Duration::from_secs(60),
                )
                .expect("claim");
            *inner.borrow_mut() = Some((second, loop_claim));
            Ok(())
        })
        .expect("outer");
        let (second, loop_claim) = inner.into_inner().expect("ran");
        let text = format!("{:#}", second.expect_err("second is refused"));
        assert!(text.contains("already running"), "{text}");
        assert_eq!(loop_claim, ClaimOutcome::NotClaimed);

        // Released: another run-now works, and the held occurrence is claimable.
        run_now_with(root, run_now_args("nightly", false), |_, _| Ok(())).expect("again");
    }

    /// A lock that is lost mid-run fails the run-now, even when the run itself
    /// completed: the scheduler may have started the same schedule beside it.
    #[test]
    fn a_lost_run_lock_fails_the_run_now() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        scheduled(root);
        let error = run_now_with(root, run_now_args("nightly", false), |_, args| {
            let holder = format!("run-now:{}", args.run.as_deref().expect("run id"));
            open_claims(root)
                .expect("claims")
                .release_manual_run("nightly", &holder)
                .expect("take the lock away");
            // Longer than a few renewals.
            std::thread::sleep(Duration::from_millis(600));
            Ok(())
        })
        .expect_err("a lost lock fails the run");
        let text = format!("{error:#}");
        assert!(
            text.contains("lock") && text.contains("lost") && text.contains("completed"),
            "{text}"
        );
        assert_eq!(events(root).last().map(String::as_str), Some("failed"));
    }

    #[test]
    fn verify_falls_back_to_the_projects_own_test_command() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        assert_eq!(
            verify_command(root, Some("make check".into())).expect("flag"),
            "make check"
        );
        let none = verify_command(root, None).expect_err("nothing declared");
        assert!(format!("{none:#}").contains("--verify"), "{none:#}");
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts": {"test": "jest"}}"#,
        )
        .expect("write");
        assert_eq!(verify_command(root, None).expect("npm"), "npm test");
    }
}
