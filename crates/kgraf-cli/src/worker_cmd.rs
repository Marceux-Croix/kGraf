//! `kgraf worker`: run one worker against a live provider route.
//!
//! This is the live acceptance path for the worker loop (an adapter is
//! verified against a running speaker, and the loop through it). What runs is
//! `kgraf_cli::worker`'s assembly, the same code the end-to-end test drives:
//!
//! - the worker works in **its own git worktree**, created under
//!   `.kgraf/workspaces/<worker>` by `kgraf-sandbox`'s `WorkspaceManager`,
//!   and its shell commands run in that worktree's sandbox;
//! - every model call and every MCP exchange goes through the project ledger;
//! - the events are the worker's durable transcript (`JsonlTranscript`), so a
//!   cancelled run resumes with `--resume <transcript>`;
//! - at the end of every run the worktree is committed through
//!   `kgraf-sandbox`'s hardened merge-back (`MergeBack::commit`) and left
//!   under `refs/kgraf/attempts/<worker>-<n>`, which a resumed run starts
//!   from; merging it is the orchestrator's decision, never the worker's;
//! - its build directory is its own (decision 0060), seeded from a warm one.

use anyhow::{Context, Result, bail};
use kgraf_cli::worker::ask::{Attendance, DEFAULT_PROMPT_TIMEOUT};
use kgraf_cli::worker::assemble::{Assembly, GraphParts, allow_store_path, state_read_denies};
use kgraf_cli::worker::config::WorkerConfig;
use kgraf_cli::worker::run::{CancelWhen, Owner, SegmentEnd, open_transcript, plan_segment};
use kgraf_cli::worker::segment::{Fault, SegmentOptions, conclude, run_segment};
use kgraf_cli::worker::workspace;
use kgraf_core::NodeId;
use kgraf_graph::GraphStore;
use kgraf_provider::Ledger;
use kgraf_runtime::{
    CallMode, ContextConfig, PointSource, WorkerBrief, WorkerEvent, WorkerOutcome,
};
use kgraf_sandbox::{MergeBack, WorkspaceManager};
use kgraf_session::approval_store::PersistedAllowStore;
use kgraf_tools::execpolicy::ExecPolicy;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct WorkerArgs {
    pub provider: String,
    pub model: String,
    pub brief: PathBuf,
    pub slices: Vec<String>,
    pub budget: u32,
    pub max_tokens: u32,
    /// The most progress checkpoints the worker may reach (decision 0096).
    pub max_checkpoints: u32,
    pub mode: String,
    pub window: u64,
    pub point: Option<u8>,
    pub events: Option<PathBuf>,
    pub resume: Option<PathBuf>,
    pub cancel_after_ms: Option<u64>,
    pub ledger: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub base: String,
    pub integration_branch: Option<String>,
    pub warm_target: Option<PathBuf>,
    pub worker: String,
    pub task: String,
    /// `--shell-timeout`: wins over the configuration's `shell_timeout_secs`
    /// (decision 0084).
    pub shell_timeout: Option<std::time::Duration>,
    /// A Unix socket a Python Harbor shim already listens on. When set,
    /// this worker's shell commands run inside the shim's task container
    /// instead of this host's sandbox (`ShellTool::with_external_executor`),
    /// and its `apply_patch` writes are synced into the container. `None` by
    /// default, so a plain `kgraf worker` run is unaffected.
    pub tbench_bridge_socket: Option<PathBuf>,
}

pub fn run(root: &Path, mut args: WorkerArgs) -> Result<()> {
    let instruction = std::fs::read_to_string(&args.brief)
        .with_context(|| format!("cannot read the brief {}", args.brief.display()))?;
    if instruction.trim().is_empty() {
        bail!("the brief {} is empty", args.brief.display());
    }
    if args.events.is_some() && args.resume.is_some() {
        bail!("--resume names the transcript to continue; do not also pass --events");
    }
    // An output cap above the model's supported limit is lowered, said on
    // stderr, never silently.
    let (max_tokens, clamp) = kgraf_host::limits::clamp_output(
        args.max_tokens,
        [(args.provider.as_str(), args.model.as_str())],
    );
    args.max_tokens = max_tokens;
    if let Some(c) = clamp {
        eprintln!("clamped    {}", c.sentence("worker"));
    }
    let entry = kgraf_fleet::entry(&args.provider)?;
    let credential = kgraf_fleet::credential_from_env(entry)?;
    let provider: Arc<dyn kgraf_provider::Provider> = Arc::new(kgraf_provider::RouteRecorded::new(
        Arc::from(kgraf_fleet::build(
            &args.provider,
            &credential,
            vec![args.model.clone()],
        )?),
        args.provider.clone(),
    ));
    let mode = match args.mode.as_str() {
        "stream" => CallMode::Stream,
        "complete" => CallMode::Complete,
        other => bail!("--mode must be stream or complete, not `{other}`"),
    };

    let state = crate::state_dir(root)?;
    let ledger_path = args
        .ledger
        .clone()
        .unwrap_or_else(|| state.join("ledger.db"));
    let ledger = Arc::new(
        Ledger::open(&ledger_path)
            .with_context(|| format!("cannot open {}", ledger_path.display()))?,
    );

    // The configuration: `--config` must exist; the default may be absent,
    // which is said, not assumed. It is unreadable to the worker's shell, like
    // the state directory (it can hold MCP stdio `env` secrets); see
    // `state_read_denies`.
    for removed in kgraf_cli::worker::assemble::clean_mcp_homes(&state.join("mcp-home"))? {
        println!(
            "cleaned    {} (a dead process's MCP home)",
            removed.display()
        );
    }
    let config = match &args.config {
        Some(path) => WorkerConfig::load(path)?,
        None => {
            let path = state.join("worker.json");
            if path.exists() {
                WorkerConfig::load(&path)?
            } else {
                println!(
                    "config     none ({} is absent): no MCP servers",
                    path.display()
                );
                WorkerConfig::default()
            }
        }
    };
    // Decision 0084: the flag wins over the file's `shell_timeout_secs`.
    let config = match args.shell_timeout {
        Some(timeout) if timeout.is_zero() => {
            bail!(
                "--shell-timeout must be at least one second: every shell command would be killed"
            )
        }
        Some(timeout) => config.with_shell_timeout(timeout),
        None => config,
    };

    // The graph, when this repository has been indexed. Without one the worker
    // still runs, with file tools only, and says so; a slice asked for without
    // a graph is refused, never dropped.
    let graph_path = state.join("graph.db");
    let graph = if graph_path.exists() {
        // Never rebuilt from here: only `kgraf index` rebuilds an index.
        let store = GraphStore::open_existing_for_facts(&graph_path, root)
            .context("cannot open the graph")?;
        (store.node_count()? > 0).then_some(store)
    } else {
        None
    };
    let (graph, slice_nodes) = match graph {
        Some(store) => {
            let nodes = args
                .slices
                .iter()
                .map(|symbol| {
                    // An ambiguous name is refused with its candidates, never
                    // resolved to whichever node sorts first.
                    kgraf_graph::slice::resolve_unambiguous(&store, symbol)
                        .map(|n| NodeId::new(n.id))
                        .with_context(|| format!("resolving --slice `{symbol}`"))
                })
                .collect::<Result<Vec<_>>>()?;
            let tool = GraphStore::open_existing_for_facts(&graph_path, root)
                .context("cannot open the graph")?;
            (
                Some(GraphParts {
                    slices: store,
                    tool,
                }),
                nodes,
            )
        }
        None => {
            if !args.slices.is_empty() {
                bail!(
                    "--slice needs an indexed repository; run `kgraf -C {} index` first",
                    root.display()
                );
            }
            println!("note       no graph index in this repository; graph_slice is not offered");
            (None, Vec::new())
        }
    };
    let mut context = ContextConfig::for_model(args.window)?;
    if let Some(point) = args.point {
        context = context.with_point(point, PointSource::User)?;
    }
    let transcript_path = args
        .resume
        .clone()
        .or(args.events.clone())
        .unwrap_or_else(|| state.join(format!("worker-{}.jsonl", args.worker)));
    let owner = Owner::new(&args.worker, &args.task, &instruction);
    let resume = args.resume.is_some();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    // Everything from here runs with the runtime; `shutdown_background` at the
    // end, so a terminal read left blocked by a timed-out approval prompt
    // cannot hold the process open.
    let outcome = drive(DriveArgs {
        runtime: &runtime,
        root,
        state: &state,
        args: &args,
        instruction,
        owner,
        resume,
        transcript_path,
        ledger: ledger.clone(),
        provider,
        vendor: entry.vendor,
        route: (entry.id, entry.base_url),
        mode,
        config,
        graph,
        slice_nodes,
        context,
        ledger_path,
    });
    runtime.shutdown_background();
    let result = outcome?;

    println!("steps      {}", result.steps);
    // Decision 0096: the round budget is a checkpoint; each one reached, with
    // what it measured and decided, and the ceiling.
    if let Some(blocks) = &result.round_blocks {
        println!(
            "blocks     {} rounds each, at most {} checkpoints{}",
            blocks.block_rounds,
            blocks.ceiling,
            if blocks.extended() {
                "; ran past its round budget"
            } else {
                ""
            }
        );
        for checkpoint in &blocks.checkpoints {
            println!(
                "checkpoint {}",
                kgraf_secrets::scrub_persist(&checkpoint.render())
            );
        }
    }
    println!("prefix     {}", result.prefix_digest);
    for failure in &result.trace_failures {
        println!("trace      NOT RECORDED: {failure}");
    }
    for path in &result.changed_paths {
        println!("changed    {}", path.display());
    }
    for call in &result.calls {
        match ledger.call(call)? {
            Some(row) => println!(
                "call       {}  {}  in {} (cache r {} w {})  out {} (reasoning {})  ttft {}  parent {}",
                row.id,
                row.settlement.as_str(),
                row.usage.uncached_input,
                row.usage.cache_read,
                row.usage.cache_write,
                row.usage.text_output,
                row.usage.reasoning_output,
                row.trace
                    .ttft_ms
                    .map_or_else(|| "-".to_owned(), |ms| format!("{ms} ms")),
                row.trace.parent_call.as_ref().map_or("-", |p| p.as_str()),
            ),
            None => println!("call       {call}  <missing from the ledger>"),
        }
    }
    let task = args.task.as_str();
    for mcp in ledger.mcp_calls()? {
        if mcp.call.task_id.as_deref() == Some(task) {
            println!(
                "mcp call   {}  {}  {}{}  {} ms  {}  in {} B out {} B",
                mcp.id,
                mcp.call.server,
                mcp.call.kind,
                mcp.call
                    .tool
                    .as_deref()
                    .map_or_else(String::new, |t| format!(" {t}")),
                mcp.call.duration_ms,
                mcp.call.outcome,
                mcp.call.bytes_in,
                mcp.call.bytes_out
            );
        }
    }
    if !result.answer.is_empty() {
        println!("\nanswer\n{}", result.answer);
    }
    match result.outcome {
        WorkerOutcome::Completed | WorkerOutcome::Cancelled => Ok(()),
        other => bail!(
            "the worker did not complete: {}",
            serde_json::to_string(&other)?
        ),
    }
}

struct DriveArgs<'a> {
    runtime: &'a tokio::runtime::Runtime,
    root: &'a Path,
    state: &'a Path,
    args: &'a WorkerArgs,
    instruction: String,
    owner: Owner,
    resume: bool,
    transcript_path: PathBuf,
    ledger: Arc<Ledger>,
    provider: Arc<dyn kgraf_provider::Provider>,
    vendor: &'static str,
    route: (&'static str, &'static str),
    mode: CallMode,
    config: WorkerConfig,
    graph: Option<GraphParts>,
    slice_nodes: Vec<NodeId>,
    context: ContextConfig,
    ledger_path: PathBuf,
}

/// Validate the transcript, plan and create the segment, run it, and record
/// its end. Nothing is created before the transcript and the resume check
/// pass; once the worktree exists, the segment always ends
/// through `run_segment`, which commits or keeps it (item 1).
fn drive(d: DriveArgs<'_>) -> Result<kgraf_runtime::WorkerResult> {
    let DriveArgs {
        runtime,
        root,
        state,
        args,
        instruction,
        owner,
        resume,
        transcript_path,
        ledger,
        provider,
        vendor,
        route,
        mode,
        config,
        graph,
        slice_nodes,
        context,
        ledger_path,
    } = d;
    let mut opened = runtime.block_on(open_transcript(&transcript_path))?;
    // After the transcript exists, so it is resolvable and denied too.
    let read_denies = state_read_denies(
        state,
        args.config.as_deref(),
        &ledger_path,
        &transcript_path,
    )?;
    if let Some(torn) = &opened.repaired {
        println!(
            "repaired   cut a torn last line at byte {}: {:?}",
            torn.valid_prefix_len, torn.discarded
        );
    }
    let manager = Arc::new(
        WorkspaceManager::open(root, &state.join("workspaces"))
            .context("cannot open the worker workspaces")?,
    );
    let plan = plan_segment(
        &opened,
        &owner,
        resume,
        &args.base,
        &workspace::GitView {
            repo: root,
            manager: &manager,
            worker: &args.worker,
        },
    )?;
    if let Some((segment, end)) = &plan.recovered {
        println!("recovered  segment {segment} ended {end:?} (its end was not recorded)");
    }
    let integration = match &args.integration_branch {
        Some(branch) => branch.clone(),
        None => workspace::current_branch(root)?,
    };
    // Opened before the worker runs: its attribute source is fixed to the
    // integration tip now, never read from what the worker writes.
    let merge_back = MergeBack::open(
        manager.clone(),
        &integration,
        workspace::owner_identity(root)?,
    )
    .context("cannot open merge-back")?;
    // Decision 0060: the warm build directory is `--warm-target`, else the
    // repository's own `target/` when it has one; with neither, the worktree
    // builds cold, and that is said.
    let warm = args
        .warm_target
        .clone()
        .or_else(|| Some(root.join("target")).filter(|t| t.is_dir()));
    let segment = workspace::open(&manager, &merge_back, &plan, warm.as_deref())?;
    let ws = segment.workspace().clone();
    // Recorded before anything else can fail: a segment that starts is one a
    // resume knows about.
    if let Err(e) = runtime.block_on(opened.begin_segment(&owner, &plan)) {
        // Nothing ran; the (empty) worktree is still ended the normal way.
        let (end, _) = runtime.block_on(conclude(
            &manager,
            &merge_back,
            segment,
            &mut opened,
            "not started",
            Fault::None,
        ));
        return Err(e.context(format!(
            "the segment could not be recorded; it ended {end:?}"
        )));
    }

    println!(
        "route      {} ({}), adapter {}",
        route.0,
        route.1,
        provider.adapter()
    );
    println!("mode       {mode:?}");
    println!("worktree   {} (from {})", ws.root().display(), plan.start);
    match &segment.seeded {
        Some(report) => println!(
            "target     {} seeded by {:?} in {:?}",
            report.dest.display(),
            report.method,
            report.elapsed
        ),
        None => println!("target     not seeded (no warm build directory); builds start cold"),
    }
    println!("transcript {}", transcript_path.display());

    // The control the worker runs under, kept so the rules that name none of
    // its tools can be said once its MCP servers have listed theirs.
    let mut permissions = None;
    let prepared = (|| -> Result<Assembly> {
        let (allow_store, rules) = PersistedAllowStore::open(allow_store_path(state), ws.root())
            .context("cannot open the project's allow rules")?;
        println!("rules      {} persisted allow rule(s)", rules.rule_count);
        for rule in allow_store.inert_rules() {
            // A rule recorded under an earlier, wider check: kept in the file,
            // but it can never match, so the command is asked about again.
            println!(
                "notice     the allow rule {:?} for {} no longer applies: that command \
                 cannot be allowed always, so it is asked about each time",
                rule.scope, rule.tool
            );
        }
        let project = root
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", root.display()))?
            .to_string_lossy()
            .into_owned();
        // Connect to the Python shim's socket now, while `ws.root()` (the
        // worktree the shim must sync against) is already known, earlier
        // than any shell command runs. The shim refuses the session if the
        // worktree and the container differ.
        let external_executor: Option<Arc<dyn kgraf_tools::external::ExternalExecutor>> =
            match &args.tbench_bridge_socket {
                Some(socket) => {
                    let executor = runtime
                        .block_on(kgraf_tbench_bridge::ProcessExecutor::connect(
                            socket,
                            ws.root(),
                        ))
                        .with_context(|| {
                            format!(
                                "cannot connect to the tbench bridge socket {}",
                                socket.display()
                            )
                        })?;
                    Some(Arc::new(executor))
                }
                None => None,
            };
        Ok(Assembly {
            ledger: ledger.clone(),
            provider: provider.clone(),
            vendor: vendor.to_owned(),
            model: args.model.clone(),
            workspace: ws.clone(),
            checkout: root.to_path_buf(),
            graph,
            config,
            worker: args.worker.as_str().into(),
            task: args.task.as_str().into(),
            exec_policy: ExecPolicy::default_kgraf_policy(),
            allow_store,
            project,
            attendance: Attendance::detect(DEFAULT_PROMPT_TIMEOUT),
            call_mode: mode,
            max_output_tokens: args.max_tokens,
            max_checkpoints: args.max_checkpoints,
            path_case: kgraf_tools::detect_path_case(ws.root())
                .context("cannot probe the worktree's filesystem case behavior")?,
            read_denies,
            mcp_home: state.join("mcp-home"),
            extra_tools: Vec::new(),
            // `kgraf worker` has no orchestrator to bind a session line to
            // (decision 0067).
            session_line: None,
            prefixes: kgraf_runtime::PrefixRegistry::global(),
            // `kgraf worker` has no `Host` and so no shared `MachineGate` to
            // bind to; its shell commands stay unbounded (decision 0067).
            machine_gate: None,
            external_executor,
            dependency_refetch: None,
            // Decision 0053: the project's skills, from the inventory
            // `kgraf instructions` wrote; none for a Terminal-Bench bridge run.
            skills: kgraf_cli::worker::assemble::run_skills(
                state,
                args.tbench_bridge_socket.as_deref(),
            )
            .context("cannot read the project's skill inventory")?,
            permissions: permissions
                .insert(crate::permissions_cli::control(root, &Default::default())?)
                .clone(),
            protected_writes: None,
            refusals: None,
        })
    })();
    let assembly = match prepared {
        Ok(assembly) => assembly,
        Err(e) => {
            let (end, recorded) = runtime.block_on(conclude(
                &manager,
                &merge_back,
                segment,
                &mut opened,
                "not started",
                Fault::None,
            ));
            return Err(e.context(format!(
                "the segment ended {end:?}{}",
                recorded
                    .err()
                    .map_or(String::new(), |r| format!(" (not recorded: {r})"))
            )));
        }
    };
    let brief = WorkerBrief {
        task: args.task.as_str().into(),
        worker: args.worker.as_str().into(),
        instruction,
        workspace_root: ws.root().to_path_buf(),
        model: args.model.clone(),
        slices: slice_nodes,
        skills: vec![],
        step_budget: args.budget,
        context,
        write_scope: None,
    };
    let started = std::time::Instant::now();
    let report = runtime.block_on(run_segment(
        assembly,
        segment,
        &manager,
        &merge_back,
        &mut opened,
        brief,
        SegmentOptions {
            resume: plan.resume,
            cancel: args
                .cancel_after_ms
                .map(|ms| CancelWhen::After(std::time::Duration::from_millis(ms))),
            cancel_on_interrupt: true,
            message: format!("kgraf worker {} task {}", args.worker, args.task),
        },
        |assembled| {
            for (server, report) in &assembled.registrations {
                println!(
                    "mcp        {server}: {} tool(s) registered",
                    report.registered.len()
                );
                for (tool, why) in &report.refused {
                    println!("mcp        {server}: tool `{tool}` NOT registered: {why}");
                }
            }
            // Decision 0094, amended 2026-10-02: a rule naming an MCP tool is
            // checked once the servers have listed their tools.
            if let Some(control) = &permissions {
                let tools: Vec<String> = assembled.env.tools.names().map(str::to_owned).collect();
                for sentence in
                    kgraf_tools::permissions::unmatched_entries(&control.settings(), &tools)
                {
                    println!("notice     {sentence}");
                }
            }
            println!(
                "prefix     {} ({})",
                assembled.env.prefix.digest().as_str(),
                assembled.env.prefix.encoder()
            );
        },
        |event| show(event, started),
    ));

    println!();
    match &report.end {
        SegmentEnd::Committed {
            attempt_ref,
            commit,
        } => {
            println!("\nattempt    {commit} at {attempt_ref} (not merged; the worktree is removed)")
        }
        SegmentEnd::Empty => println!("\nattempt    none: the worker changed nothing"),
        SegmentEnd::Refused { reasons, kept } => {
            println!("\nattempt    REFUSED by merge-back: {reasons}; worktree kept at {kept}")
        }
        SegmentEnd::NotCarried { error, kept } => {
            println!("\nattempt    NOT COMMITTED: {error}; worktree kept at {kept}")
        }
    }
    if let Some(result) = &report.result {
        println!("outcome    {}", outcome_line(&result.outcome)?);
    }
    let mut errors = report.errors;
    match &report.end {
        SegmentEnd::Refused { .. } | SegmentEnd::NotCarried { .. } => errors.push(anyhow::anyhow!(
            "the segment's work was not committed; see above"
        )),
        _ => {}
    }
    if let Some(first) = errors.first() {
        for later in &errors[1..] {
            eprintln!("also: {later:#}");
        }
        bail!("{first:#}");
    }
    report
        .result
        .context("the segment ended without a result and without an error")
}

/// The outcome as printed: its JSON, persist-scrubbed (decision 0071). A failure's error can quote
/// a provider's reply and a hand-back carries the worker's own text; a terminal is a display like
/// any other.
fn outcome_line(outcome: &kgraf_runtime::WorkerOutcome) -> Result<String> {
    Ok(kgraf_secrets::scrub_persist_owned(serde_json::to_string(
        outcome,
    )?))
}

/// Show one event on the terminal.
fn show(event: &WorkerEvent, started: std::time::Instant) {
    match event {
        WorkerEvent::TextDelta { text } => print!("{text}"),
        WorkerEvent::ReasoningDelta { .. } => {}
        // The round's settled text already streamed live above, one
        // `TextDelta` at a time; printing it again here would duplicate it
        // on the terminal (decision 0071).
        WorkerEvent::AssistantMessage { .. } => {}
        WorkerEvent::ToolCallStarted { tool, input, .. } => {
            println!("\n[tool {tool} {input}]")
        }
        WorkerEvent::ToolCallFinished {
            output, is_error, ..
        } => {
            let first = output.lines().next().unwrap_or_default();
            let more = output.lines().count().saturating_sub(1);
            println!(
                "[{} {first}{}]",
                if *is_error { "error" } else { "result" },
                if more > 0 {
                    format!(" (+{more} lines)")
                } else {
                    String::new()
                }
            );
        }
        WorkerEvent::Usage { call, usage } => println!(
            "\n[usage {call}: in {} cache r {} w {} out {}]",
            usage.uncached_input, usage.cache_read, usage.cache_write, usage.text_output
        ),
        WorkerEvent::Stop { reason, .. } => println!("\n[stop {reason:?}]"),
        WorkerEvent::Cancelled { phase } => {
            println!(
                "\n[cancelled during {phase:?} after {:?}]",
                started.elapsed()
            )
        }
        WorkerEvent::Approval { .. } => {}
        WorkerEvent::Context {
            prefix_digest,
            slices,
            ..
        } => println!(
            "[context prefix {prefix_digest}, {} private slice(s)]",
            slices.len()
        ),
        WorkerEvent::Notice { text, .. } => println!("\n[notice {text}]"),
        WorkerEvent::ModelCall { call } => println!("\n[model call {call}]"),
        WorkerEvent::Injected { source, text, .. } => {
            println!("\n[injected from {source:?}: {text}]")
        }
        WorkerEvent::RoundAbandoned { phase, reason } => {
            println!("\n[round abandoned during {phase:?}: {reason}]")
        }
        WorkerEvent::Waiting { call_id, reason } => {
            println!("\n[{call_id} waiting: {reason}]")
        }
        WorkerEvent::Resumed { call_id } => println!("\n[{call_id} resumed]"),
        // Kinds and counts only (decision 0071).
        WorkerEvent::Redacted { counts } => println!("\n[redacted {counts:?}]"),
        // Decision 0095: the runtime's own notice, then the text the worker
        // continues from.
        WorkerEvent::Compacted {
            notice, summary, ..
        } => println!(
            "\n[compacted at {} of {} tokens; summary from {:?}]\n{}",
            notice.at_tokens,
            notice.window_tokens,
            notice.summary_source,
            kgraf_runtime::render_notice(notice, summary)
        ),
        // Decision 0096: the loop's own measurement at a round checkpoint.
        WorkerEvent::Checkpoint { checkpoint, .. } => println!("\n[{}]", checkpoint.render()),
    }
    std::io::stdout().flush().ok();
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    /// A failure quoting a token prints scrubbed.
    #[test]
    fn a_printed_outcome_is_scrubbed() {
        let token = format!("ghp_{}", "Ou1Tc2Om3Ep4Ri5Nt6Ab7Cd8Ef9Gh0Ij1Kl2");
        let outcome = kgraf_runtime::WorkerOutcome::Failed {
            error: format!("provider said: bad token {token}"),
        };
        let line = outcome_line(&outcome).expect("json");
        assert!(!line.contains(&token), "{line}");
        assert!(line.contains("[redacted:github-pat;"), "{line}");
    }
}
