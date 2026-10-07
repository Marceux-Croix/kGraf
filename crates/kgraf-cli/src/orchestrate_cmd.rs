//! `kgraf orchestrate`: run the orchestrator against live provider
//! routes, or resume a run from its record.
//!
//! This is the live acceptance path for the orchestrator (verified against
//! running speakers). The run is built by `kgraf-host`, which the desktop shell
//! uses too; this file maps the flags onto a `RunConfig`, calls the host's steps in
//! order, and prints what they report. Every model call, the orchestrator's and
//! every worker's, goes through the project ledger. Each worker gets its own git
//! worktree under `.kgraf/worktrees`, created at the run's current
//! integration tip and reopened by its recorded gitlink when a killed run resumes.
//! Each worker's environment is assembled in its own worktree: the shell bound to
//! it with its approval hook, and the MCP servers of `.kgraf/worker.json`.
//! The verifier runs `--verify` in the worktree and commits the change through
//! merge-back; the orchestrator then merges the selected attempt into the run's
//! integration branch, `kgraf/<run>` (continuous merge-back, 0018). A conflict
//! comes back typed and the contract runs again from the new tip. Quality gates run
//! the project's discovered validation commands (or `--gate-command`) in a sandbox,
//! in a worktree of the integration commit they pin. `--interactive` reads stdin as
//! the message port: a line is a note, a line starting with `?` is a status
//! question.

use anyhow::{Context, Result, bail};
use kgraf_core::RunId;
use kgraf_host::{CorpusRequest, Host, Notice, RunConfig, RunStart};
use kgraf_orchestrator::{
    GateDecision, InboundKind, InboundMessage, MessagePort, OutboundReply, PortError, UntrustedText,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct OrchestrateArgs {
    pub routes: Vec<String>,
    pub planner: Option<String>,
    /// `--model-route kind=route:model` (decision 0089).
    pub model_routes: Vec<String>,
    pub instruction: Option<PathBuf>,
    pub resume: Option<String>,
    pub run: Option<String>,
    pub prices: PathBuf,
    pub max_items: usize,
    pub max_agents: usize,
    pub steps: u32,
    /// Most progress checkpoints per worker (decision 0096).
    pub max_checkpoints: u32,
    pub max_tokens: u32,
    pub window: u64,
    pub point: Option<u8>,
    pub verify: String,
    pub interactive: bool,
    /// Validation commands for quality gates; empty discovers them.
    pub gate_commands: Vec<String>,
    /// Longest one gate command, or one run of `verify`, may run.
    pub gate_timeout: Duration,
    /// Interface contracts written, checked before dispatch, open to challenge.
    pub contract_checks: bool,
    /// Cross-vendor review at the interface (decision 0079): a model on a
    /// cross-vendor consumer's route reviews a producer's delivery before it
    /// counts as verified. Off by default; unmeasured. Threaded through
    /// `RunConfig` into `kgraf-host`, which installs a `RoutedInterfaceReviewer`
    /// on the orchestrator next to `Host::contract_checks` (`host.rs`).
    pub interface_review: bool,
    /// `--keep-worktrees` (decision 0088): whether a worker's worktree and its
    /// build output are deleted after use. The flag is this command's only
    /// source for it: the command does not read `.kgraf/run.json`.
    pub worktrees: kgraf_host::WorktreeRetention,
    /// Every agent is an addressable session (decision 0067); on by
    /// default since a live run shows all three together.
    pub sessions: bool,
    /// Bounds of one contract-check test run and one compile.
    pub check_timeout: Duration,
    pub check_compile_timeout: Duration,
    /// `--shell-timeout` (decision 0084): wins over the worker configuration.
    pub shell_timeout: Option<Duration>,
    /// Output ceiling of each orchestrator call.
    pub planner_max_tokens: u32,
    /// A held or failed gate to retry, or to waive, before `--resume`.
    pub retry_gate: Option<String>,
    pub waive_gate: Option<String>,
    pub gate_note: Option<String>,
    /// Start from the corpus.
    pub corpus: bool,
    /// With `corpus`: approve the stored reading with this plan id and run it.
    pub approve: Option<String>,
    /// With `approve`: proposed validation commands accepted as gates, by name.
    pub accept_commands: Vec<String>,
    pub corrections: Vec<String>,
    pub interpret_budget: Option<u64>,
    /// `ROUTE=RPM[/BURST][@MAX_WAIT_MS]` per rate-limited route.
    pub rate_limits: Vec<String>,
    /// Vendor/model routing policy (decision 0075): `rotation` (default, and
    /// what today's three-arm experiment depends on being unchanged) or
    /// `fitness` (coupling- and measured-capability-aware).
    pub routing: kgraf_orchestrator::RoutingPolicy,
    /// A Unix socket a Python Harbor shim listens on. See
    /// `RunConfig::tbench_bridge_socket`.
    pub tbench_bridge_socket: Option<PathBuf>,
    /// `--fetch-deps` / `--no-fetch-deps` (decision 0087).
    pub fetch_deps: kgraf_host::deps::RunFetch,
    /// The permission flags (decision 0094): the mode, the allow set, the
    /// rules, plan review and the user-level file, built into one control by
    /// [`crate::permissions_cli::control`].
    pub permissions: crate::permissions_cli::PermissionArgs,
    /// `--approve-plan` (decision 0094).
    pub approve_plan: bool,
    /// Warn at run start about a verify or gate command that runs a program
    /// your rules block for workers (`--warn-rule-conflicts`, else the user
    /// setting, else off).
    pub warn_rule_conflicts: bool,
    /// `--answer-wait`, else the user setting; `None` derives the wait per
    /// call from the planner model's recent latencies.
    pub answer_wait_secs: Option<u32>,
}

/// The per-project part of the flags, as the host takes it. What is per
/// invocation (the instruction, a resume, a gate decision, the corpus options,
/// `--interactive`) stays here.
fn run_config(root: &Path, args: &OrchestrateArgs) -> RunConfig {
    RunConfig {
        root: root.to_path_buf(),
        routes: args.routes.clone(),
        planner: args.planner.clone(),
        model_routes: args.model_routes.clone(),
        prices: args.prices.clone(),
        cap_run: None,
        cap_task: None,
        cap_hour: None,
        max_items: args.max_items,
        max_agents: args.max_agents,
        steps: args.steps,
        max_checkpoints: args.max_checkpoints,
        max_tokens: args.max_tokens,
        window: args.window,
        point: args.point,
        verify: args.verify.clone(),
        gate_commands: args.gate_commands.clone(),
        warn_rule_conflicts: args.warn_rule_conflicts,
        answer_wait_secs: args.answer_wait_secs,
        gate_timeout: args.gate_timeout,
        rate_limits: args.rate_limits.clone(),
        contract_checks: args.contract_checks,
        interface_review: args.interface_review,
        worktrees: args.worktrees,
        check_timeout: args.check_timeout,
        check_compile_timeout: args.check_compile_timeout,
        shell_timeout: args.shell_timeout,
        // Decision 0067: on by default (`--sessions false`
        // to turn it off): a live run shows the steer, an
        // orchestrator-answered question and an admitted, merged subagent
        // together.
        sessions: args.sessions,
        planner_max_tokens: args.planner_max_tokens,
        routing: args.routing,
        tbench_bridge_socket: args.tbench_bridge_socket.clone(),
        dependency_fetch: args.fetch_deps,
        approve_plan: args.approve_plan,
    }
}

/// One line per notice, as the command always printed them.
fn notice_line(n: &Notice) -> String {
    match n {
        Notice::Cleaned(removed) => format!(
            "cleaned    {} ({:?})",
            removed.path.display(),
            removed.cause
        ),
        Notice::Stranded { path, name } => format!(
            "stranded   {}: its owner ran on another machine or in another PID namespace \
             (or its host is unreadable); check, then run `kgraf clean {name}`",
            path.display()
        ),
        Notice::CleanedMcpHome(path) => {
            format!("cleaned    {} (a dead process's MCP home)", path.display())
        }
        Notice::CleanedCheckScratch(path) => format!(
            "cleaned    {} (a dead process's contract-check scratch)",
            path.display()
        ),
        Notice::AncestorReadRefused(detail) => format!("sandbox    {detail}"),
        Notice::NoWorkerConfig(path) => format!(
            "config     none ({} is absent): no MCP servers",
            path.display()
        ),
        Notice::McpServer {
            route,
            server,
            report,
        } => format!("mcp        {route}: {server}: {report:?}"),
        Notice::EstimatedSpend {
            call,
            model,
            amount,
            reason,
        } => format!("estimated  ledger call {call} ({model}) at {amount}: {reason}"),
        Notice::PrefixChanged { route, change } => format!(
            "cache miss {route}: its shared prefix differs from the last run's in this process \
             ({} -> {}); the first calls pay a cache write",
            change.old, change.new
        ),
        Notice::DependenciesFetched { registry, summary } => {
            format!("fetched    dependencies from {registry} (decision 0087): {summary}")
        }
        Notice::DependenciesUnchecked(why) => format!(
            "deps       the offline dependency check could not decide, so nothing was fetched: \
             {why}"
        ),
        Notice::DependenciesNotInstalled(missing) => {
            format!("deps       not installed: {}", missing.join("; "))
        }
        Notice::GateCommands(commands) if commands.is_empty() => {
            "gates      none: no test command was configured or found, so the run has no quality \
             gate"
                .to_owned()
        }
        Notice::GateCommands(commands) => format!("gates      {}", commands.join("; ")),
        Notice::AcceptanceNarrowed {
            command,
            added_gate,
        } => format!(
            "accept     each contract runs the tests of its own files from `{command}`; the whole \
             command runs on the merged result as a root gate{}",
            if *added_gate { " (added)" } else { "" }
        ),
        Notice::AcceptanceWhole {
            command,
            added_gate,
        } => format!(
            "accept     each contract runs `{command}` whole; it also runs on the merged result \
             as a root gate{}, and it is deferred to that gate for a contract that cannot pass it \
             before the plan's other contracts merge",
            if *added_gate { " (added)" } else { "" }
        ),
        Notice::OutputCapClamped { which, clamp } => {
            format!("clamped    {}", clamp.sentence(which))
        }
        Notice::RuleConflict(conflict) => format!("notice     {}", conflict.sentence()),
        Notice::RuleMatchesNoTool(sentence) => format!("notice     {sentence}"),
        Notice::ChecksNetworkOpen(sentence) => format!("sandbox    {sentence}"),
        Notice::GraphEmpty { root, files_seen } => format!(
            "graph      empty after indexing {} ({files_seen} file(s) seen): no graph slices \
             for this run, likely no supported language in the tree",
            root.display()
        ),
    }
}

fn print_notice(n: Notice) {
    println!("{}", notice_line(&n));
}

/// How long a run may take to stop after the first interrupt: workers
/// cancelled, calls settled, worktrees listed. Past it the process exits and
/// says so; startup cleanup (decision 0056) handles what is left.
const STOP_BOUND: Duration = Duration::from_secs(30);

/// A run that ended because the process was asked to stop: SIGINT or SIGTERM
/// on Unix, Ctrl-C on Windows. `main` turns it into the conventional exit code
/// (128 plus the signal number), so a supervisor can tell it from a failure.
#[derive(Debug, thiserror::Error)]
#[error("run {run} was interrupted by {name}{detail}")]
pub struct Interrupted {
    run: RunId,
    name: &'static str,
    signal: i32,
    detail: String,
}

impl Interrupted {
    /// The process exit code: 128 plus the signal number.
    pub fn exit_code(&self) -> u8 {
        u8::try_from(128 + self.signal).unwrap_or(130)
    }
}

/// The interrupt signals of this process. Installed before anything else a
/// run does, so a signal that arrives during setup is kept, not lost, and so
/// the dependency fetch's own handler (`kgraf_host::deps`) chains to this one
/// instead of ending the process.
struct Interrupts {
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl Interrupts {
    /// Must run inside the tokio runtime.
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                int: signal(SignalKind::interrupt())?,
                term: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    /// The next interrupt: its number (2 for SIGINT and Ctrl-C, 15 for SIGTERM).
    async fn next(&mut self) -> i32 {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.int.recv() => 2,
                _ = self.term.recv() => 15,
            }
        }
        #[cfg(windows)]
        {
            self.ctrl_c.recv().await;
            2
        }
    }

    /// An interrupt already received, without waiting for one.
    async fn pending(&mut self) -> Option<i32> {
        tokio::time::timeout(Duration::ZERO, self.next()).await.ok()
    }
}

fn signal_name(signal: i32) -> &'static str {
    match (signal, cfg!(windows)) {
        (15, _) => "SIGTERM",
        (_, true) => "Ctrl-C",
        _ => "SIGINT",
    }
}

fn interrupted(run: &RunId, signal: i32, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Interrupted {
        run: run.clone(),
        name: signal_name(signal),
        signal,
        detail: detail.into(),
    })
}

/// The end of a run that was asked to stop twice, or did not stop within
/// [`STOP_BOUND`]. Exits at once: the run is still going, and a second
/// interrupt is the user saying not to wait. What is left on disk is
/// handled by the next run's startup cleanup.
fn cut_short(why: &str, signal: i32) -> ! {
    println!(
        "cut short  {why}; worktrees and attempt refs of this run may be left on disk and calls \
         may still be open: `kgraf unreconciled` lists calls, `kgraf clean <worktree>` removes a \
         worktree, and the next run's startup removes stale ones"
    );
    std::process::exit(128 + signal)
}

pub fn run(root: &Path, args: OrchestrateArgs) -> Result<()> {
    // Decision 0094: the user-level permissions file and this command's
    // flags, the only sources of the mode. Bypass is announced on stderr.
    let permissions = crate::permissions_cli::control(root, &args.permissions)?;
    // Decision 0094: a new run with plan review on has nobody to approve the plan
    // on the command line, so it is refused here, before `Host::open` creates
    // `.kgraf`, its databases or an integration branch, and before a trace path is
    // printed for a trace that is never written. A reading (`--corpus` without
    // `--approve`) starts no run, and a resume is judged against its own record by
    // the orchestrator.
    let starts_a_run = args.resume.is_none() && !(args.corpus && args.approve.is_none());
    if starts_a_run {
        kgraf_orchestrator::plan_review_refusal(
            permissions.plan_review(),
            false, // the command line is never attended
            args.approve_plan,
        )?;
    }
    let host = Host::open(run_config(root, &args))?.with_permissions(permissions);
    let run_id = match (&args.resume, &args.run) {
        (Some(r), _) => RunId::new(r.clone()),
        (None, Some(r)) => RunId::new(r.clone()),
        (None, None) => RunId::new(format!(
            "run-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        )),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let mut interrupts = runtime
        .block_on(async { Interrupts::install() })
        .context("cannot listen for interrupts")?;

    // Reading a corpus (with or without corrections) creates nothing in
    // the repository: no branch, no worktree. Approval names the plan id of a
    // stored reading and makes no model call.
    if args.corpus && args.approve.is_none() {
        let instruction = match &args.instruction {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                if text.trim().is_empty() {
                    bail!("the instruction file {} is empty", path.display());
                }
                Some(text)
            }
            None => None,
        };
        // Decision 0064: the roadmap first (deterministic, no tokens), so the
        // reading's items that cite a section a roadmap node stands for are not
        // presumed twice. No roadmap is said, not raised: the reading below
        // plans from the whole corpus, or from the instruction alone.
        host.with_graph(|g| crate::plan_cmd::presume_after_index(g, "the corpus is still read"))?;
        let reading_call = host.read_corpus(
            &run_id,
            CorpusRequest {
                instruction,
                corrections: args.corrections.clone(),
                budget_tokens: args.interpret_budget,
            },
        );
        let intake = runtime.block_on(async {
            tokio::select! {
                intake = reading_call => Ok(intake),
                signal = interrupts.next() => Err(signal),
            }
        });
        let intake = match intake {
            Ok(intake) => intake?,
            // The reading call is dropped: its ledger row is left
            // unreconciled (it may have billed), never in flight.
            Err(signal) => {
                return Err(interrupted(
                    &run_id,
                    signal,
                    ": the reading was cut off, nothing was stored, and its model call may have \
                     billed (`kgraf unreconciled` lists it)",
                ));
            }
        };
        for c in &intake.charged {
            println!("ledger call {c} (interpretation)");
        }
        let reading = intake.result?;
        println!("{}", reading.reading());
        // Decision 0064: the reading's work items become presumed nodes now, so the
        // galaxy shows the plan before it is approved. The tokens were the
        // interpretation call's, already ledgered above; this makes no call.
        // A failure here is a stated line: the reading is paid for and
        // stored, and its plan id, printed below, is what the user approves.
        let presumed = host.with_graph(|g| {
            kgraf_interpret::presumed::presume_reading(g, &reading, crate::plan_cmd::now_ms())
        })?;
        match presumed {
            Err(e) => println!(
                "presumed   error: {e} (the reading is stored; its work items are not presumed)"
            ),
            Ok(presumed) if presumed.questions_only => {
                println!("presumed   nothing: the reading has questions and no work items yet")
            }
            Ok(presumed) => {
                let s = &presumed.sync;
                println!(
                    "presumed   {} from this reading ({} new, {} updated, {} unchanged, {} kept \
                 (changed by someone else), {} kept (adopted), {} gone); {} already stood for by \
                 a roadmap node",
                    s.inserted.len() + s.updated.len() + s.unchanged as usize,
                    s.inserted.len(),
                    s.updated.len(),
                    s.unchanged,
                    s.kept_edited.len(),
                    s.kept_adopted.len(),
                    s.gone.len(),
                    presumed.covered.len()
                );
            }
        }
        let t = reading.timings();
        println!(
            "first action in {} ms (index {} ms, brief {} ms, model {} ms)",
            t.first_action_ms, t.index_ms, t.brief_ms, t.model_ms
        );
        let accept: String = reading
            .proposed_commands()
            .iter()
            .map(|c| format!(" --accept-command {c:?}"))
            .collect();
        println!(
            "\nNot started, and nothing was created. To run this plan as shown:\n  \
             --corpus --approve {}{}\n(each --accept-command makes a proposed command a gate; \
             omit the ones you do not accept). To correct the reading instead: --corpus \
             --correction \"...\".",
            reading.plan_id(),
            accept
        );
        return Ok(());
    }
    let from_plan = match &args.approve {
        Some(plan_id) => Some(host.approve_plan(plan_id, &args.accept_commands)?),
        None => None,
    };

    let startup = host.startup(&mut print_notice)?;
    // The user's decision about a held gate is recorded before the run resumes.
    let decision = match (&args.retry_gate, &args.waive_gate) {
        (Some(_), Some(_)) => bail!("give --retry-gate or --waive-gate, not both"),
        (Some(g), None) => Some((g.clone(), GateDecision::Retry)),
        (None, Some(g)) => Some((g.clone(), GateDecision::Waive)),
        (None, None) => None,
    };
    if let Some((gate, decision)) = decision {
        if args.resume.is_none() {
            bail!("a gate decision applies to a run being resumed: give --resume RUN");
        }
        let note = args
            .gate_note
            .clone()
            .context("a gate decision needs --gate-note saying why")?;
        host.decide_gate(&run_id, &gate, decision, &note)?;
        println!("gate       {gate}: {decision:?} recorded ({note})");
    }
    let build = |host: &Host| {
        runtime.block_on(host.build(
            &startup,
            run_id.clone(),
            from_plan.as_ref(),
            &mut print_notice,
        ))
    };
    // Decision 0087: dependencies not in the cache and no answer yet. A
    // person at the terminal is asked once; nobody there, and the error
    // (with the instruction) stands. The gate runs before anything of the
    // run is built, so building again is safe.
    let prepared = match build(&host) {
        Ok(prepared) => prepared,
        Err(e) => match e.dependency_question() {
            Some(q) if crate::deps_cmd::attended() => {
                use crate::deps_cmd::Answer;
                use kgraf_host::deps::Consent;
                // The prompt blocks on the terminal, so it runs off the
                // runtime and an interrupt ends the process at once.
                let registry = q.registry;
                let asked = runtime.block_on(async {
                    let prompt = tokio::task::spawn_blocking(move || crate::deps_cmd::ask(&q));
                    tokio::select! {
                        answer = prompt => Ok(answer),
                        signal = interrupts.next() => Err(signal),
                    }
                });
                let answer = match asked {
                    Ok(answer) => answer.context("the dependency question ended abnormally")??,
                    Err(signal) => {
                        println!(
                            "stopped    {}: no dependency answer given",
                            signal_name(signal)
                        );
                        std::process::exit(128 + signal)
                    }
                };
                match answer {
                    Answer::Once => host.grant_dependency_fetch(&run_id),
                    Answer::Always => host.set_dependency_consent(registry, Consent::Allowed)?,
                    Answer::NotNow => return Err(e.into()),
                    // Stored, then built again: the refusal's own error (with
                    // how to undo it) is what the user reads.
                    Answer::Never => host.set_dependency_consent(registry, Consent::Refused)?,
                }
                build(&host)?
            }
            _ => return Err(e.into()),
        },
    };

    // An interrupt that came during setup: nothing has started, so say so.
    if let Some(signal) = runtime.block_on(interrupts.pending()) {
        return Err(interrupted(
            &run_id,
            signal,
            ": it arrived before the run started, so nothing was started",
        ));
    }

    let mut signaled: Option<i32> = None;
    let outcome = runtime.block_on(async {
        if args.interactive {
            prepared.attach_port(Arc::new(StdinPort::start()));
        }
        let summary = prepared.summary();
        println!("run        {}", summary.run);
        println!("integrate  {}", summary.integration);
        if summary.gate_commands.is_empty() {
            println!("gates      none: no validation commands discovered");
        } else {
            println!("gates      {}", summary.gate_commands.join(" && "));
        }
        println!("trace      {}", summary.trace.display());
        let shape = prepared.shape();
        if shape.contract_checks {
            println!(
                "contracts  interface contracts checked before dispatch (compile bound {}s, \
                 test bound {}s); workers may challenge them",
                shape.check_compile_timeout.as_secs(),
                shape.check_timeout.as_secs()
            );
        } else {
            println!(
                "contracts  off: no interface contracts, no checks (--contract-checks turns them on)"
            );
        }
        let start = if let Some(plan) = from_plan {
            println!(
                "plan       {} ({} items, {} already complete)",
                plan.plan_id(),
                plan.items().len(),
                plan.skipped().len()
            );
            RunStart::Plan(Box::new(plan))
        } else {
            match (&args.resume, &args.instruction) {
                (Some(_), _) => RunStart::Resume,
                (None, Some(path)) => {
                    let text = std::fs::read_to_string(path)
                        .with_context(|| format!("cannot read {}", path.display()))?;
                    if text.trim().is_empty() {
                        bail!("the instruction file {} is empty", path.display());
                    }
                    RunStart::Instruction(text)
                }
                (None, None) => bail!("give --instruction FILE, --corpus or --resume RUN"),
            }
        };
        let handle = prepared.handle();
        let running = prepared.run(start);
        tokio::pin!(running);
        let ended = tokio::select! {
            ended = &mut running => ended,
            signal = interrupts.next() => {
                signaled = Some(signal);
                println!(
                    "stopping   {} received: no new work starts, workers and model calls are \
                     cancelled, and calls and worktrees are accounted for (at most {} s; \
                     a second interrupt exits at once)",
                    signal_name(signal),
                    STOP_BOUND.as_secs()
                );
                // The same stop the desktop's stop button sends: cancels every
                // worker and planner call, keeps worktrees and transcripts, and
                // leaves the run record resumable.
                handle.stop().await;
                tokio::select! {
                    ended = &mut running => ended,
                    second = interrupts.next() => cut_short("a second interrupt arrived", second),
                    () = tokio::time::sleep(STOP_BOUND) => {
                        cut_short(
                            &format!("the {} s stop bound was reached", STOP_BOUND.as_secs()),
                            signal,
                        )
                    }
                }
            }
        };
        match ended {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                // A run that fails has no report, so the planner refusals it
                // recorded would be lost with it.
                for line in plan_refusal_lines(&summary.trace).await {
                    println!("{line}");
                }
                Err(anyhow::Error::from(e))
            }
        }
    })?;

    for line in report_lines(&outcome.report) {
        println!("{line}");
    }
    // Housekeeping after the report: a failed sweep is said, and never hides
    // the report or fails a run that completed.
    let housekeeping = outcome.housekeeping;
    match housekeeping.swept {
        Ok(swept) if !swept.is_empty() => println!("swept      {} attempt ref(s)", swept.len()),
        Ok(_) => {}
        Err(e) => println!("sweep      failed, attempt refs left in place: {e:#}"),
    }
    for line in kept_lines(&housekeeping.kept, &outcome.report.run) {
        println!("{line}");
    }
    if let Err(e) = &housekeeping.tidied {
        println!("tidy       failed, kGraf's worktree directories left in place: {e:#}");
    }
    let open = housekeeping.unreconciled?;
    if open != 0 {
        println!("{open} call(s) with unaccounted spend; run `kgraf unreconciled`");
    }
    // The report and housekeeping are out; now the exit code says how it went.
    // A run that stopped because the process was interrupted exits with the
    // signal's code; one that finished anyway is a finished run.
    if let Some(signal) = signaled
        && matches!(
            outcome.report.status,
            kgraf_orchestrator::RunStatus::Stopped
        )
    {
        return Err(interrupted(
            &outcome.report.run,
            signal,
            format!("; `--resume {}` continues it", outcome.report.run),
        ));
    }
    ensure_completed(&outcome.report)
}

/// One line per worker worktree the run left on disk, with how to continue or
/// remove it; a failure to list them is said. A kept worktree is never left
/// without a word.
fn kept_lines(kept: &kgraf_host::Result<Vec<PathBuf>>, run: &RunId) -> Vec<String> {
    match kept {
        Ok(paths) => paths
            .iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!(
                    "kept       worktree {name} ({}): `kgraf orchestrate --resume {run}` \
                     continues a held or stopped contract in it; `kgraf clean {name}` removes it",
                    p.display()
                )
            })
            .collect(),
        Err(e) => vec![format!("kept       worktrees could not be listed: {e:#}")],
    }
}

/// A run that did not complete is an error, so the exit code and a caller such
/// as `schedule run-now` see a blocked or stopped run as the failure it is.
pub(crate) fn ensure_completed(report: &kgraf_orchestrator::RunReport) -> Result<()> {
    use kgraf_orchestrator::RunStatus;
    match &report.status {
        RunStatus::Completed => Ok(()),
        RunStatus::Blocked { reason } => bail!("run {} is blocked: {reason}", report.run),
        RunStatus::Stopped => bail!(
            "run {} was stopped before it completed; `--resume {}` continues it",
            report.run,
            report.run
        ),
        RunStatus::Running => bail!("run {} ended while still marked running", report.run),
    }
}

/// `kgraf clean <worktree>`: remove a worktree on the owner's word (see
/// `kgraf_host::clean`).
pub fn clean(root: &Path, name: &str, force: bool) -> Result<PathBuf> {
    Ok(kgraf_host::clean(root, name, force)?)
}

/// A run report with the given status and nothing else set, for tests here and
/// in `schedule_cmd`.
#[cfg(test)]
pub(crate) fn test_report(status: kgraf_orchestrator::RunStatus) -> kgraf_orchestrator::RunReport {
    kgraf_orchestrator::RunReport {
        run: RunId::new("r9"),
        status,
        verified: Vec::new(),
        held: Vec::new(),
        counters: Default::default(),
        serialized_fraction: 0.0,
        lease_measurements: Default::default(),
        spent: kgraf_core::Money::ZERO,
        budget_admitted: 0,
        contracts: Default::default(),
        contract_checks: Default::default(),
        analytics_error: None,
        warnings: Vec::new(),
        notices: Vec::new(),
        unverified: Vec::new(),
        unfinished: Vec::new(),
    }
}

/// The run report as printed. A failure of the run analytics (the demand
/// record or a template update) is printed, never dropped.
fn report_lines(report: &kgraf_orchestrator::RunReport) -> Vec<String> {
    let mut out = vec![
        format!("status     {:?}", report.status),
        format!("verified   {}", report.verified.join(", ")),
    ];
    // Decision 0093: a verified contract whose check says nothing about the
    // work is said on its own line, so a completed run is never read as
    // checked when it was not.
    for (c, reason) in &report.unverified {
        out.push(format!("unverified {c}: {}", plain(reason)));
    }
    for (c, reason) in &report.held {
        out.push(format!("held       {c}: {reason}"));
    }
    // A run that did not complete names what it left unfinished, so a stop
    // with work left is never read as one with none (decision 0056).
    for (c, state) in &report.unfinished {
        out.push(format!("unfinished {c}: {}", plain(state)));
    }
    out.push(format!(
        "dispatched {} (serialized {}, fraction {:.3})",
        report.counters.dispatches, report.counters.serialized, report.serialized_fraction
    ));
    let leases = &report.lease_measurements;
    out.push(format!(
        "leases     {} path(s) held by more than one agent, needless waits {}",
        leases.serialized_paths,
        match leases.needless_wait_fraction {
            Some(f) => format!(
                "{:.3} of {} handoff(s) with declared ranges",
                f, leases.known_handoffs
            ),
            None => "not measured: no handoff had declared ranges on both sides".to_owned(),
        }
    ));
    out.push(format!(
        "refusals   capacity {}, readiness {} (reservations {})",
        report.counters.capacity_refusals,
        report.counters.readiness_failures,
        report.budget_admitted,
    ));
    // Decision 0095: a worker that compacted at its stop point is a sizing miss
    // as a hand-back is, one per worker.
    out.push(format!(
        "misses     {} (hand-backs, stop-point compactions and workers past their round budget)",
        report.counters.sizing_misses
    ));
    // Decision 0096: the round budget is a checkpoint.
    out.push(format!(
        "rounds     {} worker(s) ran past their round budget on measured progress; {} stopped \
         by a checkpoint (no progress twice in a row, or the ceiling)",
        report.counters.round_extensions, report.counters.checkpoint_stops
    ));
    // The graph is the context source: whether workers actually got slices is
    // printed, so a run that gave none is visible without reading the ledger.
    let n = &report.counters;
    out.push(if n.dispatches > 0 && n.sliced_dispatches == 0 {
        format!(
            "slices     no worker was given a graph slice in {} dispatch(es); {} named lookup(s) \
             found nothing",
            n.dispatches, n.empty_slice_lookups
        )
    } else {
        format!(
            "slices     given in {} of {} dispatch(es), {} from declared files; {} named \
             lookup(s) found nothing; {} left out of a brief (unreadable for the worker)",
            n.sliced_dispatches,
            n.dispatches,
            n.slices_from_writes,
            n.empty_slice_lookups,
            n.slices_left_out
        )
    });
    if report.contract_checks.enabled {
        let c = &report.contracts;
        out.push(format!(
            "contracts  {} (interfaces {}, passed {}, failed {}, not run {}, challenges {}, \
             escalations {})",
            report.contract_checks,
            c.interfaces_registered,
            c.checks_passed,
            c.artifact_failures
                + c.stub_compile_failures
                + c.vacuity_failures
                + c.handshake_failures,
            c.checks_not_run,
            c.challenges,
            c.escalations
        ));
        out.push(format!("interfaces  {}", c.interface_economy()));
    } else {
        out.push("contracts  off: no interface contract was written or checked".to_owned());
    }
    out.push(format!("spent      {}", report.spent));
    for w in &report.warnings {
        out.push(format!("warning    {}", plain(w)));
    }
    for n in &report.notices {
        out.push(format!("notice     {}", plain(n)));
    }
    if let Some(e) = &report.analytics_error {
        out.push(format!(
            "analytics  FAILED: {e} (the run finished; its demand record may be missing)"
        ));
    }
    out
}

/// The planner refusals in a run's trace, one printed line each, for the error
/// path: a failed run prints no report, and the refusals explain the failure.
///
/// The file is read directly, never opened as a [`kgraf_orchestrator::TraceLog`]:
/// that takes the write lock a live run may still hold.
async fn plan_refusal_lines(trace: &Path) -> Vec<String> {
    match tokio::fs::read_to_string(trace).await {
        Ok(text) => {
            let events: Vec<kgraf_orchestrator::TraceEvent> = text
                .lines()
                .filter(|l| l.contains("\"plan_refused\"") || l.contains("\"interfaces_refused\""))
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            refusal_lines(&events)
        }
        // No trace file means the run failed before it wrote its first event, so
        // there are no refusals to show and nothing is wrong with the trace.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => vec![format!("notice     the trace could not be read: {e}")],
    }
}

fn refusal_lines(events: &[kgraf_orchestrator::TraceEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            kgraf_orchestrator::TraceEvent::PlanRefused {
                ask, of, reason, ..
            } => Some(format!(
                "notice     plan refused (ask {ask} of {of}): {}",
                plain(reason)
            )),
            kgraf_orchestrator::TraceEvent::InterfacesRefused {
                ask, of, reason, ..
            } => Some(format!(
                "notice     interfaces refused (ask {ask} of {of}): {}",
                plain(reason)
            )),
            _ => None,
        })
        .collect()
}

/// `text` with control characters replaced by `?`: warnings and notices can
/// carry a file name a worker created, and a terminal acts on an escape
/// sequence in one.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// stdin as a message port: `?` starts a status question, anything else is a
/// note. Replies print to stdout.
struct StdinPort {
    lines: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<String>>,
}

impl StdinPort {
    fn start() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            while stdin.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                if tx.blocking_send(line.trim_end().to_owned()).is_err() {
                    break;
                }
                line.clear();
            }
        });
        Self {
            lines: tokio::sync::Mutex::new(rx),
        }
    }
}

#[async_trait::async_trait]
impl MessagePort for StdinPort {
    fn channel(&self) -> &str {
        "stdin"
    }

    async fn receive(&self) -> Result<Option<InboundMessage>, PortError> {
        let mut rx = self.lines.lock().await;
        loop {
            let Some(line) = rx.recv().await else {
                return Ok(None);
            };
            if line.trim().is_empty() {
                continue;
            }
            let (kind, text) = match line.strip_prefix('?') {
                Some(q) => (InboundKind::StatusQuestion, q.trim().to_owned()),
                None => (InboundKind::Note, line),
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            return Ok(Some(InboundMessage {
                id: format!("stdin-{now}"),
                channel: "stdin".into(),
                received_ms: now,
                kind,
                text: UntrustedText::new(text),
            }));
        }
    }

    async fn reply(&self, reply: OutboundReply) -> Result<(), PortError> {
        println!("[{}] {}", reply.in_reply_to, reply.text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::Money;
    use kgraf_graph::GraphStore;
    use kgraf_orchestrator::OrchestratorConfig;
    use kgraf_provider::Ledger;
    use kgraf_runtime::ContextConfig;

    /// Each worktree the run left is named with how to continue or remove it.
    #[test]
    fn every_kept_worktree_is_named_with_its_way_out() {
        let run = RunId::new("r7");
        let kept = Ok(vec![PathBuf::from("/repo/.kgraf/worktrees/r7-c2-r0-a0")]);
        let lines = kept_lines(&kept, &run);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("worktree r7-c2-r0-a0"), "{}", lines[0]);
        assert!(lines[0].contains("--resume r7"), "{}", lines[0]);
        assert!(lines[0].contains("kgraf clean r7-c2-r0-a0"), "{}", lines[0]);
        assert!(kept_lines(&Ok(Vec::new()), &run).is_empty());
        let failed = kept_lines(&Err(kgraf_host::Error::Invalid("boom".into())), &run);
        assert!(failed[0].contains("could not be listed"), "{failed:?}");
    }

    /// The error path lists every planner refusal the trace holds.
    #[test]
    fn the_error_path_lists_each_plan_refusal() {
        use kgraf_orchestrator::TraceEvent;
        let refusal = |ask| TraceEvent::PlanRefused {
            at_ms: 0,
            ask,
            of: 3,
            reason: format!("refusal {ask}\u{1b}[31m"),
        };
        let lines = refusal_lines(&[refusal(1), refusal(2), refusal(3)]);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[2].contains("ask 3 of 3") && lines[2].contains("refusal 3"));
        assert!(!lines.iter().any(|l| l.contains('\u{1b}')), "plain text");
        let interfaces = refusal_lines(&[TraceEvent::InterfacesRefused {
            at_ms: 0,
            ask: 2,
            of: 3,
            reason: "no interface binds c5 <- c2".into(),
        }]);
        assert_eq!(
            interfaces,
            ["notice     interfaces refused (ask 2 of 3): no interface binds c5 <- c2"]
        );

        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("trace.jsonl");
        let text: String = (1..=3)
            .map(|ask| serde_json::to_string(&refusal(ask)).expect("json") + "\n")
            .collect();
        std::fs::write(&path, text).expect("write");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let from_file = rt.block_on(plan_refusal_lines(&path));
        assert_eq!(from_file.len(), 3, "{from_file:?}");
    }

    /// A run that failed before its first event wrote no trace. That is not a
    /// fault of the trace, so it prints no notice; a trace that exists and cannot
    /// be read (here, a directory in its place) still does.
    #[test]
    fn a_missing_trace_prints_no_notice_and_an_unreadable_one_does() {
        let dir = tempfile::tempdir().expect("dir");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let missing = rt.block_on(plan_refusal_lines(&dir.path().join("trace.jsonl")));
        assert!(missing.is_empty(), "{missing:?}");
        let unreadable = rt.block_on(plan_refusal_lines(dir.path()));
        assert_eq!(unreadable.len(), 1, "{unreadable:?}");
        assert!(unreadable[0].contains("the trace could not be read"));
    }

    /// Graph slice use is printed: how many dispatches were given a slice,
    /// how many of those from the declared files, and how many named lookups
    /// found nothing. A run that gave none says so in words.
    #[test]
    fn the_printed_report_says_how_many_dispatches_had_graph_slices() {
        let mut report = test_report(kgraf_orchestrator::RunStatus::Completed);
        report.counters.dispatches = 3;
        report.counters.sliced_dispatches = 2;
        report.counters.slices_from_writes = 1;
        report.counters.empty_slice_lookups = 4;
        report.counters.slices_left_out = 5;
        let lines = report_lines(&report);
        let line = lines
            .iter()
            .find(|l| l.starts_with("slices"))
            .expect("a slices line");
        assert!(line.contains("2 of 3 dispatch(es)"), "{line}");
        assert!(line.contains("1 from declared files"), "{line}");
        assert!(line.contains("4 named lookup(s) found nothing"), "{line}");
        assert!(line.contains("5 left out of a brief"), "{line}");

        report.counters.sliced_dispatches = 0;
        report.counters.slices_from_writes = 0;
        let lines = report_lines(&report);
        let line = lines
            .iter()
            .find(|l| l.starts_with("slices"))
            .expect("a slices line");
        assert!(line.contains("no worker was given a graph slice"), "{line}");
    }

    /// The lease measurements of decisions 0022 and 0049 are printed, and an
    /// unmeasured share says why, rather than printing a rate of zero.
    #[test]
    fn the_printed_report_carries_the_lease_measurements() {
        let mut report = test_report(kgraf_orchestrator::RunStatus::Completed);
        report.lease_measurements = kgraf_graph::LeaseMeasurements {
            serialized_paths: 3,
            known_handoffs: 4,
            needless_wait_fraction: Some(0.25),
        };
        let lines = report_lines(&report);
        let line = lines
            .iter()
            .find(|l| l.starts_with("leases"))
            .expect("a leases line");
        assert!(line.contains("3 path(s)"), "{line}");
        assert!(line.contains("0.250 of 4 handoff(s)"), "{line}");

        let unmeasured = report_lines(&test_report(kgraf_orchestrator::RunStatus::Completed));
        let line = unmeasured
            .iter()
            .find(|l| l.starts_with("leases"))
            .expect("a leases line");
        assert!(line.contains("not measured"), "{line}");
    }

    /// A run with contract checks reports what its interface call cost and
    /// what that bought, beside the `contracts` line the canary reads: planner
    /// calls, seconds and output tokens, and the interfaces accepted by kind.
    /// The `contracts` line keeps its form.
    #[test]
    fn the_printed_report_states_the_interface_calls_cost_and_yield() {
        let mut report = test_report(kgraf_orchestrator::RunStatus::Completed);
        report.contract_checks.enabled = true;
        report.contracts.interfaces_registered = 7;
        report.contracts.compile_only_registered = 6;
        report.contracts.interface_asks = 2;
        report.contracts.interface_ask_ms = 310_400;
        report.contracts.interface_ask_output_tokens = 9_400;
        let lines = report_lines(&report);
        assert!(
            lines.iter().any(|l| l
                == "interfaces  planner 2 calls 310 s 9,400 tokens; accepted 6 compile-only, 1 behavior"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("(interfaces 7, passed 0, failed 0, not run 0,")),
            "the contracts line keeps its form: {lines:?}"
        );
        // No call: zero, and the singular reads as one.
        report.contracts.interface_asks = 1;
        report.contracts.interface_ask_ms = 0;
        report.contracts.interface_ask_output_tokens = 0;
        report.contracts.compile_only_registered = 0;
        let lines = report_lines(&report);
        assert!(
            lines.iter().any(|l| l
                == "interfaces  planner 1 call 0 s 0 tokens; accepted 0 compile-only, 7 behavior"),
            "{lines:?}"
        );
    }

    /// The CLI report carries an analytics failure.
    #[test]
    fn the_printed_report_carries_an_analytics_failure() {
        let mut report = kgraf_orchestrator::RunReport {
            run: RunId::new("r1"),
            status: kgraf_orchestrator::RunStatus::Completed,
            verified: vec!["a".into()],
            held: Vec::new(),
            counters: Default::default(),
            serialized_fraction: 0.0,
            lease_measurements: Default::default(),
            spent: Money::ZERO,
            budget_admitted: 0,
            contracts: Default::default(),
            contract_checks: Default::default(),
            analytics_error: None,
            warnings: vec!["workflow template t1 quarantined".into()],
            notices: vec!["the check generated Cargo.lock".into()],
            unverified: Vec::new(),
            unfinished: Vec::new(),
        };
        assert!(
            !report_lines(&report)
                .iter()
                .any(|l| l.contains("analytics")),
            "nothing to report"
        );
        assert!(
            report_lines(&report)
                .iter()
                .any(|l| l == "contracts  off: no interface contract was written or checked"),
            "a run without checks says so"
        );
        assert!(
            report_lines(&report)
                .iter()
                .any(|l| l == "warning    workflow template t1 quarantined"),
            "warnings are printed"
        );
        assert!(
            report_lines(&report)
                .iter()
                .any(|l| l == "notice     the check generated Cargo.lock"),
            "notices are printed"
        );
        // A worker-made file name with an escape sequence never reaches the
        // terminal as one.
        let mut hostile = report.clone();
        hostile.notices = vec!["made \u{1b}[2Jx\ny.txt".into()];
        hostile.warnings = vec!["w\u{7}".into()];
        let printed = report_lines(&hostile);
        assert!(
            printed.iter().any(|l| l == "notice     made ?[2Jx?y.txt"),
            "{printed:?}"
        );
        assert!(printed.iter().any(|l| l == "warning    w?"), "{printed:?}");
        report.analytics_error = Some("graph: database is locked".into());
        let lines = report_lines(&report);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("analytics  FAILED: graph: database is locked")),
            "{lines:?}"
        );
    }

    fn report_with(status: kgraf_orchestrator::RunStatus) -> kgraf_orchestrator::RunReport {
        test_report(status)
    }

    /// A stopped run prints one `unfinished` line per item it left, after the
    /// held lines, so a reader sees what the stop cut off.
    #[test]
    fn a_stopped_run_prints_what_it_left_unfinished() {
        let mut report = report_with(kgraf_orchestrator::RunStatus::Stopped);
        report.verified = vec!["c1".into()];
        report.unfinished = vec![
            ("c2".into(), "not verified: cut off by the stop".into()),
            (
                "gate:root".into(),
                "not run: it covers open contract(s) c2".into(),
            ),
        ];
        let lines = report_lines(&report);
        assert!(lines.iter().any(|l| l == "status     Stopped"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l == "unfinished c2: not verified: cut off by the stop"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "unfinished gate:root: not run: it covers open contract(s) c2"),
            "{lines:?}"
        );
    }

    /// Only a completed run exits zero; blocked and stopped runs are errors that
    /// name the run and, for a stop, how to resume it.
    #[test]
    fn only_a_completed_run_is_a_success() {
        use kgraf_orchestrator::RunStatus;
        assert!(ensure_completed(&report_with(RunStatus::Completed)).is_ok());
        let blocked = ensure_completed(&report_with(RunStatus::Blocked {
            reason: "needs the user".into(),
        }))
        .expect_err("blocked");
        assert!(blocked.to_string().contains("needs the user"), "{blocked}");
        let stopped = ensure_completed(&report_with(RunStatus::Stopped)).expect_err("stopped");
        assert!(stopped.to_string().contains("--resume r9"), "{stopped}");
        assert!(ensure_completed(&report_with(RunStatus::Running)).is_err());
    }

    struct SeedAsker;

    #[async_trait::async_trait]
    impl kgraf_interpret::Asker for SeedAsker {
        fn identity(&self) -> String {
            "ollama:llama3".into()
        }

        async fn ask(
            &self,
            _system: &str,
            _user: String,
        ) -> std::result::Result<kgraf_interpret::AskAnswer, kgraf_interpret::AskFailure> {
            Ok(kgraf_interpret::AskAnswer {
                text: serde_json::json!({"reading": "r", "work_items": [{"id": "a",
                    "title": "a", "instruction": "do", "acceptance": [{"text": "x"}]}]})
                .to_string(),
                call: None,
            })
        }
    }

    fn corpus_args(prices: PathBuf, approve: Option<String>) -> OrchestrateArgs {
        OrchestrateArgs {
            routes: vec!["ollama:llama3".into()],
            planner: None,
            model_routes: Vec::new(),
            instruction: None,
            resume: None,
            run: Some("r1".into()),
            prices,
            max_items: 4,
            max_agents: 8,
            steps: 5,
            max_checkpoints: kgraf_host::limits::DEFAULT_MAX_CHECKPOINTS,
            max_tokens: 256,
            window: 128_000,
            point: None,
            verify: "true".into(),
            interactive: false,
            gate_commands: Vec::new(),
            gate_timeout: Duration::from_secs(60),
            contract_checks: true,
            interface_review: false,
            worktrees: kgraf_host::WorktreeRetention::default(),
            sessions: true,
            check_timeout: Duration::from_secs(600),
            check_compile_timeout: Duration::from_secs(600),
            shell_timeout: None,
            planner_max_tokens: kgraf_host::limits::DEFAULT_PLANNER_MAX_TOKENS,
            retry_gate: None,
            waive_gate: None,
            gate_note: None,
            corpus: true,
            approve,
            accept_commands: Vec::new(),
            corrections: Vec::new(),
            interpret_budget: None,
            rate_limits: Vec::new(),
            routing: kgraf_orchestrator::RoutingPolicy::default(),
            tbench_bridge_socket: None,
            fetch_deps: kgraf_host::deps::RunFetch::Stored,
            permissions: Default::default(),
            approve_plan: false,
            warn_rule_conflicts: false,
            answer_wait_secs: None,
        }
    }

    /// Decision 0093: a completed run with an unverified contract prints it on
    /// its own line, after the status.
    #[test]
    fn the_printed_report_names_each_unverified_contract() {
        let mut report = test_report(kgraf_orchestrator::RunStatus::Completed);
        report.verified = vec!["c1".into()];
        report.unverified = vec![(
            "c1".into(),
            "the acceptance command `true` checks nothing".into(),
        )];
        let lines = report_lines(&report);
        assert!(
            lines
                .iter()
                .any(|l| l == "unverified c1: the acceptance command `true` checks nothing"),
            "{lines:?}"
        );
    }

    fn assert_nothing_created(root: &Path) {
        let refs = std::process::Command::new("git")
            .current_dir(root)
            .args(["for-each-ref", "--format=%(refname)", "refs/heads/kgraf"])
            .output()
            .expect("git");
        assert!(
            String::from_utf8_lossy(&refs.stdout).trim().is_empty(),
            "a branch was created: {}",
            String::from_utf8_lossy(&refs.stdout)
        );
        assert!(
            !root.join(".kgraf/worktrees").exists(),
            "a worktree base was created"
        );
    }

    /// Reading a corpus, or refusing an approval, creates no
    /// integration branch and no worktree. The reading is seeded with the brief the
    /// command builds and the planner's identity, so `run` reuses it and makes no
    /// model call (the route is local and needs no credential; nothing listens).
    #[test]
    fn a_corpus_read_or_a_refused_approval_creates_no_branch_or_worktree() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        host_git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "# Tool\n\nIt does a thing.\n").expect("write");
        host_git(&root, &["add", "."]);
        host_git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        let state = root.join(".kgraf");
        std::fs::create_dir_all(&state).expect("state");
        {
            let store = GraphStore::open(&state.join("graph.db"), &root).expect("graph");
            let mut request = kgraf_interpret::InterpretRequest::new("ollama:llama3");
            request.budget_tokens = ContextConfig::for_model(128_000)
                .expect("context")
                .worker_plan_target_tokens();
            request.limits = kgraf_interpret::PlanLimits {
                max_items: OrchestratorConfig::default().max_contracts_per_plan,
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(async {
                kgraf_interpret::prepare(&store, request)
                    .expect("prepare")
                    .ask(&SeedAsker)
                    .await
                    .expect("ask")
                    .finish(&store, &kgraf_interpret::NoTestRuns)
                    .expect("finish")
            });
        }
        let prices = dir.path().join("prices.json");
        std::fs::write(
            &prices,
            r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
        )
        .expect("prices");

        run(&root, corpus_args(prices.clone(), None)).expect("a read from the stored reading");
        assert_nothing_created(&root);
        let ledger = Ledger::open(&state.join("ledger.db")).expect("ledger");
        assert!(
            ledger.calls().expect("calls").is_empty(),
            "the read reused the stored reading: no model call"
        );
        // Decision 0064: the reading's one work item is a presumed node now, with
        // the model that read it on record; presuming made no call either.
        {
            let store = GraphStore::open(&state.join("graph.db"), &root).expect("graph");
            let views = kgraf_interpret::presumed::views(&store).expect("views");
            let item = views
                .iter()
                .find(|v| v.id == "presumed/reading/item/a")
                .unwrap_or_else(|| panic!("{views:#?}"));
            assert!(
                item.parser.starts_with("model ollama:llama3"),
                "{}",
                item.parser
            );
        }

        let err = run(
            &root,
            corpus_args(prices, Some("plan-0000000000000000".into())),
        )
        .expect_err("an unknown plan id is refused");
        assert!(err.to_string().contains("no stored reading"), "{err}");
        assert_nothing_created(&root);
        assert!(
            ledger.calls().expect("calls").is_empty(),
            "approval makes no call"
        );
    }

    fn host_git(dir: &Path, args: &[&str]) {
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
    }

    /// The command and the library build the same run from the same settings:
    /// the flags mapped by `run_config`, and a configuration file loaded by
    /// `RunConfig::load`, give one construction (routes, limiters, approval
    /// chain, caps, limits, gates). Both build on the real steps: a git
    /// repository, the route probes, the orchestrator. The routes are local
    /// (`ollama`), so no credential is needed and nothing is called.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_command_and_the_library_build_the_same_run() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        host_git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "# t\n").expect("write");
        host_git(&root, &["add", "."]);
        host_git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        let prices = dir.path().join("prices.json");
        std::fs::write(
            &prices,
            r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1},
               "qwen3": {"uncached_input": 2, "cache_read": 1, "cache_write": 1, "output": 2, "reasoning": 2}}"#,
        )
        .expect("prices");
        let mut args = corpus_args(prices.clone(), None);
        args.corpus = false;
        args.routes = vec!["ollama:llama3".into(), "ollama:qwen3".into()];
        args.planner = Some("ollama:qwen3".into());
        args.max_items = 3;
        args.max_agents = 5;
        args.steps = 7;
        args.max_tokens = 1_000;
        args.window = 64_000;
        args.point = Some(60);
        args.gate_commands = vec!["true".into()];
        args.gate_timeout = Duration::from_secs(90);
        args.check_timeout = Duration::from_secs(120);
        args.check_compile_timeout = Duration::from_secs(300);
        args.rate_limits = vec!["ollama=60/5@30000".into()];
        let from_flags = run_config(&root, &args);

        let file = dir.path().join("run.json");
        std::fs::write(
            &file,
            r#"{
                "routes": ["ollama:llama3", "ollama:qwen3"],
                "planner": "ollama:qwen3",
                "prices": "prices.json",
                "max_items": 3, "max_agents": 5, "steps": 7, "max_tokens": 1000,
                "window": 64000, "point": 60, "verify": "true",
                "gate_commands": ["true"], "gate_timeout": 90,
                "contract_checks": true, "check_timeout": 120, "check_compile_timeout": 300,
                "rate_limits": ["ollama=60/5@30000"]
            }"#,
        )
        .expect("config");
        let from_file = RunConfig::load(&root, &file).expect("load");
        assert_eq!(from_flags, from_file);

        // This test's `--rate-limit` on a
        // used route (`ollama`) really does exercise `kgraf-host`'s shared
        // rate-limit store, so it must point at its own tempdir, never
        // `Host::open`'s real per-user default.
        let store_path = dir.path().join("rate-limits.sqlite3");
        // A build dials each local route first. The dial goes to a listener
        // this test owns, so the result does not depend on a server on this
        // machine. The listener never answers; a connection is all the dial needs.
        let stub = std::net::TcpListener::bind("127.0.0.1:0").expect("stub listener");
        let stub_url = format!("http://{}/v1", stub.local_addr().expect("stub address"));
        let mut shapes = Vec::new();
        for config in [from_flags, from_file] {
            let host = Host::open_with_rate_limit_store(config, Some(&store_path))
                .expect("open")
                .with_local_endpoint("ollama", &stub_url)
                .expect("local endpoint");
            let prepared = host
                .prepare(RunId::new("same"), None, &mut |_| {})
                .await
                .expect("prepare");
            shapes.push(prepared.shape().clone());
        }
        assert_eq!(shapes[0], shapes[1]);
        let shape = &shapes[0];
        let limit = kgraf_provider::RateLimit::parse("60/5@30000").expect("limit");
        assert_eq!(shape.planner_rate_limit, Some(limit));
        assert_eq!(
            shape
                .routes
                .iter()
                .map(|r| (r.name.as_str(), r.rate_limit))
                .collect::<Vec<_>>(),
            vec![
                ("ollama:llama3", Some(limit)),
                ("ollama:qwen3", Some(limit))
            ]
        );
        assert!(
            shape
                .routes
                .iter()
                .all(|r| r.tools.iter().any(|t| t == "shell"))
        );
        assert!(
            shape
                .routes
                .iter()
                .all(|r| !r.approval.is_empty() && r.approval == shape.routes[0].approval),
            "one approval chain for every route"
        );
        assert_eq!(
            shape.limits,
            kgraf_orchestrator::Limits::new(3, 5).expect("limits")
        );
        assert_eq!(shape.step_budget, 7);
        assert_eq!(shape.gate_commands, vec!["true".to_owned()]);
        assert_eq!(shape.gate_timeout, Duration::from_secs(90));
        assert!(shape.contract_checks, "turned on in both, so on in both");
        assert_eq!(shape.check_timeout, Duration::from_secs(120));
        assert_eq!(shape.check_compile_timeout, Duration::from_secs(300));
        assert!(
            shape.routes.iter().all(|r| r
                .tools
                .iter()
                .any(|t| t == kgraf_orchestrator::CHALLENGE_TOOL)),
            "every route's workers can challenge a contract"
        );
        assert!(shape.routes.iter().all(|r| r.max_output_tokens == 1_000));
        assert_eq!(shape.integration, "kgraf/same");
    }

    /// The run says which quality gates it uses, or that it has none.
    #[test]
    fn the_gate_commands_are_reported() {
        assert_eq!(
            notice_line(&Notice::GateCommands(vec![
                "cargo test --workspace".into(),
                "python3 -m pytest".into()
            ])),
            "gates      cargo test --workspace; python3 -m pytest"
        );
        assert!(notice_line(&Notice::GateCommands(Vec::new())).contains("no quality gate"));
    }

    /// The rule-conflict warning prints as one `notice` line with the fixed
    /// wording.
    #[test]
    fn the_rule_conflict_warning_prints_as_one_notice_line() {
        let line = notice_line(&Notice::RuleConflict(
            kgraf_host::rule_conflicts::RuleConflict {
                command: "verify",
                program: "python3".into(),
            },
        ));
        assert_eq!(
            line,
            "notice     Your verify command runs python3 and your rules block python3 for \
             workers. kGraf will continue to block workers from accessing python3 and will run \
             your verify command directly, without AI."
        );
        assert!(!line.contains('\n'));
    }

    /// The notices print as the command always printed them.
    #[test]
    fn notices_print_as_before() {
        assert_eq!(
            notice_line(&Notice::NoWorkerConfig(PathBuf::from(
                "/p/.kgraf/worker.json"
            ))),
            "config     none (/p/.kgraf/worker.json is absent): no MCP servers"
        );
        assert_eq!(
            notice_line(&Notice::CleanedMcpHome(PathBuf::from("/h/w1"))),
            "cleaned    /h/w1 (a dead process's MCP home)"
        );
        assert_eq!(
            notice_line(&Notice::Stranded {
                path: PathBuf::from("/b/w"),
                name: "w".into()
            }),
            "stranded   /b/w: its owner ran on another machine or in another PID namespace \
             (or its host is unreadable); check, then run `kgraf clean w`"
        );
        let report = kgraf_mcp::RegistrationReport::default();
        assert_eq!(
            notice_line(&Notice::McpServer {
                route: "r:m".into(),
                server: "s".into(),
                report: report.clone()
            }),
            format!("mcp        r:m: s: {report:?}")
        );
    }
}
