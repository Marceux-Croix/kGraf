//! `kgraf`: command-line access to the code graph and the provider layer.
//!
//! The desktop shell does not exist yet. This binary exists so the
//! two components that do exist are usable and demonstrable now, rather than only
//! through their test suites. It is a thin surface over the libraries: `anyhow` at
//! this boundary, `thiserror` inside them, per the project's conventions.
//!
//! It is deliberately not a second interface. Once the graph view exists, the
//! standing rule against a second place to look applies to anything this
//! grows into, so it stays at the level of "run the indexer, ask the index a
//! question, read the ledger".

mod deps_cmd;
mod diag_cmd;
mod orchestrate_cmd;
mod permissions_cli;
mod plan_cmd;
mod schedule_cmd;
mod worker_cmd;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kgraf_graph::{GraphStore, Indexer, Pattern, cochange, slice};
use kgraf_provider::Ledger;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "kgraf",
    about = "Code graph and provider accounting for kGraf",
    version
)]
struct Cli {
    /// The project root. Defaults to the working directory.
    #[arg(long, short = 'C', global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

// Parsed once per process and matched immediately; the size of the largest
// variant (`Worker`) costs nothing worth boxing it for.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Index the working tree, or bring an existing index up to date.
    Index {
        /// Report what would change without writing an index.
        #[arg(long)]
        dry_run: bool,
        /// Walk into nested repositories and git worktrees (directories with
        /// their own `.git`, such as `.kgraf/worktrees/*`), which are skipped
        /// and named by default.
        #[arg(long)]
        include_nested: bool,
        /// Skip reading the roadmap documents into presumed work nodes, which
        /// every index otherwise does (see `kgraf plan`).
        #[arg(long)]
        no_presume: bool,
    },
    /// Presumed work nodes: the work a project's roadmap documents describe,
    /// in the graph before any of it is approved or built.
    ///
    /// With no flag, list every presumed node and its state, derived now from
    /// what its document claims and what the code holds. `kgraf index` reads the
    /// roadmap too, by default.
    Plan {
        /// Read the roadmap documents into presumed work nodes: ROADMAP, TODO,
        /// PLAN, MILESTONES, BACKLOG or TASKS (.md or .markdown, any case) in the
        /// root and docs/, or the ones named with --source. Deterministic: no
        /// model call. Re-running updates the same nodes; it never duplicates
        /// them, never deletes them, and never overwrites a node someone else
        /// changed or real work adopted. No roadmap found is an error.
        #[arg(long)]
        from_roadmap: bool,
        /// A roadmap document to read, relative to the root, instead of the
        /// discovered ones. Repeatable.
        #[arg(long = "source", requires = "from_roadmap")]
        sources: Vec<String>,
    },
    /// Resolve a symbol to its nodes.
    Symbol { name: String },
    /// Print a context slice for a symbol, sized to a token budget.
    Slice {
        name: String,
        /// Estimated-token ceiling for the slice.
        #[arg(long, default_value_t = 2000)]
        budget: u64,
    },
    /// Run a structural query.
    ///
    /// Examples:
    ///   kind=function name~^handle
    ///   kind=method -calls-> name=parse
    ///   kind=struct <-contains- kind=file path~^crates/
    Query {
        pattern: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Compute the co-change relation from version history.
    Cochange {
        /// How many commits of history to read.
        #[arg(long, default_value_t = 2000)]
        commits: u32,
        /// Commits touching more files than this contribute no pairs. A
        /// tractability bound, reported in the output, not an operating point.
        #[arg(long, default_value_t = 200)]
        oversize_cap: u32,
    },
    /// Files that historically change alongside a given path.
    Partners {
        path: String,
        /// Minimum co-change count. A read-time filter: nothing is thresholded at
        /// index time.
        #[arg(long, default_value_t = 1)]
        min_count: u32,
        /// Ignore pairs seen only in commits larger than this many files.
        #[arg(long)]
        max_commit_size: Option<u32>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Report recorded provider spend.
    Spend {
        /// A JSON price table: {"model-name": {"uncached_input": 3000000, ...}},
        /// rates in micro-dollars per million tokens.
        #[arg(long)]
        prices: Option<PathBuf>,
    },
    /// List calls whose spend is unaccounted for, or close one with `resolve`.
    Unreconciled {
        #[command(subcommand)]
        action: Option<UnreconciledAction>,
    },
    /// Induced demand: whether runs offered workflow templates started more
    /// work than runs without, with decomposition spend and spend per verified
    /// task for each. Reports directions only; claims no saving.
    Demand,
    /// Verify or redact the accounting ledger's hash chain.
    Ledger {
        #[command(subcommand)]
        action: LedgerAction,
    },
    /// Make one ledgered call to a provider route and print what came back.
    ///
    /// The credential is read from the route's own environment variable, for
    /// example KGRAF_KIMI_API_KEY for `kimi` and KGRAF_KIMI_CODE_API_KEY for
    /// `kimi-code`. Set it in your own shell; it is never read from a file here.
    Call {
        /// Catalog route id, as `kgraf providers` lists it.
        #[arg(long)]
        provider: String,
        /// Model id. The catalog lists none on purpose; name the one you mean.
        #[arg(long)]
        model: String,
        /// Output ceiling.
        #[arg(long, default_value_t = 256)]
        max_tokens: u32,
        /// Write the exchange as a JSON record to this path. The credential is
        /// never written.
        #[arg(long)]
        record: Option<PathBuf>,
        /// The prompt.
        #[arg(default_value = "Reply with the single word: pong")]
        prompt: String,
    },
    /// Gather instruction sets and skills from the agents you already use.
    Instructions {
        #[command(subcommand)]
        action: InstructionsAction,
    },
    /// Run one worker against a live provider route, in its own worktree.
    ///
    /// The repository is `-C`; the worker gets its own git worktree under
    /// `.kgraf/workspaces/<worker>-<n>`, and its work is committed through
    /// merge-back to `refs/kgraf/attempts/<worker>-<n>` when the run ends
    /// (never merged by the worker). The brief file holds the
    /// instruction, verbatim. Every model call and MCP exchange is ledgered (see
    /// `kgraf spend`). Events are the worker's transcript, stored in
    /// `--events` (default `.kgraf/worker-<id>.jsonl`), which `--resume`
    /// continues from after a cancel. MCP servers come from `--config` (default
    /// `.kgraf/worker.json`, section `mcp_servers`).
    Worker {
        /// Catalog route id, as `kgraf providers` lists it.
        #[arg(long)]
        provider: String,
        /// Model id.
        #[arg(long)]
        model: String,
        /// A text file holding the instruction.
        #[arg(long)]
        brief: PathBuf,
        /// A symbol whose graph slice joins the private context. Repeatable; needs
        /// `kgraf index` in the worktree first.
        #[arg(long = "slice")]
        slices: Vec<String>,
        /// Rounds in one block. A block is extended by another of the same size
        /// while the worker makes measured progress (decision 0096), at most
        /// --max-checkpoints blocks; a worker that stops making progress ends
        /// at the next block boundary with a final answer round. Default: the
        /// `rounds_per_worker` user setting, else 12.
        #[arg(long)]
        budget: Option<u32>,
        /// The most blocks of rounds the worker may be granted; 1 is a fixed
        /// round limit. Default: the `max_checkpoints` user setting, else 10.
        #[arg(long)]
        max_checkpoints: Option<u32>,
        /// Output ceiling per round, lowered to the model's supported limit.
        /// Default: the `worker_max_tokens` user setting, else 16,384.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// `complete` or `stream`. `stream` needs an adapter with streaming.
        #[arg(long, default_value = "complete")]
        mode: String,
        /// The model's context window in tokens (the effective window is the
        /// lesser of this and 100,000). Default: the `context_window` user
        /// setting, else 128,000.
        #[arg(long)]
        window: Option<u64>,
        /// Compaction point in percent (5 to 95); the worker hands back there.
        #[arg(long)]
        point: Option<u8>,
        /// Where to store the event transcript (a new run needs a new file).
        #[arg(long)]
        events: Option<PathBuf>,
        /// Resume from this event transcript (and append to it), in a worktree at
        /// the worker's branch.
        #[arg(long)]
        resume: Option<PathBuf>,
        /// The worker configuration (MCP servers, hard-link read allowances).
        /// Defaults to `.kgraf/worker.json`, which may be absent.
        #[arg(long)]
        config: Option<PathBuf>,
        /// The commit a new worker's worktree starts from.
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// The branch merge-back reads attributes from (its tip's tree, fixed
        /// at start). Defaults to the checked-out branch. The worker never
        /// merges into it; that is the orchestrator's decision.
        #[arg(long)]
        integration_branch: Option<String>,
        /// A warm build directory to seed the worktree's own `target/` from by
        /// copy-on-write clone. Defaults to `<repo>/target` when it exists.
        #[arg(long)]
        warm_target: Option<PathBuf>,
        /// Cancel the worker this many milliseconds after it starts.
        #[arg(long)]
        cancel_after_ms: Option<u64>,
        /// The ledger database. Defaults to `.kgraf/ledger.db` in the worktree.
        #[arg(long)]
        ledger: Option<PathBuf>,
        /// Worker id, for the ledger, the worktree and branch names, and the
        /// events file name.
        #[arg(long, default_value = "cli-worker")]
        worker: String,
        /// Task id, for the ledger.
        #[arg(long, default_value = "cli-task")]
        task: String,
        /// Seconds one worker shell command may run before its process group
        /// is killed (its output so far is kept). Overrides the worker
        /// configuration's `shell_timeout_secs`; without either, 600.
        #[arg(long)]
        shell_timeout: Option<u64>,
        /// A Unix socket a Python Harbor `BaseAgent` shim already listens
        /// on (Terminal-Bench runs). When set, every shell command this
        /// worker runs is routed into that shim's task container instead of
        /// this host's sandbox, and every `apply_patch` write is synced into
        /// the container before the call returns. Commands the policy
        /// forbids are still refused first. Absent by default.
        #[arg(long)]
        tbench_bridge_socket: Option<PathBuf>,
    },
    /// Run the orchestrator on an instruction, or resume a run.
    ///
    /// The instruction is decomposed into contracts, dispatched to workers across
    /// the routes (each in its own git worktree under `.kgraf/worktrees`),
    /// verified with `--verify`, and recorded so a killed run resumes with
    /// `--resume RUN`. Every model call is ledgered; the run's trace is written
    /// under `.kgraf/runs/`.
    Orchestrate {
        /// A worker route as `route:model` (for example `kimi:kimi-k3`).
        /// Repeatable; workers rotate across routes.
        #[arg(long = "route", required = true)]
        routes: Vec<String>,
        /// The orchestrator's own model as `route:model`. Defaults to the first
        /// `--route`.
        #[arg(long)]
        planner: Option<String>,
        /// A route of another model kind as `kind=route:model`, for example
        /// `embedding=openai:text-embedding-3-small`, `rerank=llama-cpp:bge-reranker`
        /// or `typed-decision=systemone:jev-latest` (decision 0089). Repeatable.
        /// It runs no worker; the orchestrator is told it by kind and workers may
        /// call its tool (`embed`, `rerank`, `decide`). Nothing calls it by default.
        #[arg(long = "model-route")]
        model_routes: Vec<String>,
        /// A text file holding the instruction, verbatim.
        #[arg(long, conflicts_with = "resume")]
        instruction: Option<PathBuf>,
        /// Resume this run from its record.
        #[arg(long)]
        resume: Option<String>,
        /// Run id for a new run (default `run-<unix seconds>`).
        #[arg(long)]
        run: Option<String>,
        /// A JSON price table covering every model used (see `spend --prices`).
        /// Required: spend is reserved before each dispatch.
        #[arg(long)]
        prices: PathBuf,
        /// Work items in flight at once.
        #[arg(long, default_value_t = 4)]
        max_items: usize,
        /// Live workers at once, parallel attempts included.
        #[arg(long, default_value_t = 8)]
        max_agents: usize,
        /// Rounds in one block per worker. A block is extended by another of
        /// the same size while the worker makes measured progress (decision
        /// 0096), at most --max-checkpoints blocks. Default: the
        /// `rounds_per_worker` user setting, else 30.
        #[arg(long)]
        steps: Option<u32>,
        /// The most blocks of rounds a worker may be granted; 1 is a fixed
        /// round limit. Default: the `max_checkpoints` user setting, else 10.
        #[arg(long)]
        max_checkpoints: Option<u32>,
        /// Output ceiling per worker round, lowered to each model's supported
        /// limit with a notice. Default: the `worker_max_tokens` user setting,
        /// else 16,384.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// The models' context window in tokens. Default: the `context_window`
        /// user setting, else 128,000.
        #[arg(long)]
        window: Option<u64>,
        /// Compaction point in percent (5 to 95).
        #[arg(long)]
        point: Option<u8>,
        /// The acceptance command run in each worker's worktree.
        #[arg(long, default_value = "cargo test --workspace")]
        verify: String,
        /// Read notes (and `?` status questions) from stdin while running.
        #[arg(long)]
        interactive: bool,
        /// A quality-gate validation command. Repeatable. Without one, the
        /// project's commands are discovered (Cargo, npm `test`, pyproject,
        /// a Makefile `test` target); a project with none gets no gate.
        #[arg(long = "gate-command")]
        gate_commands: Vec<String>,
        /// Seconds one gate command, or one run of the --verify command, may
        /// run before it is killed.
        #[arg(long, default_value_t = 1800)]
        gate_timeout: u64,
        /// Turn on interface contracts for this run: the planner writes them,
        /// each is checked in the sandbox before its first worker starts
        /// (stub-compile, vacuity against a stub and a reference, the
        /// handshake), and workers may challenge them. Experimental and off by
        /// default. Interface contracts carry whole files, so a run with them
        /// may need a higher --planner-max-tokens.
        #[arg(long)]
        contract_checks: bool,
        /// Cross-vendor review at the interface: before a producer attempt is
        /// counted verified, one review call goes to a model on a cross-vendor
        /// consumer's route (a consumer contract depends on the producer, and
        /// its route names a different vendor). A finding is advisory: recorded
        /// and reported, never a silent pass or block. Off by default.
        #[arg(long)]
        interface_review: bool,
        /// Keep every worker's worktree, with the build output in it, after
        /// its attempt is merged or discarded, so builds can be inspected or
        /// reused. Without it, a worktree nothing needs any more is deleted
        /// (decision 0088). A worktree a stopped or resumable run needs is
        /// kept either way; `kgraf clean` removes a kept one.
        #[arg(long)]
        keep_worktrees: bool,
        /// Make every agent an addressable session: registers
        /// `ask_user_question`, `spawn_subagent`, `send_message` and
        /// `request_connection` in every worker environment. On by default;
        /// give `--sessions false` to turn it off.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        sessions: bool,
        /// Seconds one contract-check test run may take.
        #[arg(long, default_value_t = 600)]
        check_timeout: u64,
        /// Seconds one contract-check compile may take.
        #[arg(long, default_value_t = 600)]
        check_compile_timeout: u64,
        /// Seconds one worker shell command may run before its process group
        /// is killed (its output so far is kept). Overrides the worker
        /// configuration's `shell_timeout_secs`; without either, 600.
        #[arg(long)]
        shell_timeout: Option<u64>,
        /// Output ceiling, in tokens, of each orchestrator call, lowered to the
        /// planner model's supported limit with a notice. An answer cut off at
        /// it fails, naming this flag. Default: the `planner_max_tokens` user
        /// setting, else 32,768.
        #[arg(long)]
        planner_max_tokens: Option<u32>,
        /// Seconds a worker's question waits for the orchestrator's model to
        /// answer it, and a connection judgment may run (30 to 1800). A
        /// later answer still reaches the worker at its next round. Default:
        /// the `answer_wait_secs` user setting, else derived per call from
        /// the planner model's recent latencies (twice the 95th percentile,
        /// 120 to 900 s; 300 s with too little history).
        #[arg(long, value_parser = clap::value_parser!(u32).range(30..=1800))]
        answer_wait: Option<u32>,
        /// With --resume: run this held or failed gate again, with a fresh
        /// remediation allowance. Needs --gate-note.
        #[arg(long, requires = "resume", requires = "gate_note")]
        retry_gate: Option<String>,
        /// With --resume: waive this held or failed gate. Needs --gate-note.
        #[arg(
            long,
            requires = "resume",
            requires = "gate_note",
            conflicts_with = "retry_gate"
        )]
        waive_gate: Option<String>,
        /// Why the gate is retried or waived; recorded with the retry or waiver.
        #[arg(long)]
        gate_note: Option<String>,
        /// Start from the repository's own documents and code: read the
        /// corpus, print the plan read from it with its plan id, and stop. Nothing
        /// is created in the repository by a read. With `--instruction`, the
        /// instruction leads; in an empty directory it is required (greenfield).
        /// Doneness is derived from the code, and finished work is not planned
        /// again.
        #[arg(long, conflicts_with = "resume")]
        corpus: bool,
        /// With --corpus: approve the stored reading with this plan id (the id the
        /// reading printed) and run it. No model call is made; an id with no
        /// stored reading is refused.
        #[arg(long, requires = "corpus", value_name = "PLAN_ID")]
        approve: Option<String>,
        /// With --approve: accept this proposed validation command as a gate, by
        /// name. Repeatable. A proposed command not named stays proposed.
        #[arg(long = "accept-command", requires = "approve")]
        accept_commands: Vec<String>,
        /// With --corpus: a correction to the previous reading, verbatim.
        /// Repeatable, in order; the corpus is read again with them.
        #[arg(long = "correction", requires = "corpus")]
        corrections: Vec<String>,
        /// With --corpus: the interpretation call's input bound in tokens
        /// (default: 40% of the effective window `--window` gives; 40,000 at
        /// the default).
        #[arg(long, requires = "corpus")]
        interpret_budget: Option<u64>,
        /// A provider call rate limit as `ROUTE=RPM[/BURST][@MAX_WAIT_MS]` (for
        /// example `kimi=60/5@30000`). Repeatable, one per route; shared by the
        /// planner and every worker on that route. A call that would wait longer
        /// than MAX_WAIT_MS (default 60000) fails loudly instead.
        #[arg(long = "rate-limit")]
        rate_limits: Vec<String>,
        /// Vendor/model routing policy. `rotation` (default) rotates workers
        /// across the routes; `fitness` routes coupled
        /// contracts to one vendor and picks each worker's model by its
        /// measured cost per satisfied task for its kind.
        #[arg(long, default_value = "rotation")]
        routing: kgraf_orchestrator::RoutingPolicy,
        /// A Unix socket a Python Harbor `BaseAgent` shim already listens
        /// on (Terminal-Bench runs). Every attempt's shell commands are then
        /// routed into that shim's task container instead of the local
        /// sandbox, and its file writes are synced into the container. The
        /// container has one filesystem, so a run with this set must also
        /// pass `--max-agents 1` to keep contracts serialized. Absent by
        /// default.
        #[arg(long)]
        tbench_bridge_socket: Option<PathBuf>,
        /// Fetch the project's dependencies from crates.io for this run, if
        /// they are not in the local cache, without asking (decision 0087).
        /// Runs `cargo fetch` on this machine, outside the workers' sandbox.
        /// Stores nothing; `kgraf deps allow` makes it the project's default.
        #[arg(long, conflicts_with = "no_fetch_deps")]
        fetch_deps: bool,
        /// Never fetch dependencies for this run: a run whose dependencies
        /// are not in the local cache stops before it starts.
        #[arg(long)]
        no_fetch_deps: bool,
        /// Warn at the start of the run when your verify or gate command runs
        /// a program your deny or ask rules block for workers. The command
        /// runs anyway. Overrides the `warn_rule_conflicts` user setting; off
        /// by default.
        #[arg(long, conflicts_with = "no_warn_rule_conflicts")]
        warn_rule_conflicts: bool,
        /// Never warn about a verify or gate command that your rules block
        /// for workers, whatever the user setting says.
        #[arg(long)]
        no_warn_rule_conflicts: bool,
        /// Who answers a permission request past the base policy (decision
        /// 0094): manual (you), accept-edits (file edits granted), auto (your
        /// allow set granted), orchestrator-decides (the orchestrator grants
        /// or refuses, never asks), or bypass (everything granted except your
        /// deny and ask rules and deleting system paths). Overrides the
        /// user-level permissions file. With nobody at the terminal, what
        /// would ask is denied.
        #[arg(long, value_name = "MODE", value_parser = parse_mode)]
        permission_mode: Option<kgraf_tools::PermissionMode>,
        /// A shell program (an exact argv[0], for example `python3`) the Auto
        /// permission mode grants without asking. Repeatable. Given without
        /// `--permission-mode`, the run is in Auto. `--may-grant` is its older
        /// name.
        #[arg(long = "allow", visible_alias = "may-grant", value_name = "PROGRAM")]
        allow: Vec<String>,
        /// A repository path (exact, or a prefix ending in `*`) a contract's
        /// write scope may be widened to in the Auto permission mode.
        #[arg(long = "allow-path", value_name = "PATH")]
        allow_path: Vec<String>,
        /// A rule that blocks matching calls in every permission mode, Bypass
        /// included: `TOOL` or `TOOL(PATTERN)`, for example `shell(curl*)`.
        #[arg(long = "deny", value_name = "RULE")]
        deny: Vec<String>,
        /// A rule that asks about matching calls in every permission mode;
        /// with nobody to answer, they are denied.
        #[arg(long = "ask", value_name = "RULE")]
        ask: Vec<String>,
        /// The user-level permissions file to read instead of the default
        /// (`permissions.toml` in kGraf's per-user configuration directory).
        #[arg(long, value_name = "FILE")]
        permissions_file: Option<PathBuf>,
        /// Show the plan and wait for your yes before any worker starts. With
        /// nobody to approve, the run is refused unless `--approve-plan`.
        #[arg(long)]
        plan_review: bool,
        /// With plan review on, approve the plan without showing it.
        #[arg(long)]
        approve_plan: bool,
        /// Shell constructs a mode that grants programs may also grant inside
        /// a `sh -c` script, comma-separated: variables, subshells,
        /// background, input-redirects, escapes, outside-redirects; or
        /// `none`. Replaces the `script_constructs` list of the user-level
        /// permissions file for this run; all are off by default. With one
        /// on, a script using it is granted when every program kGraf can see
        /// in it is granted: kGraf cannot see what a variable or a
        /// substitution runs. Your deny and ask rules still apply.
        #[arg(long, value_name = "LIST", value_parser = parse_constructs)]
        script_constructs: Option<kgraf_tools::permissions::ScriptConstructs>,
        /// The Bypass permission mode for a run whose commands execute in a
        /// disposable container. Also requires `KGRAF_CONTAINER_SANDBOX=1` in
        /// the environment. Never use it on a machine you care about.
        #[arg(long)]
        container_sandbox: bool,
    },
    /// Always allow an MCP tool (`mcp__<server>__<tool>`) in this project, for
    /// every worker, attended or not; recorded in `.kgraf/allow-rules.json`.
    Allow {
        tool: String,
        /// Why, kept with the rule.
        #[arg(long, default_value = "allowed with `kgraf allow`")]
        note: String,
        /// Remove the rule instead.
        #[arg(long)]
        revoke: bool,
    },
    /// Remove a kept or stranded worktree (a kept one stays by the retention
    /// setting; a stranded one's owner was on another machine or in another
    /// PID namespace, so nothing removes it on its own). Looked up under both
    /// `.kgraf/worktrees` (runs) and `.kgraf/workspaces` (`kgraf worker`).
    /// Refused while its owner runs here or any process started in it runs.
    Clean {
        /// The worktree's name, or `worktrees/<name>` / `workspaces/<name>`
        /// when both directories hold one by that name.
        worktree: String,
        /// Remove it even though a run record still names it for a resume
        /// (the resume then starts that worker over).
        #[arg(long)]
        force: bool,
    },
    /// List the provider routes kGraf can reach.
    Providers {
        /// Show only routes whose usage shape has been verified.
        #[arg(long)]
        verified_only: bool,
    },
    /// Manage scheduled runs. Each schedule is a property of a work node in the
    /// graph (`--node`, or a `schedules/<id>` node made from the instruction),
    /// and every run is recorded on that node.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Local-only diagnostics: usage counts and errors in one capped, per-user
    /// file. Nothing is ever sent anywhere; you can export the file and share it
    /// by hand if you choose.
    Diag {
        #[command(subcommand)]
        action: DiagAction,
    },
    /// Whether kGraf may fetch this project's dependencies (decision 0087).
    /// Workers build with the network off, so a run needs its dependencies in
    /// the local cache before it starts. Run `kgraf deps allow` once per
    /// project, or `cargo fetch` yourself.
    Deps {
        #[command(subcommand)]
        action: DepsAction,
    },
}

#[derive(Subcommand)]
enum DiagAction {
    /// Print a human-readable summary of the diagnostics file.
    Show,
    /// Write a shareable, re-scrubbed copy of the diagnostics file.
    Export {
        /// Where to write it. Defaults to `kgraf-diag-export-<ts>.jsonl` in
        /// the current directory.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ScheduleAction {
    /// Add a schedule. Give exactly one of `--cron` or `--at`.
    Add {
        /// A unique schedule id.
        id: String,
        /// A text file holding the instruction, verbatim.
        #[arg(long)]
        instruction: PathBuf,
        /// An explicit IANA zone (for example `America/New_York`). Refused if
        /// unknown; never defaulted to the machine's local zone.
        #[arg(long)]
        zone: String,
        /// A recurring cron pattern (seconds-first, per `croner`), e.g. `0 0 3 * * *`
        /// for daily at 03:00 in `--zone`.
        #[arg(long, conflicts_with = "at")]
        cron: Option<String>,
        /// A one-shot instant, RFC 3339 (e.g. `2026-09-27T03:00:00Z`).
        #[arg(long, conflicts_with = "cron")]
        at: Option<String>,
        /// `skip` drops every occurrence missed while the app was closed; `run-once`
        /// (the default) fires one catch-up run representing all of them.
        #[arg(long, default_value = "run-once")]
        missed_policy: String,
        /// The work node this schedule belongs to. Omit it to create a node from
        /// the instruction, `schedules/<id>`.
        #[arg(long)]
        node: Option<String>,
    },
    /// List stored schedules, their work nodes, their last run and claim-table
    /// status.
    List {
        /// A JSON price table: with it, spend is shown in dollars, settled calls only, unreconciled
        /// shown apart; without it, raw token counts are shown, matching how `kgraf spend` already
        /// treats cost as optional.
        #[arg(long)]
        prices: Option<PathBuf>,
    },
    /// Remove a schedule from its work node and clear its claim-table bookkeeping.
    /// The node keeps the schedule's run history.
    Remove { id: String },
    /// Clear a claim-table disable (a readiness failure, or too many consecutive
    /// transient failures) so the schedule resumes firing.
    Enable { id: String },
    /// Run a stored schedule's instruction immediately, through the same live path
    /// `kgraf orchestrate` uses (readiness, admission and spend caps all apply).
    RunNow {
        id: String,
        /// A worker route as `route:model`. Repeatable.
        #[arg(long = "route", required = true)]
        routes: Vec<String>,
        /// The orchestrator's own model as `route:model`. Defaults to the first
        /// `--route`.
        #[arg(long)]
        planner: Option<String>,
        /// A route of another model kind as `kind=route:model`, for example
        /// `embedding=openai:text-embedding-3-small`, `rerank=llama-cpp:bge-reranker`
        /// or `typed-decision=systemone:jev-latest` (decision 0089). Repeatable.
        /// It runs no worker; the orchestrator is told it by kind and workers may
        /// call its tool (`embed`, `rerank`, `decide`). Nothing calls it by default.
        #[arg(long = "model-route")]
        model_routes: Vec<String>,
        /// A JSON price table covering every model used.
        #[arg(long)]
        prices: PathBuf,
        #[arg(long, default_value_t = 4)]
        max_items: usize,
        #[arg(long, default_value_t = 8)]
        max_agents: usize,
        /// Rounds in one block per worker. A block is extended by another of
        /// the same size while the worker makes measured progress (decision
        /// 0096). The block count limit is the `max_checkpoints` user setting,
        /// else 10. Default: the `rounds_per_worker` user setting, else 30.
        #[arg(long)]
        steps: Option<u32>,
        /// Output ceiling per worker round. Default: the `worker_max_tokens`
        /// user setting, else 16,384.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// The models' context window in tokens. Default: the `context_window`
        /// user setting, else 128,000.
        #[arg(long)]
        window: Option<u64>,
        /// The acceptance command run in each worker's worktree. Without it,
        /// the first test command the project's build files declare (Cargo,
        /// npm `test`, pyproject, a Makefile `test` target).
        #[arg(long)]
        verify: Option<String>,
        /// Run although the claim table has this schedule disabled.
        #[arg(long)]
        force: bool,
        /// Seconds one worker shell command may run before its process group
        /// is killed (its output so far is kept). Overrides the worker
        /// configuration's `shell_timeout_secs`; without either, 600.
        #[arg(long)]
        shell_timeout: Option<u64>,
        /// Fetch the project's dependencies from crates.io for this run, if
        /// they are not in the local cache, without asking (decision 0087).
        /// Runs `cargo fetch` on this machine, outside the workers' sandbox.
        /// Stores nothing; `kgraf deps allow` makes it the project's default.
        #[arg(long, conflicts_with = "no_fetch_deps")]
        fetch_deps: bool,
        /// Never fetch dependencies for this run: a run whose dependencies
        /// are not in the local cache stops before it starts.
        #[arg(long)]
        no_fetch_deps: bool,
        /// Warn at the start of the run when your verify or gate command runs
        /// a program your deny or ask rules block for workers. The command
        /// runs anyway. Overrides the `warn_rule_conflicts` user setting; off
        /// by default.
        #[arg(long, conflicts_with = "no_warn_rule_conflicts")]
        warn_rule_conflicts: bool,
        /// Never warn about a verify or gate command that your rules block
        /// for workers, whatever the user setting says.
        #[arg(long)]
        no_warn_rule_conflicts: bool,
        /// Keep every worker's worktree and its build output after use
        /// (decision 0088); see `kgraf orchestrate --keep-worktrees`.
        #[arg(long)]
        keep_worktrees: bool,
    },
}

/// `kgraf deps`: whether kGraf may fetch this project's dependencies
/// (decision 0087).
#[derive(Subcommand)]
enum DepsAction {
    /// Show the stored answer and whether the dependencies are ready offline.
    Status,
    /// Let kGraf fetch this project's dependencies from crates.io whenever a
    /// run needs them, without asking again. The one-time setup step.
    Allow,
    /// Never fetch; a run that needs a fetch stops before it starts.
    Refuse,
    /// Forget the answer: the next run that needs a fetch asks again.
    Reset,
    /// Fetch now, once (`cargo fetch` outside the workers' sandbox).
    Fetch,
}

#[derive(Subcommand)]
enum UnreconciledAction {
    /// Close one open call with the usage the provider's own invoice or console
    /// shows for it. Counts in the call's own protocol: for an inclusive route
    /// `--input` is the prompt total, for an additive route it excludes cache
    /// reads and writes. Use `--no-charge` when the provider confirms it billed
    /// nothing. A field left out keeps the partial figure already recorded for
    /// the call. A call still marked in flight may be live; resolving it needs
    /// `--abandoned`, which states that the process that made it is gone. A call is
    /// resolved once; the event is chained like any settle. There is no bulk form:
    /// each call needs the provider's own figure or a `--no-charge` statement.
    Resolve {
        /// The ledger call id, as printed by `kgraf unreconciled`.
        call_id: String,
        /// The provider confirms this call was not billed.
        #[arg(long, conflicts_with_all = ["input", "cache_read", "cache_write", "output", "reasoning"])]
        no_charge: bool,
        /// Input tokens, as the provider reports them for this route's protocol.
        #[arg(long)]
        input: Option<u64>,
        #[arg(long)]
        cache_read: Option<u64>,
        #[arg(long)]
        cache_write: Option<u64>,
        /// Text output tokens (reasoning excluded).
        #[arg(long)]
        output: Option<u64>,
        #[arg(long)]
        reasoning: Option<u64>,
        /// The call is still marked in flight and its process is gone. Without
        /// this flag an in-flight call is refused, because a live call would
        /// settle over the figure stated here.
        #[arg(long)]
        abandoned: bool,
    },
}

#[derive(Subcommand)]
enum LedgerAction {
    /// Walk the hash chain from genesis and report the first broken row, if any.
    /// Redacted rows are named even when the chain is intact.
    Verify,
    /// Remove a settled call's raw payload (a secret captured by accident,
    /// typically) and append a `redact` chain event naming why. `ledger verify`
    /// still passes afterward: the event is chained on its own content, not on
    /// the payload being removed.
    Redact {
        /// The ledger call id, as printed by `kgraf unreconciled` or `kgraf
        /// call`.
        call_id: String,
        /// Why the payload is being removed. Required and stored with the
        /// redaction event.
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
enum InstructionsAction {
    /// Discover known locations and imported folders, and rebuild the inventory.
    Scan,
    /// Add a folder holding instruction files or skills, then rebuild.
    Import { folder: PathBuf },
    /// Print the inventory: layers, skills, conflicts and problems.
    Show,
    /// Resolve a conflicting skill by choosing one variant's hash (a unique prefix
    /// is enough), then rebuild.
    Choose { skill: String, hash: String },
}

/// The name `kgraf diag show`'s "feature use" counts record for each
/// subcommand. A fixed, small vocabulary (never the parsed arguments): this
/// is a count of which command ran, not a log of how it was invoked.
fn feature_name(command: &Command) -> &'static str {
    match command {
        Command::Index { .. } => "index",
        Command::Plan { .. } => "plan",
        Command::Symbol { .. } => "symbol",
        Command::Slice { .. } => "slice",
        Command::Query { .. } => "query",
        Command::Cochange { .. } => "cochange",
        Command::Partners { .. } => "partners",
        Command::Spend { .. } => "spend",
        Command::Unreconciled { .. } => "unreconciled",
        Command::Demand => "demand",
        Command::Ledger { .. } => "ledger",
        Command::Call { .. } => "call",
        Command::Instructions { .. } => "instructions",
        Command::Worker { .. } => "worker",
        Command::Orchestrate { .. } => "orchestrate",
        Command::Allow { .. } => "allow",
        Command::Clean { .. } => "clean",
        Command::Providers { .. } => "providers",
        Command::Schedule { .. } => "schedule",
        Command::Diag { .. } => "diag",
        Command::Deps { .. } => "deps",
    }
}

/// This run's answer about fetching dependencies, from the two flags (clap
/// refuses both at once).
/// `--permission-mode`'s value (decision 0094).
fn parse_mode(s: &str) -> std::result::Result<kgraf_tools::PermissionMode, String> {
    s.parse()
}

/// `--script-constructs`' value (decision 0094, amended 2026-10-02).
fn parse_constructs(
    s: &str,
) -> std::result::Result<kgraf_tools::permissions::ScriptConstructs, String> {
    kgraf_tools::permissions::ScriptConstructs::parse_list(s)
}

/// The four run limits of one command: its flags, the user's settings file,
/// then the built-in defaults, with each value's source printed on stderr so
/// a run says where its limits came from. A settings file that is broken or
/// names a value outside the sliders' bounds stops the command.
fn resolve_limits(
    flags: kgraf_host::limits::UserLimits,
    rounds_default: u32,
    planner: bool,
) -> Result<kgraf_host::limits::ResolvedLimits> {
    let user = kgraf_host::limits::UserLimits::load_default()?;
    let resolved = kgraf_host::limits::ResolvedLimits::resolve(&flags, &user, rounds_default);
    for line in resolved.report(planner) {
        eprintln!("{line}");
    }
    Ok(resolved)
}

/// This run's answer about the rule-conflict warning, from its two flags
/// (clap refuses both at once): `None` leaves it to the user setting.
fn warn_choice(warn: bool, no_warn: bool) -> Option<bool> {
    match (warn, no_warn) {
        (true, _) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
    }
}

fn fetch_choice(fetch: bool, no_fetch: bool) -> kgraf_host::deps::RunFetch {
    use kgraf_host::deps::RunFetch;
    match (fetch, no_fetch) {
        (true, _) => RunFetch::Allow,
        (false, true) => RunFetch::Refuse,
        (false, false) => RunFetch::Stored,
    }
}

/// Prints the report `fn main() -> Result` prints and exits 1 for a failure;
/// an interrupted run exits with 128 plus its signal.
fn main() -> std::process::ExitCode {
    match run_cli() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:?}");
            match err.downcast_ref::<orchestrate_cmd::Interrupted>() {
                Some(interrupted) => std::process::ExitCode::from(interrupted.exit_code()),
                None => std::process::ExitCode::FAILURE,
            }
        }
    }
}

/// Finds Git Bash, or offers `winget` on a terminal. Without a terminal there
/// is nobody to ask, so the fix is the error.
#[cfg(windows)]
fn ensure_git_for_windows() -> Result<()> {
    use kgraf_sandbox::git_for_windows::{SystemProbe, ensure};
    use std::io::{BufRead, IsTerminal, Write};
    let outcome = ensure(&SystemProbe, |question| {
        if !std::io::stdin().is_terminal() {
            return false;
        }
        eprint!("{question}\n[y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).is_ok()
            && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    });
    outcome
        .into_result()
        .map(|_| ())
        .map_err(|text| anyhow::anyhow!(text))
}

fn run_cli() -> Result<()> {
    // First, so every later line (including `rmcp`'s) goes through the filter.
    kgraf_cli::logging::init()?;
    // A configured credential the scrubber refuses (a placeholder, a repeat)
    // is not scrubbed from transcripts: say so on stderr, whatever the log
    // filter (decision 0071). Never the value.
    kgraf_secrets::set_refusal_sink(Box::new(|notice| eprintln!("warning: {notice}")));
    // A rate-limiter mismatch, registration failure or stuck shared-store lock
    // (decision 0072 follow-up 1) is likewise said on stderr here; the shell
    // has its own sink that shows the same notices in its UI instead.
    kgraf_provider::set_notice_sink(Box::new(|notice| eprintln!("warning: {notice}")));
    let cli = Cli::parse();
    // The worker shell and git need Git for Windows. `diag` is how a problem
    // gets reported, so it runs without the check.
    #[cfg(windows)]
    if !matches!(cli.command, Command::Diag { .. }) {
        ensure_git_for_windows()?;
    }
    let root = match cli.root {
        Some(root) => root,
        None => std::env::current_dir().context("cannot determine the working directory")?,
    };
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    // Canonicalize: `--root .` (or any relative root) must
    // resolve to an absolute path before it is used to scrub diagnostics
    // text, since `scrub::scrub_path`'s `strip_prefix` only recognizes a path
    // as "inside the project" when both sides are in the same (absolute)
    // form. A root that fails to canonicalize (a race with something
    // removing it, a permission problem) still runs every command as before;
    // it only means diagnostics scrubbing falls back to hashing everything
    // under that root instead of showing it relative, which is safe, just
    // less readable.
    let root = root.canonicalize().unwrap_or(root);

    // Local-only diagnostics (decision 0069): usage counts
    // and errors in one capped, per-user file, shared with the desktop shell.
    // `resolve_path` honors `KGRAF_DIAG_PATH`/`KGRAF_DIAG_DISABLE`. A test run
    // without an override would write real lines into the user's actual
    // per-user file, so every test that runs this binary must
    // set one of them. `None` (disabled, or no resolvable per-user data
    // directory) just leaves diagnostics off for this run; it never fails the
    // command it would have instrumented (`kgraf_diag::record` is itself
    // always a safe no-op before `init`).
    if let Some(diag_path) = kgraf_diag::resolve_path() {
        kgraf_diag::init(diag_path, kgraf_diag::DEFAULT_CAP_BYTES);
    }
    // Held for the rest of `main`: its `Drop` flushes the writer on every
    // return from here, including an early `?`/`bail!` and a panic unwinding
    // through `main`.
    let _diag_shutdown = kgraf_diag::guard(DIAG_SHUTDOWN_TIMEOUT);

    // `kgraf diag show` must not record its own launch or feature use: doing
    // so would mean every `show` adds a line about itself to the very file it
    // just displayed.
    let showing_diag = matches!(
        &cli.command,
        Command::Diag {
            action: DiagAction::Show
        }
    );
    if !showing_diag {
        // One record, not a `Launch` and a `FeatureUsed` pair.
        kgraf_diag::record(kgraf_diag::DiagEvent::launch_with_feature(feature_name(
            &cli.command,
        )));
    }

    let result = match cli.command {
        Command::Index {
            dry_run,
            include_nested,
            no_presume,
        } => index(&root, dry_run, include_nested, no_presume),
        Command::Plan {
            from_roadmap,
            sources,
        } => {
            let store = open_graph_writer(&root)?;
            if from_roadmap {
                plan_cmd::from_roadmap(&store, &sources)
            } else {
                plan_cmd::list(&store)
            }
        }
        Command::Symbol { name } => symbol(&root, &name),
        Command::Slice { name, budget } => print_slice(&root, &name, budget),
        Command::Query { pattern, limit } => query(&root, &pattern, limit),
        Command::Cochange {
            commits,
            oversize_cap,
        } => build_cochange(&root, commits, oversize_cap),
        Command::Partners {
            path,
            min_count,
            max_commit_size,
            limit,
        } => partners(&root, &path, min_count, max_commit_size, limit),
        Command::Spend { prices } => spend(&root, prices.as_deref()),
        Command::Unreconciled { action: None } => unreconciled(&root),
        Command::Unreconciled {
            action:
                Some(UnreconciledAction::Resolve {
                    call_id,
                    no_charge,
                    input,
                    cache_read,
                    cache_write,
                    output,
                    reasoning,
                    abandoned,
                }),
        } => {
            let ledger = open_ledger(&root)?;
            let usage = ResolvedUsage {
                no_charge,
                input,
                cache_read,
                cache_write,
                output,
                reasoning,
                abandoned,
            };
            println!("{}", resolve_unreconciled(&ledger, &call_id, usage)?);
            Ok(())
        }
        Command::Demand => demand(&root),
        Command::Ledger { action } => ledger_command(&root, action),
        Command::Providers { verified_only } => providers(verified_only),
        Command::Allow { tool, note, revoke } => {
            let state = state_dir(&root)?;
            let project = root
                .canonicalize()
                .with_context(|| format!("cannot resolve {}", root.display()))?
                .to_string_lossy()
                .into_owned();
            if revoke {
                let n = kgraf_cli::worker::ask::revoke_mcp_tool(&state, &project, &tool)?;
                if n == 0 {
                    bail!("no rule allows {tool} in {project}");
                }
                println!("revoked    {tool} in {project} ({n} rule(s))");
                return Ok(());
            }
            let config_path = state.join("worker.json");
            let configured: Vec<String> = if config_path.exists() {
                kgraf_cli::worker::config::WorkerConfig::load(&config_path)?
                    .mcp_servers()
                    .iter()
                    .map(|s| s.name().to_owned())
                    .collect()
            } else {
                Vec::new()
            };
            let warning = kgraf_cli::worker::ask::allow_mcp_tool(
                &state,
                &project,
                &tool,
                &note,
                &configured,
            )?;
            if let Some(w) = warning {
                eprintln!("{w}");
            }
            println!("allowed    {tool} in {project}");
            Ok(())
        }
        Command::Clean { worktree, force } => {
            let removed = orchestrate_cmd::clean(&root, &worktree, force)?;
            println!("removed    {}", removed.display());
            Ok(())
        }
        Command::Orchestrate {
            routes,
            planner,
            model_routes,
            instruction,
            resume,
            run,
            prices,
            max_items,
            max_agents,
            steps,
            max_checkpoints,
            max_tokens,
            window,
            point,
            verify,
            interactive,
            gate_commands,
            gate_timeout,
            contract_checks,
            interface_review,
            keep_worktrees,
            sessions,
            check_timeout,
            check_compile_timeout,
            shell_timeout,
            planner_max_tokens,
            retry_gate,
            waive_gate,
            gate_note,
            corpus,
            approve,
            accept_commands,
            corrections,
            interpret_budget,
            rate_limits,
            routing,
            tbench_bridge_socket,
            fetch_deps,
            no_fetch_deps,
            permission_mode,
            allow,
            allow_path,
            deny,
            ask,
            permissions_file,
            plan_review,
            approve_plan,
            script_constructs,
            container_sandbox,
            warn_rule_conflicts,
            no_warn_rule_conflicts,
            answer_wait,
        } => {
            let limits = resolve_limits(
                kgraf_host::limits::UserLimits {
                    rounds: steps,
                    max_checkpoints,
                    worker_max_tokens: max_tokens,
                    planner_max_tokens,
                    window,
                    warn_rule_conflicts: warn_choice(warn_rule_conflicts, no_warn_rule_conflicts),
                    // `--point` is read on its own; this field is the desktop's.
                    compaction_point: None,
                    answer_wait_secs: answer_wait,
                },
                kgraf_host::limits::DEFAULT_ROUNDS_ORCHESTRATED,
                true,
            )?;
            orchestrate_cmd::run(
                &root,
                orchestrate_cmd::OrchestrateArgs {
                    routes,
                    planner,
                    model_routes,
                    instruction,
                    resume,
                    run,
                    prices,
                    max_items,
                    max_agents,
                    steps: limits.rounds.0,
                    max_checkpoints: limits.max_checkpoints.0,
                    max_tokens: limits.worker_max_tokens.0,
                    window: limits.window.0,
                    point,
                    verify,
                    interactive,
                    gate_commands,
                    gate_timeout: std::time::Duration::from_secs(gate_timeout),
                    contract_checks,
                    interface_review,
                    worktrees: kgraf_host::WorktreeRetention::from_delete(!keep_worktrees),
                    sessions,
                    check_timeout: std::time::Duration::from_secs(check_timeout),
                    check_compile_timeout: std::time::Duration::from_secs(check_compile_timeout),
                    shell_timeout: shell_timeout.map(std::time::Duration::from_secs),
                    planner_max_tokens: limits.planner_max_tokens.0,
                    retry_gate,
                    waive_gate,
                    gate_note,
                    corpus,
                    approve,
                    accept_commands,
                    corrections,
                    interpret_budget,
                    rate_limits,
                    routing,
                    tbench_bridge_socket,
                    fetch_deps: fetch_choice(fetch_deps, no_fetch_deps),
                    permissions: permissions_cli::PermissionArgs {
                        mode: permission_mode,
                        allow,
                        allow_paths: allow_path,
                        deny,
                        ask,
                        container_sandbox,
                        plan_review,
                        permissions_file,
                        script_constructs,
                    },
                    approve_plan,
                    warn_rule_conflicts: limits.warn_rule_conflicts.0,
                    answer_wait_secs: limits.answer_wait_secs.0,
                },
            )
        }
        Command::Instructions { action } => instructions(&root, action),
        Command::Worker {
            provider,
            model,
            brief,
            slices,
            budget,
            max_checkpoints,
            max_tokens,
            mode,
            window,
            point,
            events,
            resume,
            cancel_after_ms,
            ledger,
            config,
            base,
            integration_branch,
            warm_target,
            worker,
            task,
            shell_timeout,
            tbench_bridge_socket,
        } => {
            let limits = resolve_limits(
                kgraf_host::limits::UserLimits {
                    rounds: budget,
                    max_checkpoints,
                    worker_max_tokens: max_tokens,
                    planner_max_tokens: None,
                    window,
                    warn_rule_conflicts: None,
                    compaction_point: None,
                    answer_wait_secs: None,
                },
                kgraf_host::limits::DEFAULT_ROUNDS_SINGLE_WORKER,
                false,
            )?;
            worker_cmd::run(
                &root,
                worker_cmd::WorkerArgs {
                    provider,
                    model,
                    brief,
                    slices,
                    budget: limits.rounds.0,
                    max_checkpoints: limits.max_checkpoints.0,
                    max_tokens: limits.worker_max_tokens.0,
                    mode,
                    window: limits.window.0,
                    point,
                    events,
                    resume,
                    cancel_after_ms,
                    ledger,
                    config,
                    base,
                    integration_branch,
                    warm_target,
                    worker,
                    task,
                    shell_timeout: shell_timeout.map(std::time::Duration::from_secs),
                    tbench_bridge_socket,
                },
            )
        }
        Command::Schedule { action } => match action {
            ScheduleAction::Add {
                id,
                instruction,
                zone,
                cron,
                at,
                missed_policy,
                node,
            } => schedule_cmd::add(
                &root,
                schedule_cmd::AddArgs {
                    id,
                    instruction,
                    zone,
                    cron,
                    at,
                    missed_policy,
                    node,
                },
            ),
            ScheduleAction::List { prices } => schedule_cmd::list(&root, prices.as_deref()),
            ScheduleAction::Remove { id } => schedule_cmd::remove(&root, &id),
            ScheduleAction::Enable { id } => schedule_cmd::enable(&root, &id),
            ScheduleAction::RunNow {
                id,
                routes,
                planner,
                model_routes,
                prices,
                max_items,
                max_agents,
                steps,
                max_tokens,
                window,
                verify,
                force,
                shell_timeout,
                fetch_deps,
                no_fetch_deps,
                warn_rule_conflicts,
                no_warn_rule_conflicts,
                keep_worktrees,
            } => {
                let limits = resolve_limits(
                    kgraf_host::limits::UserLimits {
                        rounds: steps,
                        max_checkpoints: None,
                        worker_max_tokens: max_tokens,
                        planner_max_tokens: None,
                        window,
                        warn_rule_conflicts: warn_choice(
                            warn_rule_conflicts,
                            no_warn_rule_conflicts,
                        ),
                        compaction_point: None,
                        answer_wait_secs: None,
                    },
                    kgraf_host::limits::DEFAULT_ROUNDS_ORCHESTRATED,
                    true,
                )?;
                schedule_cmd::run_now(
                    &root,
                    schedule_cmd::RunNowArgs {
                        id,
                        routes,
                        planner,
                        model_routes,
                        prices,
                        max_items,
                        max_agents,
                        steps: limits.rounds.0,
                        max_checkpoints: limits.max_checkpoints.0,
                        max_tokens: limits.worker_max_tokens.0,
                        planner_max_tokens: limits.planner_max_tokens.0,
                        warn_rule_conflicts: limits.warn_rule_conflicts.0,
                        answer_wait_secs: limits.answer_wait_secs.0,
                        window: limits.window.0,
                        verify,
                        force,
                        shell_timeout: shell_timeout.map(std::time::Duration::from_secs),
                        fetch_deps: fetch_choice(fetch_deps, no_fetch_deps),
                        worktrees: kgraf_host::WorktreeRetention::from_delete(!keep_worktrees),
                    },
                )
            }
        },
        Command::Call {
            provider,
            model,
            max_tokens,
            record,
            prompt,
        } => call(
            &root,
            &provider,
            &model,
            max_tokens,
            record.as_deref(),
            &prompt,
        ),
        Command::Diag { action } => match action {
            DiagAction::Show => diag_cmd::show(),
            DiagAction::Export { out } => diag_cmd::export(out, &root),
        },
        Command::Deps { action } => match action {
            DepsAction::Status => deps_cmd::status(&root),
            DepsAction::Allow => deps_cmd::set(&root, deps_cmd::Consent::Allowed),
            DepsAction::Refuse => deps_cmd::set(&root, deps_cmd::Consent::Refused),
            DepsAction::Reset => deps_cmd::set(&root, deps_cmd::Consent::Unset),
            DepsAction::Fetch => deps_cmd::fetch(&root),
        },
    };

    if let Err(err) = &result {
        // `err:#` includes the chain (`source`s), which is what a diagnosis
        // actually needs; `scrub_text` removes any path in it the same way
        // `kgraf diag export` would.
        let (code, component) = diag_code_for(err);
        kgraf_diag::record(kgraf_diag::DiagEvent::error(
            code,
            component,
            kgraf_diag::scrub::scrub_text(&format!("{err:#}"), Some(&root)),
        ));
    }
    result
}

/// Seconds a return from `main` waits for the diagnostics writer to drain
/// before giving up: generous relative to a JSONL append, tiny relative to
/// how long a user waits for the process to actually exit either way.
const DIAG_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The most specific `KG-nnnn` code and component this `anyhow` chain names
/// (requirement 3, "the specific codes are never used"): walks every cause
/// looking for a type that implements [`kgraf_diag::DiagCode`], returning the
/// first (closest to the top) match. Falls back to this crate's own catch-all
/// when nothing in the chain is a type this binary knows how to downcast to
/// (a library error kind not yet given an `impl DiagCode`, or a chain made
/// entirely of `anyhow::anyhow!`/`.context()` strings, which carry no typed
/// cause at all).
fn diag_code_for(err: &anyhow::Error) -> (&'static str, &'static str) {
    use kgraf_diag::DiagCode;
    macro_rules! try_downcast {
        ($cause:expr, $($ty:ty),+ $(,)?) => {
            $(
                if let Some(e) = $cause.downcast_ref::<$ty>() {
                    return Some((e.diag_code(), e.diag_component()));
                }
            )+
        };
    }
    // The chain runs outermost (the top-level `anyhow::Error`) to innermost
    // (the root cause). The *innermost* match is the most specific one: an
    // `InterpretError::Ingest(#[from] IngestError)` is itself in this list
    // (its own `diag_code` has a catch-all for exactly this shape), so
    // stopping at the first match found in chain order returned that
    // catch-all before ever looking at the `IngestError` behind it. Walking the chain in reverse
    // finds the innermost matching type first instead.
    let downcast_one =
        |cause: &(dyn std::error::Error + 'static)| -> Option<(&'static str, &'static str)> {
            try_downcast!(
                cause,
                kgraf_host::Error,
                kgraf_host::NotReady,
                kgraf_orchestrator::OrchestratorError,
                kgraf_graph::GraphError,
                kgraf_provider::ProviderError,
                kgraf_sandbox::WorkspaceError,
                kgraf_sandbox::SandboxError,
                kgraf_schedule::ScheduleError,
                kgraf_interpret::InterpretError,
                kgraf_ingest::IngestError,
                kgraf_tools::ToolError,
            );
            None
        };
    let chain: Vec<&(dyn std::error::Error + 'static)> = err.chain().collect();
    for cause in chain.into_iter().rev() {
        if let Some(found) = downcast_one(cause) {
            return found;
        }
    }
    let spec = kgraf_diag::catch_all_for("kgraf-cli");
    (spec.code, spec.crate_name)
}

/// Print the vendor catalog.
///
/// The confidence column is the load-bearing one. The project's endpoint research
/// found three confirmed cases of a provider accepting a field and silently ignoring
/// it, all returning HTTP 200, so a route whose usage shape was never confirmed must
/// not be presented as though it had been.
fn providers(verified_only: bool) -> Result<()> {
    use kgraf_fleet::{CATALOG, Confidence};
    // Decision 0089: each route's model kinds, as `--route` (generative) and
    // `--model-route kind=route:model` (the others) take them.
    let kinds = |entry: &kgraf_fleet::Entry| {
        kgraf_fleet::kinds_served(entry)
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    println!(
        "{:<15} {:<26} {:<24} {:<10} {:<11} {:<12} KINDS",
        "ID", "VENDOR", "WIRE", "USAGE", "CONFIDENCE", "CACHE"
    );
    for entry in CATALOG {
        if verified_only && entry.confidence != Confidence::Verified {
            continue;
        }
        println!(
            "{:<15} {:<26} {:<24} {:<10} {:<11} {:<12} {}",
            entry.id,
            entry.vendor,
            entry.wire.as_str(),
            entry.protocol.as_str(),
            entry.confidence.as_str(),
            if entry.honors_cache_control {
                "breakpoints"
            } else {
                "-"
            },
            kinds(entry),
        );
    }
    println!();
    for entry in CATALOG {
        if verified_only && entry.confidence != Confidence::Verified {
            continue;
        }
        println!(
            "{} ({})\n  {}\n  {}",
            entry.id,
            entry.confidence.as_str(),
            entry.base_url,
            entry.note
        );
        if !entry.requires_credential {
            println!("  No credential required.");
        }
        println!();
    }
    println!(
        "No route lists model names: model identifiers change faster than a release, and\n\
         a stale list routes to a cheap tier that answers plausibly. Supply the models\n\
         you mean to use; an adapter refuses anything outside that set before the\n\
         request leaves.\n\
         \n\
         KINDS is what each route can serve. A generative route is a --route or the\n\
         --planner; any other kind is declared as --model-route kind=route:model. A\n\
         route lists a kind only where its vendor documents the endpoint, and calls it\n\
         with the route's own key. Verified: embedding on the openai route. Unverified:\n\
         rerank and typed-decision everywhere, and embedding on every other route."
    );
    Ok(())
}

/// One call through the full accounting path.
///
/// The ledger row is opened before the request leaves and settled from what came
/// back, exactly as a worker's call will be. A failure prints the ledger's
/// verdict on it too, because "failed" and "unreconciled" mean different things for
/// spend.
fn call(
    root: &Path,
    provider_id: &str,
    model: &str,
    max_tokens: u32,
    record: Option<&Path>,
    prompt: &str,
) -> Result<()> {
    use kgraf_provider::Provider as _;
    let entry = kgraf_fleet::entry(provider_id)?;
    let credential = kgraf_fleet::credential_from_env(entry)?;
    let provider = kgraf_provider::RouteRecorded::new(
        std::sync::Arc::from(kgraf_fleet::build(
            provider_id,
            &credential,
            vec![model.to_owned()],
        )?),
        provider_id,
    );
    let ledger = open_ledger(root)?;
    let request = kgraf_provider::Request::new(model, prompt, max_tokens);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let token = ledger
        .begin(
            provider.adapter(),
            provider.protocol(),
            model,
            "cli",
            "call",
        )
        .context("cannot open a ledger row")?;
    let call_id = token.id().clone();
    let outcome = runtime.block_on(provider.complete(&request, &token));
    drop(token);
    let row = ledger
        .call(&call_id)
        .context("cannot read the ledger row back")?;
    let settlement = row
        .as_ref()
        .map(|r| r.settlement.as_str())
        .unwrap_or("<missing>");

    println!("route      {} ({})", entry.id, entry.base_url);
    println!(
        "adapter    {} / {}",
        provider.adapter(),
        provider.protocol().as_str()
    );
    println!("ledger     {call_id} settled {settlement}");
    let recorded = match &outcome {
        Ok(response) => {
            let usage = &response.usage;
            println!("model      {}", response.model);
            println!(
                "request    {}",
                response.request_id.as_deref().unwrap_or("<none>")
            );
            println!("stop       {:?}", response.stop_reason);
            println!(
                "usage      in {} (cache r {} w {})  out {} (reasoning {})",
                usage.uncached_input,
                usage.cache_read,
                usage.cache_write,
                usage.text_output,
                usage.reasoning_output
            );
            let raw = usage
                .raw
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "<none>".into());
            println!("raw usage  {raw}");
            println!("\n{}", response.text());
            serde_json::json!({
                "route": entry.id,
                "base_url": entry.base_url,
                "adapter": provider.adapter(),
                "protocol": provider.protocol().as_str(),
                "requested_model": model,
                "returned_model": response.model,
                "request_id": response.request_id,
                "stop_reason": format!("{:?}", response.stop_reason),
                "usage": {
                    "uncached_input": usage.uncached_input,
                    "cache_read": usage.cache_read,
                    "cache_write": usage.cache_write,
                    "text_output": usage.text_output,
                    "reasoning_output": usage.reasoning_output,
                    "raw": usage.raw,
                },
                "text": response.text(),
                "settlement": settlement,
            })
        }
        Err(error) => {
            println!("error      {error}");
            serde_json::json!({
                "route": entry.id,
                "base_url": entry.base_url,
                "requested_model": model,
                "error": error.to_string(),
                "settlement": settlement,
            })
        }
    };
    if let Some(path) = record {
        let text = serde_json::to_string_pretty(&recorded).context("cannot encode the record")?;
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
        println!("\nrecorded to {}", path.display());
    }
    match outcome {
        Ok(_) => Ok(()),
        Err(error) => Err(anyhow::Error::new(error).context("the call failed")),
    }
}

/// The instructions and skills inventory.
///
/// Rebuilt from the sources on every change, never edited in place, so what it says
/// is always what the files say.
fn instructions(root: &Path, action: InstructionsAction) -> Result<()> {
    use kgraf_instructions::Config;
    let dir = state_dir(root)?;
    let config_path = dir.join("instruction-sources.json");
    let inventory_path = dir.join(kgraf_instructions::INVENTORY_FILE);
    let mut config = Config::load(&config_path)?;

    match action {
        InstructionsAction::Show => {
            // The error names the path and says whether the file could not be
            // read or is not a valid inventory.
            let inventory = kgraf_instructions::Inventory::load(&inventory_path)
                .context("cannot read the inventory")?
                .with_context(|| {
                    format!(
                        "no inventory at {}; run `kgraf instructions scan` first",
                        inventory_path.display()
                    )
                })?;
            print_inventory(&inventory, true);
            return Ok(());
        }
        InstructionsAction::Import { folder } => {
            let folder = folder
                .canonicalize()
                .with_context(|| format!("cannot open {}", folder.display()))?;
            if !folder.is_dir() {
                bail!("{} is not a folder", folder.display());
            }
            config.add_import(folder);
        }
        InstructionsAction::Choose { skill, hash } => {
            config.choices.insert(skill, hash);
        }
        InstructionsAction::Scan => {}
    }
    config.save(&config_path)?;

    let home = std::env::home_dir();
    let mut candidates = kgraf_instructions::discover(home.as_deref(), root);
    for folder in &config.imported {
        if folder.is_dir() {
            candidates.extend(kgraf_instructions::scan_folder(folder));
        } else {
            println!(
                "imported folder {} no longer exists; skipped",
                folder.display()
            );
        }
    }
    let inventory = kgraf_instructions::build(&candidates, &config.choices, home.as_deref());
    let text = serde_json::to_string_pretty(&inventory).context("cannot encode the inventory")?;
    kgraf_instructions::write_if_changed(&inventory_path, &text)?;
    print_inventory(&inventory, false);
    println!("\ninventory written to {}", inventory_path.display());
    Ok(())
}

fn print_inventory(inventory: &kgraf_instructions::Inventory, full: bool) {
    println!("instruction layers, in the order they apply:");
    for layer in &inventory.layers {
        let via = layer
            .imported_by
            .as_ref()
            .map(|p| format!("  (imported by {})", p.display()))
            .unwrap_or_default();
        println!(
            "  {:<8} {:<15} {}{via}",
            format!("{:?}", layer.source.scope).to_lowercase(),
            layer.source.agent,
            layer.source.path.display()
        );
        for other in &layer.also_at {
            println!("{:>26} identical at {}", "", other.display());
        }
    }
    println!("\nskills:");
    for skill in &inventory.skills {
        println!(
            "  {:<28} {}  ({} source{})",
            skill.name,
            &skill.hash[..12],
            skill.sources.len(),
            if skill.sources.len() == 1 { "" } else { "s" }
        );
        if full {
            println!("      {}", skill.description);
            if let Some(tools) = &skill.allowed_tools {
                println!("      allowed tools: {}", tools.join(", "));
            }
        }
    }
    if !inventory.conflicts.is_empty() {
        println!("\nconflicts (a skill is unusable until you choose):");
        for conflict in &inventory.conflicts {
            let state = match &conflict.chosen {
                Some(hash) => format!("chose {}", &hash[..12]),
                None => "UNRESOLVED".to_owned(),
            };
            println!("  {}  {state}", conflict.name);
            for variant in &conflict.variants {
                for source in &variant.sources {
                    println!("      {}  {}", &variant.hash[..12], source.path.display());
                }
            }
            if conflict.chosen.is_none() {
                println!(
                    "      resolve with: kgraf instructions choose {} <hash>",
                    conflict.name
                );
            }
        }
    }
    if !inventory.problems.is_empty() {
        println!("\nproblems:");
        for problem in &inventory.problems {
            println!("  {}: {}", problem.path.display(), problem.reason);
        }
    }
}

/// Where the project's own state lives: `kgraf_host::state_dir`, the one
/// definition.
fn state_dir(root: &Path) -> Result<PathBuf> {
    Ok(kgraf_host::state_dir(root)?)
}

/// The index for `kgraf index` alone: created when missing and rebuilt when its
/// schema version is another. Every other command opens an existing index and
/// refuses one it cannot use, naming `kgraf index`.
fn open_graph_for_index(root: &Path) -> Result<GraphStore> {
    let path = state_dir(root)?.join("graph.db");
    GraphStore::open(&path, root).with_context(|| format!("cannot open {}", path.display()))
}

/// An existing index, read-only: a reader can neither create nor rebuild it.
fn open_graph_read(root: &Path) -> Result<GraphStore> {
    let path = state_dir(root)?.join("graph.db");
    GraphStore::open_read_only_checked(&path, root)
        .with_context(|| format!("cannot open {}", path.display()))
}

/// An existing index that the command records facts in (schedules): never
/// created and never rebuilt.
fn open_graph_facts(root: &Path) -> Result<GraphStore> {
    let path = state_dir(root)?.join("graph.db");
    GraphStore::open_existing_for_facts(&path, root)
        .with_context(|| format!("cannot open {}", path.display()))
}

/// The index for a command that writes facts and may start a project's first
/// (`plan --from-roadmap`, `cochange`): an existing one is opened without a
/// rebuild, and a missing one is created empty at this build's schema, which
/// leaves nothing to rebuild.
fn open_graph_writer(root: &Path) -> Result<GraphStore> {
    let path = state_dir(root)?.join("graph.db");
    if path.is_file() {
        open_graph_facts(root)
    } else {
        open_graph_for_index(root)
    }
}

fn open_ledger(root: &Path) -> Result<Ledger> {
    let path = state_dir(root)?.join("ledger.db");
    Ledger::open(&path).with_context(|| format!("cannot open {}", path.display()))
}

fn index(root: &Path, dry_run: bool, include_nested: bool, no_presume: bool) -> Result<()> {
    let store = if dry_run {
        GraphStore::open_in_memory(root).context("cannot open a scratch index")?
    } else {
        open_graph_for_index(root)?
    };
    let report = Indexer::new(&store)
        .with_nested_repositories(include_nested)
        .index()
        .context("indexing failed")?;
    println!("files seen       {}", report.files_seen);
    for nested in &report.nested_repositories_skipped {
        println!("  skipped nested checkout {nested} (--include-nested walks it)");
    }
    for change in &report.extractor_changed {
        println!("  re-derived     {}", change.describe());
    }
    println!("  added          {}", report.files_added);
    println!("  changed        {}", report.files_changed);
    println!("  unchanged      {}", report.files_unchanged);
    println!("  removed        {}", report.files_removed);
    println!("  not utf-8      {}", report.skipped_not_utf8);
    for path in &report.non_utf8_removed {
        println!("    removed {path}: its bytes changed to something that is not UTF-8");
    }
    println!("  too large      {}", report.skipped_too_large);
    println!("  excluded dirs  {}", report.excluded_dirs_skipped.len());
    for dir in &report.excluded_dirs_skipped {
        println!("    skipped excluded directory {dir}");
    }
    for path in &report.metadata_errors {
        println!("  kept, metadata unreadable: {path} (its rows were not touched)");
    }
    for path in &report.symlinks_kept {
        println!("  kept, symbolic link: {path} (not followed; its rows were not touched)");
    }
    println!(
        "  left to the bus {} (conversation files, docs/agent-comms)",
        report.bus_owned_skipped
    );
    println!("nodes            {}", report.nodes);
    println!("edges            {}", report.edges);
    println!("lines            {}", report.lines);
    println!("calls unresolved {}", report.calls_unresolved);
    println!(
        "  other language {} (of those: defined only in another language family)",
        report.calls_cross_language
    );
    println!("calls ambiguous  {}", report.calls_too_ambiguous);
    println!("change-log rows  {}", report.change_log_rows);
    // Spin-up (decision 0064): the roadmap becomes presumed work nodes on every
    // index, so a fresh or half-built project shows what remains from the start.
    if no_presume {
        println!("presumed   skipped (--no-presume)");
    } else {
        let done = if dry_run {
            "the scratch index was built"
        } else {
            "the index was committed"
        };
        plan_cmd::presume_after_index(&store, done);
    }
    if dry_run {
        println!("(dry run: nothing was written; presumed nodes were read into a scratch index)");
    }
    Ok(())
}

fn symbol(root: &Path, name: &str) -> Result<()> {
    let store = open_graph_read(root)?;
    let found = store
        .resolve_symbol(name)
        .with_context(|| format!("resolving `{name}`"))?;
    // Every candidate is printed. Picking one silently is the failure the fail-loud rule forbids.
    for node in &found {
        println!(
            "{:<10} {:<40} {}:{}-{}",
            node.kind.as_str(),
            node.qualified_name,
            node.path,
            node.start_line,
            node.end_line
        );
    }
    if found.len() > 1 {
        println!("\n{} candidates; the name is ambiguous.", found.len());
    }
    Ok(())
}

fn print_slice(root: &Path, name: &str, budget: u64) -> Result<()> {
    let store = open_graph_read(root)?;
    if store.node_count().context("cannot read the graph")? == 0 {
        anyhow::bail!("the graph has no nodes yet; run `kgraf index` first");
    }
    let slice = slice::slice_for_symbol(&store, name, budget)
        .with_context(|| format!("slicing `{name}`"))?;
    print!("{}", slice.render());
    println!(
        "\n// {} of {} estimated tokens, {} member(s), {} omitted",
        slice.estimated_tokens(),
        budget,
        slice.members.len(),
        slice.omitted.len()
    );
    Ok(())
}

fn query(root: &Path, pattern: &str, limit: usize) -> Result<()> {
    let store = open_graph_read(root)?;
    let parsed = Pattern::parse(pattern).context("the query could not be parsed")?;
    let matches = parsed.run(&store, limit).context("the query failed")?;
    for found in &matches {
        match &found.via {
            Some(via) => println!(
                "{:<10} {:<36} {}:{}  -> {}",
                found.node.kind.as_str(),
                found.node.qualified_name,
                found.node.path,
                found.node.start_line,
                via.qualified_name
            ),
            None => println!(
                "{:<10} {:<36} {}:{}",
                found.node.kind.as_str(),
                found.node.qualified_name,
                found.node.path,
                found.node.start_line
            ),
        }
    }
    println!("\n{} match(es)", matches.len());
    Ok(())
}

fn build_cochange(root: &Path, commits: u32, oversize_cap: u32) -> Result<()> {
    let store = open_graph_writer(root)?;
    let history =
        cochange::read_git_history(root, commits).context("cannot read version history")?;
    let stats = cochange::build_from_commits(&store, &history, oversize_cap)
        .context("cannot build the co-change relation")?;
    let merged =
        cochange::merge_observed_writes(&store).context("cannot merge observed write sets")?;
    println!("commits read           {}", stats.commits);
    println!("  skipped oversized    {}", stats.commits_skipped_oversized);
    println!("pairs kept             {}", stats.pairs);
    println!("files in history       {}", stats.files_in_history);
    println!("files covered          {}", stats.files_covered);
    println!("coverage               {:.1}%", stats.coverage() * 100.0);
    println!("observed pairs merged  {merged}");
    println!(
        "collisions observed    {}",
        cochange::collision_count(&store).unwrap_or(0)
    );
    println!("\nEvery observed pair is kept with its count as a weight; filters are applied");
    println!("at read time by `kgraf partners`.");
    Ok(())
}

fn partners(
    root: &Path,
    path: &str,
    min_count: u32,
    max_commit_size: Option<u32>,
    limit: usize,
) -> Result<()> {
    let store = open_graph_read(root)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let pairs = cochange::partners_of(
        &store,
        path,
        cochange::CoChangeQuery {
            min_count,
            max_commit_size,
            max_age_seconds: None,
            limit,
        },
        now,
    )
    .context("cannot read the co-change relation")?;
    if pairs.is_empty() {
        println!("no recorded co-change for {path}");
        println!("(run `kgraf cochange` first if the relation has not been built)");
        return Ok(());
    }
    for pair in &pairs {
        let other = if pair.path_a == path {
            &pair.path_b
        } else {
            &pair.path_a
        };
        println!(
            "{:>5}  min-commit {:>4}  {}",
            pair.count, pair.min_commit_size, other
        );
    }
    Ok(())
}

fn demand(root: &Path) -> Result<()> {
    use kgraf_orchestrator::demand::{DemandRecord, render, summarize};
    let graph = open_graph_read(root)?;
    let records = DemandRecord::load_all(&graph).context("cannot read demand records")?;
    if records.is_empty() {
        println!("no finished orchestrator runs are recorded yet");
        return Ok(());
    }
    print!("{}", render(&summarize(&records)));
    Ok(())
}

/// Read a `--prices` file through the host's reader, which marks the routes priced
/// only under their route-scoped key. Every command that prices a ledger reads it
/// here, so none prices such a route by a bare model entry.
pub(crate) fn load_price_table(path: &Path) -> Result<kgraf_core::cost::PriceTable> {
    Ok(kgraf_host::config::read_prices(path)?)
}

/// The ledger's total under the price file at `path`, and the models with calls
/// it could not price.
fn cost_from_file(
    ledger: &kgraf_provider::Ledger,
    path: &Path,
) -> Result<(kgraf_core::cost::Money, Vec<String>)> {
    let table = load_price_table(path)?;
    ledger.total_cost(&table).context("cannot derive cost")
}

fn spend(root: &Path, prices: Option<&Path>) -> Result<()> {
    let ledger = open_ledger(root)?;
    let by_model = ledger.usage_by_model().context("cannot read the ledger")?;
    if by_model.is_empty() {
        println!("no recorded provider calls");
    }
    for (model, usage) in &by_model {
        println!(
            "{model:<28} in {:>9} (cache r {:>9} w {:>8})  out {:>8} (reasoning {:>7})",
            usage.uncached_input,
            usage.cache_read,
            usage.cache_write,
            usage.text_output,
            usage.reasoning_output
        );
    }
    if let Some(path) = prices {
        let (total, unpriced) = cost_from_file(&ledger, path)?;
        // A model that has a price but had calls refused above a rate tier is
        // partly costed: the total is incomplete, not unknown.
        let priced_models = by_model
            .iter()
            .filter(|(m, _)| !unpriced.contains(m))
            .count();
        println!(
            "\n{}",
            total_line(
                &total.to_string(),
                by_model.len(),
                priced_models,
                !unpriced.is_empty()
            )
        );
        if !unpriced.is_empty() {
            // Not costed at zero. Calls that could not be costed are reported so
            // the total is known to be incomplete.
            println!(
                "models with calls left out of the total (no price, or above a rate tier): {}",
                unpriced.join(", ")
            );
        }
    } else {
        println!("\n(pass --prices to derive cost; no price list is compiled in)");
    }

    let open = ledger.unreconciled().context("cannot read the ledger")?;
    if !open.is_empty() {
        println!(
            "\n{} call(s) with unaccounted spend; run `kgraf unreconciled`",
            open.len()
        );
    }
    Ok(())
}

/// The `kgraf spend` total line. When no recorded model has a price the
/// sum is not zero dollars, it is unknown, so the line says `unpriced` and
/// never a zero amount. When some calls were left out, the line says the
/// total is partial.
fn total_line(total: &str, models: usize, priced_models: usize, incomplete: bool) -> String {
    if models > 0 && priced_models == 0 {
        "total unpriced (no recorded model has a price)".to_owned()
    } else if incomplete {
        format!("total {total} (partial: some calls could not be costed)")
    } else {
        format!("total {total}")
    }
}

/// The state name a listing shows for an open call.
fn open_state(ledger: &Ledger, call: &kgraf_provider::CallRecord) -> Result<&'static str> {
    Ok(
        if ledger
            .is_in_flight(&call.id)
            .context("cannot read the ledger")?
        {
            "in_flight"
        } else {
            "unreconciled"
        },
    )
}

fn unreconciled(root: &Path) -> Result<()> {
    let ledger = open_ledger(root)?;
    let open = ledger.unreconciled().context("cannot read the ledger")?;
    if open.is_empty() {
        println!("every recorded call is accounted for");
        return Ok(());
    }
    for call in &open {
        // A record reads an in_flight row as unreconciled; the ledger is the
        // only place that tells the two apart, and `resolve` needs the difference.
        let state = open_state(&ledger, call)?;
        println!(
            "{}  {}  {}  agent {}  task {}  request {}",
            call.id,
            call.model,
            state,
            call.agent_id,
            call.task_id,
            call.request_id.as_deref().unwrap_or("<none>")
        );
        // The recorded figure is a lower bound (a partial stream, or zero).
        println!("    {}", usage_line(&call.usage));
    }
    println!(
        "\n{} call(s) billed without a recorded usage figure. These are holes in the \
         accounting, not zero-cost calls. The usage shown is a lower bound.",
        open.len()
    );
    println!(
        "Close one with `kgraf unreconciled resolve <call-id>`. There is no bulk \
         resolve: each call needs its own figure from the provider, or --no-charge. \
         A call shown as in_flight needs --abandoned."
    );
    Ok(())
}

/// One line of usage figures, in the normal form.
fn usage_line(usage: &kgraf_core::TokenUsage) -> String {
    format!(
        "usage (lower bound): input {} cache r {} w {} output {} reasoning {}",
        usage.uncached_input,
        usage.cache_read,
        usage.cache_write,
        usage.text_output,
        usage.reasoning_output
    )
}

/// The usage figures given to `kgraf unreconciled resolve`.
struct ResolvedUsage {
    abandoned: bool,
    no_charge: bool,
    input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
}

/// Close one open call with a stated usage, in the call's own protocol.
///
/// Refuses a call that is not open (a settled call is never rewritten) and a
/// request that states nothing: a resolve with no figure and no `--no-charge`
/// would record zero by omission, which is the silent understatement the open
/// row exists to prevent.
fn resolve_unreconciled(ledger: &Ledger, call_id: &str, given: ResolvedUsage) -> Result<String> {
    let stated = given.input.is_some()
        || given.cache_read.is_some()
        || given.cache_write.is_some()
        || given.output.is_some()
        || given.reasoning.is_some();
    if !given.no_charge && !stated {
        bail!("state the usage (--input, --output, ...) or pass --no-charge");
    }
    let open = ledger.unreconciled().context("cannot read the ledger")?;
    let call = open
        .iter()
        .find(|c| c.id.as_str() == call_id)
        .with_context(|| format!("`{call_id}` is not an open call; see `kgraf unreconciled`"))?;
    if ledger
        .is_in_flight(&call.id)
        .context("cannot read the ledger")?
        && !given.abandoned
    {
        bail!(
            "`{call_id}` is still in flight and may be live; a live call would settle over \
             a figure stated now. If its process is gone, repeat with --abandoned"
        );
    }
    // A field the caller leaves out keeps the figure already recorded (a partial
    // stream's lower bound), and the raw payload is kept, so a resolve that
    // states only the output does not zero the input. `--no-charge` states zero
    // for every field.
    let kept = &call.usage;
    let pick = |stated: Option<u64>, recorded: u64| {
        if given.no_charge {
            0
        } else {
            stated.unwrap_or(recorded)
        }
    };
    let read = pick(given.cache_read, kept.cache_read);
    let write = pick(given.cache_write, kept.cache_write);
    let reported_input = pick(
        given.input,
        match call.protocol {
            kgraf_core::Protocol::Additive => kept.uncached_input,
            kgraf_core::Protocol::Inclusive => kept.uncached_input + read + write,
        },
    );
    let usage = kgraf_core::TokenUsage::normalize(
        call.protocol,
        reported_input,
        read,
        write,
        pick(given.output, kept.text_output),
        pick(given.reasoning, kept.reasoning_output),
        kept.raw.clone(),
    )
    .context("the stated usage is inconsistent for this route's protocol")?;
    ledger
        .reconcile(&call.id, &usage)
        .with_context(|| format!("cannot resolve call `{call_id}`"))?;
    Ok(format!(
        "resolved {call_id} ({}): input {} cache r {} w {} output {} reasoning {}",
        call.model,
        usage.uncached_input,
        usage.cache_read,
        usage.cache_write,
        usage.text_output,
        usage.reasoning_output
    ))
}

fn ledger_command(root: &Path, action: LedgerAction) -> Result<()> {
    match action {
        LedgerAction::Verify => ledger_verify(root),
        LedgerAction::Redact { call_id, reason } => ledger_redact(root, &call_id, &reason),
    }
}

/// `kgraf ledger verify`: report whether the chain is intact, and name the
/// first broken row by sequence number rather than only saying "corrupted".
/// Exits non-zero on a broken chain, so this is usable in a script.
fn ledger_verify(root: &Path) -> Result<()> {
    let ledger = open_ledger(root)?;
    let report = ledger.verify().context("cannot walk the ledger chain")?;
    println!("rows checked   {}", report.rows_checked);
    if !report.redacted.is_empty() {
        println!(
            "redacted rows  {}",
            report
                .redacted
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    match &report.first_broken {
        None => {
            println!("chain intact");
            Ok(())
        }
        Some(broken) => {
            println!(
                "chain BROKEN at sequence {}: {}",
                broken.sequence, broken.reason
            );
            bail!(
                "ledger chain broken at sequence {}: {}",
                broken.sequence,
                broken.reason
            );
        }
    }
}

/// `kgraf ledger redact <call-id> --reason <why>`.
fn ledger_redact(root: &Path, call_id: &str, reason: &str) -> Result<()> {
    let ledger = open_ledger(root)?;
    let id = kgraf_core::CallId::new(call_id.to_owned());
    ledger
        .redact(&id, reason)
        .with_context(|| format!("cannot redact call `{call_id}`"))?;
    println!("redacted {call_id}: {reason}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A price file that names only a bare model, and a ledger with one call on
    /// the Azure route to a deployment named like that model.
    pub(crate) fn azure_call_with_bare_price_file(dir: &Path) -> (kgraf_provider::Ledger, PathBuf) {
        use kgraf_core::{Protocol, TokenUsage};
        let ledger = kgraf_provider::Ledger::open_in_memory().expect("ledger");
        {
            let token = ledger
                .begin("azure", Protocol::Additive, "gpt-4o", "a", "run-1")
                .expect("begin");
            token
                .record_route_async("azure-openai")
                .wait()
                .expect("route");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: 1_000_000,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
        }
        let path = dir.join("prices.json");
        std::fs::write(
            &path,
            r#"{"gpt-4o": {"uncached_input": 1000000, "cache_read": 0, "cache_write": 0, "output": 0, "reasoning": 0}}"#,
        )
        .expect("write");
        (ledger, path)
    }

    #[test]
    fn spend_never_prices_an_azure_deployment_by_a_bare_model_entry() {
        let dir = tempfile::tempdir().expect("dir");
        let (ledger, path) = azure_call_with_bare_price_file(dir.path());
        let (total, unpriced) = cost_from_file(&ledger, &path).expect("cost");
        assert_eq!(total.micros(), 0);
        assert_eq!(unpriced, vec!["gpt-4o".to_owned()]);
    }

    /// The most specific code in the chain wins: `InterpretError::Ingest(#[from] IngestError)` is
    /// itself in `diag_code_for`'s downcast list (it has its own catch-all, for a chain that
    /// carries an `InterpretError` with no `IngestError` behind it), so stopping at the first match
    /// found in `err.chain()`'s outer-to-inner order returned `InterpretError`'s catch-all before
    /// ever reaching the `IngestError` it wraps, even though `IngestError::TooLarge` has its own
    /// named code (`KG-1101`).
    #[test]
    fn the_innermost_matching_cause_wins_over_an_outer_wrapper() {
        let ingest = kgraf_ingest::IngestError::TooLarge {
            path: "x".into(),
            size: 10,
            limit: 5,
        };
        let interpret = kgraf_interpret::InterpretError::Ingest(ingest);
        let err = anyhow::Error::new(interpret);
        let (code, component) = diag_code_for(&err);
        assert_eq!(
            code, "KG-1101",
            "expected IngestError::TooLarge's own code, not InterpretError's catch-all"
        );
        assert_eq!(component, "kgraf-ingest");
    }

    /// Decision 0087: no flag leaves the stored consent in charge; each flag
    /// answers for one run; both at once are refused, on both commands.
    #[test]
    fn the_fetch_flags_answer_for_one_run_and_exclude_each_other() {
        use kgraf_host::deps::RunFetch;
        assert_eq!(fetch_choice(false, false), RunFetch::Stored);
        assert_eq!(fetch_choice(true, false), RunFetch::Allow);
        assert_eq!(fetch_choice(false, true), RunFetch::Refuse);
        let base = [
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
        ];
        let parse = |extra: &[&str]| {
            Cli::try_parse_from(base.iter().chain(extra.iter()).copied()).map(|cli| {
                match cli.command {
                    Command::Orchestrate {
                        fetch_deps,
                        no_fetch_deps,
                        ..
                    } => (fetch_deps, no_fetch_deps),
                    _ => panic!("not orchestrate"),
                }
            })
        };
        assert_eq!(parse(&[]).expect("parse"), (false, false));
        assert_eq!(parse(&["--fetch-deps"]).expect("parse"), (true, false));
        assert_eq!(parse(&["--no-fetch-deps"]).expect("parse"), (false, true));
        assert!(parse(&["--fetch-deps", "--no-fetch-deps"]).is_err());
        let run_now = Cli::try_parse_from([
            "kgraf",
            "schedule",
            "run-now",
            "s1",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
            "--fetch-deps",
            "--no-fetch-deps",
        ]);
        assert!(run_now.is_err(), "run-now refuses both too");
    }

    /// Decision 0088: worktrees are deleted after use unless `--keep-worktrees`
    /// is given, on both commands.
    #[test]
    fn keep_worktrees_is_a_flag_of_both_commands_and_off_by_default() {
        let kept = |args: &[&str]| {
            let cli = Cli::try_parse_from(args.iter().copied()).expect("parse");
            match cli.command {
                Command::Orchestrate { keep_worktrees, .. } => keep_worktrees,
                Command::Schedule {
                    action: ScheduleAction::RunNow { keep_worktrees, .. },
                } => keep_worktrees,
                _ => panic!("unexpected command"),
            }
        };
        let orch = [
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
        ];
        let now = [
            "kgraf",
            "schedule",
            "run-now",
            "s1",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
        ];
        for base in [&orch[..], &now[..]] {
            assert!(!kept(base), "deleting is the default");
            let with: Vec<&str> = base.iter().copied().chain(["--keep-worktrees"]).collect();
            assert!(kept(&with));
        }
    }

    /// Decision 0089: `--model-route` is repeatable on both run commands and
    /// absent by default.
    #[test]
    fn model_routes_are_taken_by_both_run_commands() {
        let orch = Cli::try_parse_from([
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
            "--model-route",
            "embedding=openai:text-embedding-3-small",
            "--model-route",
            "typed-decision=systemone:jev-latest",
        ])
        .expect("parse");
        let Command::Orchestrate { model_routes, .. } = orch.command else {
            panic!("not orchestrate");
        };
        assert_eq!(
            model_routes,
            vec![
                "embedding=openai:text-embedding-3-small",
                "typed-decision=systemone:jev-latest"
            ]
        );
        let now = Cli::try_parse_from([
            "kgraf",
            "schedule",
            "run-now",
            "s1",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
            "--model-route",
            "rerank=llama-cpp:r",
        ])
        .expect("parse");
        let Command::Schedule {
            action: ScheduleAction::RunNow { model_routes, .. },
        } = now.command
        else {
            panic!("not run-now");
        };
        assert_eq!(model_routes, vec!["rerank=llama-cpp:r"]);
        let plain = Cli::try_parse_from([
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
        ])
        .expect("parse");
        let Command::Orchestrate { model_routes, .. } = plain.command else {
            panic!("not orchestrate");
        };
        assert!(model_routes.is_empty());
    }

    /// `--script-constructs` takes names in either spelling or `none`, and
    /// refuses an unknown name; absent, the file decides.
    #[test]
    fn orchestrate_takes_a_script_constructs_list() {
        use kgraf_tools::permissions::{ScriptConstruct, ScriptConstructs};
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "kgraf",
                "orchestrate",
                "--route",
                "ollama:llama3",
                "--prices",
                "p.json",
            ];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args).map(|cli| match cli.command {
                Command::Orchestrate {
                    script_constructs, ..
                } => script_constructs,
                _ => panic!("not orchestrate"),
            })
        };
        assert_eq!(parse(&[]).expect("parse"), None);
        let on = parse(&["--script-constructs", "input-redirects,outside_redirects"])
            .expect("parse")
            .expect("given");
        assert!(on.allows(ScriptConstruct::InputRedirects));
        assert!(on.allows(ScriptConstruct::OutsideRedirects));
        assert_eq!(
            parse(&["--script-constructs", "none"]).expect("parse"),
            Some(ScriptConstructs::default())
        );
        assert!(parse(&["--script-constructs", "eval"]).is_err());
    }

    /// `kgraf orchestrate`'s defaults are the host's (`RunConfig::new`,
    /// `RunConfig::load`), so a run the shell builds from a file with only the
    /// required fields is the run the command builds from only the required flags.
    #[test]
    fn orchestrate_defaults_are_the_hosts() {
        let cli = Cli::try_parse_from([
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
        ])
        .expect("parse");
        let Command::Orchestrate {
            max_items,
            max_agents,
            steps,
            max_checkpoints,
            max_tokens,
            window,
            verify,
            gate_timeout,
            contract_checks,
            sessions,
            check_timeout,
            check_compile_timeout,
            planner_max_tokens,
            ..
        } = cli.command
        else {
            panic!("not orchestrate");
        };
        use kgraf_host::config::defaults as d;
        assert_eq!(contract_checks, d::contract_checks());
        assert!(
            !contract_checks,
            "off unless asked for (kept off by decision 0078)"
        );
        assert_eq!(sessions, d::sessions());
        assert!(
            sessions,
            "decision 0067: on since a live run showed all three together"
        );
        // The four run limits are unset flags: the user setting, then these
        // defaults, fill them (`resolve_limits`).
        assert_eq!(planner_max_tokens, None);
        assert_eq!(
            kgraf_host::limits::DEFAULT_PLANNER_MAX_TOKENS,
            d::planner_max_tokens()
        );
        assert_eq!(
            std::time::Duration::from_secs(check_timeout),
            d::check_timeout()
        );
        assert_eq!(
            std::time::Duration::from_secs(check_compile_timeout),
            d::check_compile_timeout()
        );
        assert_eq!(max_items, d::max_items());
        assert_eq!(max_agents, d::max_agents());
        assert_eq!((steps, max_tokens, window), (None, None, None));
        assert_eq!(max_checkpoints, None);
        assert_eq!(d::max_checkpoints(), 10);
        assert_eq!(d::steps(), 30);
        assert_eq!(d::max_tokens(), 16_384);
        assert_eq!(d::planner_max_tokens(), 32_768);
        assert_eq!(d::window(), 128_000);
        assert_eq!(verify, d::verify());
        assert_eq!(
            std::time::Duration::from_secs(gate_timeout),
            d::gate_timeout()
        );
    }

    /// A limit flag given on the command line reaches the resolver as a
    /// flag, which outranks the user setting and the default.
    #[test]
    fn a_limit_flag_is_a_flag_and_an_unset_one_is_not() {
        use kgraf_host::limits::{ResolvedLimits, Source, UserLimits};
        let cli = Cli::try_parse_from([
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
            "--steps",
            "77",
            "--max-checkpoints",
            "1",
            "--planner-max-tokens",
            "5000",
        ])
        .expect("parse");
        let Command::Orchestrate {
            steps,
            max_checkpoints,
            max_tokens,
            window,
            planner_max_tokens,
            ..
        } = cli.command
        else {
            panic!("not orchestrate");
        };
        let flags = UserLimits {
            rounds: steps,
            max_checkpoints,
            worker_max_tokens: max_tokens,
            planner_max_tokens,
            window,
            warn_rule_conflicts: None,
            compaction_point: None,
            answer_wait_secs: None,
        };
        let user = UserLimits {
            rounds: Some(40),
            max_checkpoints: Some(25),
            worker_max_tokens: Some(2_048),
            ..UserLimits::default()
        };
        let r = ResolvedLimits::resolve(&flags, &user, 30);
        assert_eq!(r.rounds, (77, Source::Flag));
        assert_eq!(r.max_checkpoints, (1, Source::Flag), "the flag outranks 25");
        assert_eq!(r.planner_max_tokens, (5_000, Source::Flag));
        assert_eq!(r.worker_max_tokens, (2_048, Source::Setting));
        assert_eq!(r.window, (128_000, Source::Default));
    }

    /// `kgraf worker --max-checkpoints` reaches the resolver as a flag, and
    /// without it the user setting, then the default, supplies the value.
    #[test]
    fn the_worker_checkpoint_flag_outranks_the_setting() {
        use kgraf_host::limits::{ResolvedLimits, Source, UserLimits};
        let parse = |extra: &[&str]| {
            let mut argv = vec![
                "kgraf",
                "worker",
                "--provider",
                "p",
                "--model",
                "m",
                "--brief",
                "b.txt",
            ];
            argv.extend_from_slice(extra);
            let Command::Worker {
                budget,
                max_checkpoints,
                ..
            } = Cli::try_parse_from(argv).expect("parse").command
            else {
                panic!("not worker");
            };
            UserLimits {
                rounds: budget,
                max_checkpoints,
                ..UserLimits::default()
            }
        };
        let user = UserLimits {
            max_checkpoints: Some(25),
            ..UserLimits::default()
        };
        let with = ResolvedLimits::resolve(&parse(&["--max-checkpoints", "2"]), &user, 12);
        assert_eq!(with.max_checkpoints, (2, Source::Flag));
        let without = ResolvedLimits::resolve(&parse(&[]), &user, 12);
        assert_eq!(without.max_checkpoints, (25, Source::Setting));
    }

    /// `--answer-wait` outranks the setting, no flag leaves the setting or the
    /// derived wait, and a value outside 30 to 1800 s is refused.
    #[test]
    fn the_answer_wait_flag_outranks_the_setting_and_is_bounded() {
        use kgraf_host::limits::{ResolvedLimits, Source, UserLimits};
        let base = [
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
        ];
        let parse = |extra: &[&str]| {
            Cli::try_parse_from(base.iter().chain(extra.iter()).copied()).map(|cli| {
                let Command::Orchestrate { answer_wait, .. } = cli.command else {
                    panic!("not orchestrate");
                };
                answer_wait
            })
        };
        let set = UserLimits {
            answer_wait_secs: Some(600),
            ..UserLimits::default()
        };
        let resolve = |flag: Option<u32>, user: &UserLimits| {
            let flags = UserLimits {
                answer_wait_secs: flag,
                ..UserLimits::default()
            };
            ResolvedLimits::resolve(&flags, user, 30).answer_wait_secs
        };
        assert_eq!(
            resolve(parse(&[]).expect("parse"), &UserLimits::default()),
            (None, Source::Default)
        );
        assert_eq!(
            resolve(parse(&[]).expect("parse"), &set),
            (Some(600), Source::Setting)
        );
        assert_eq!(
            resolve(parse(&["--answer-wait", "90"]).expect("parse"), &set),
            (Some(90), Source::Flag)
        );
        assert!(parse(&["--answer-wait", "10"]).is_err(), "below 30 s");
        assert!(parse(&["--answer-wait", "4000"]).is_err(), "above 1800 s");
    }

    /// The warning flags reach the resolver: no flag leaves it to the user
    /// setting, and that is off unless the user turned it on; either flag
    /// outranks the setting; both together are refused.
    #[test]
    fn the_rule_conflict_flags_outrank_the_setting_and_default_to_off() {
        use kgraf_host::limits::{ResolvedLimits, Source, UserLimits};
        let base = [
            "kgraf",
            "orchestrate",
            "--route",
            "ollama:llama3",
            "--prices",
            "p.json",
        ];
        let flags = |extra: &[&str]| {
            let Command::Orchestrate {
                warn_rule_conflicts,
                no_warn_rule_conflicts,
                ..
            } = Cli::try_parse_from(base.iter().chain(extra.iter()).copied())
                .expect("parse")
                .command
            else {
                panic!("not orchestrate");
            };
            UserLimits {
                warn_rule_conflicts: warn_choice(warn_rule_conflicts, no_warn_rule_conflicts),
                ..UserLimits::default()
            }
        };
        let on = UserLimits {
            warn_rule_conflicts: Some(true),
            ..UserLimits::default()
        };
        let resolve =
            |f: &UserLimits, u: &UserLimits| ResolvedLimits::resolve(f, u, 30).warn_rule_conflicts;
        assert_eq!(
            resolve(&flags(&[]), &UserLimits::default()),
            (false, Source::Default)
        );
        assert_eq!(resolve(&flags(&[]), &on), (true, Source::Setting));
        assert_eq!(
            resolve(&flags(&["--warn-rule-conflicts"]), &UserLimits::default()),
            (true, Source::Flag)
        );
        assert_eq!(
            resolve(&flags(&["--no-warn-rule-conflicts"]), &on),
            (false, Source::Flag)
        );
        assert!(
            Cli::try_parse_from(
                base.iter()
                    .copied()
                    .chain(["--warn-rule-conflicts", "--no-warn-rule-conflicts"])
            )
            .is_err()
        );
    }

    #[test]
    fn the_spend_total_says_unpriced_when_no_model_has_a_price() {
        assert_eq!(
            total_line("$0.000000", 2, 0, true),
            "total unpriced (no recorded model has a price)"
        );
        // Partly priced: the total stands, marked partial, and the models with
        // left-out calls are listed beneath it by the caller.
        assert_eq!(
            total_line("$1.500000", 2, 1, true),
            "total $1.500000 (partial: some calls could not be costed)"
        );
        // A priced model with calls refused above a rate tier is not unpriced.
        assert_eq!(
            total_line("$1.500000", 1, 1, true),
            "total $1.500000 (partial: some calls could not be costed)"
        );
        assert_eq!(total_line("$1.500000", 2, 2, false), "total $1.500000");
        assert_eq!(total_line("$0.000000", 0, 0, false), "total $0.000000");
    }

    fn open_call(ledger: &Ledger, protocol: kgraf_core::Protocol) -> String {
        let token = ledger
            .begin("t", protocol, "model-a", "agent", "task")
            .expect("begin");
        let id = token.id().as_str().to_owned();
        token.leave_unreconciled(None).expect("leave open");
        id
    }

    fn stated(input: Option<u64>, output: Option<u64>, no_charge: bool) -> ResolvedUsage {
        ResolvedUsage {
            abandoned: false,
            no_charge,
            input,
            cache_read: None,
            cache_write: None,
            output,
            reasoning: None,
        }
    }

    /// `kgraf unreconciled resolve` closes an open call with the stated usage,
    /// once, in the call's own protocol, and refuses an empty statement.
    #[test]
    fn resolve_closes_an_open_call_once_with_the_stated_usage() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = open_call(&ledger, kgraf_core::Protocol::Additive);
        assert_eq!(ledger.unreconciled().expect("open").len(), 1);

        // An id that is not open is refused while another call is open, and
        // the open call is untouched.
        assert!(resolve_unreconciled(&ledger, "nope", stated(Some(1), None, false)).is_err());
        assert_eq!(ledger.unreconciled().expect("open").len(), 1);

        // Nothing stated and no --no-charge: refused, the call stays open.
        assert!(resolve_unreconciled(&ledger, &id, stated(None, None, false)).is_err());
        assert_eq!(ledger.unreconciled().expect("open").len(), 1);

        let line = resolve_unreconciled(&ledger, &id, stated(Some(120), Some(30), false))
            .expect("resolve");
        assert!(line.contains("input 120"), "{line}");
        assert!(ledger.unreconciled().expect("open").is_empty());
        let record = ledger
            .call(&kgraf_core::CallId::new(id.clone()))
            .expect("read")
            .expect("present");
        assert_eq!(record.usage.uncached_input, 120);
        assert_eq!(record.usage.text_output, 30);
        ledger.verify().expect("chain still verifies");

        // A resolved call is no longer open, so a second resolve is refused.
        assert!(resolve_unreconciled(&ledger, &id, stated(Some(1), None, false)).is_err());
        // An unknown id is refused.
        assert!(resolve_unreconciled(&ledger, "nope", stated(None, None, true)).is_err());
    }

    /// A field the resolve leaves out keeps the partial figure recorded for the
    /// call, and the raw payload survives.
    #[test]
    fn resolve_keeps_unstated_fields_and_the_raw_payload() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(
                "t",
                kgraf_core::Protocol::Additive,
                "model-a",
                "agent",
                "task",
            )
            .expect("begin");
        let id = token.id().as_str().to_owned();
        let partial = kgraf_core::TokenUsage {
            uncached_input: 700,
            cache_read: 300,
            text_output: 5,
            raw: Some(serde_json::json!({"partial": true})),
            ..kgraf_core::TokenUsage::default()
        };
        token
            .leave_unreconciled_with(Some(&partial), None)
            .expect("leave open");
        resolve_unreconciled(&ledger, &id, stated(None, Some(40), false)).expect("resolve");
        let record = ledger
            .call(&kgraf_core::CallId::new(id))
            .expect("read")
            .expect("present");
        assert_eq!(record.usage.uncached_input, 700);
        assert_eq!(record.usage.cache_read, 300);
        assert_eq!(record.usage.text_output, 40);
        assert_eq!(record.usage.raw, partial.raw);
    }

    /// The listing tells a call that may be live from one that ended without
    /// usage, because `resolve` treats them differently.
    #[test]
    fn the_listing_names_an_in_flight_call_in_flight() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let ended = open_call(&ledger, kgraf_core::Protocol::Inclusive);
        let live = ledger
            .begin(
                "t",
                kgraf_core::Protocol::Inclusive,
                "model-a",
                "agent",
                "task",
            )
            .expect("begin");
        let rows = ledger.unreconciled().expect("open");
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let expected = if row.id.as_str() == ended {
                "unreconciled"
            } else {
                assert_eq!(row.id.as_str(), live.id().as_str());
                "in_flight"
            };
            assert_eq!(open_state(&ledger, row).expect("state"), expected);
        }
    }

    /// A call still in flight may be live: resolve refuses it without
    /// `--abandoned`, and a live settle cannot then overwrite the resolve.
    #[test]
    fn resolve_refuses_an_in_flight_call_unless_abandoned() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(
                "t",
                kgraf_core::Protocol::Additive,
                "model-a",
                "agent",
                "task",
            )
            .expect("begin");
        let id = token.id().as_str().to_owned();
        let err = resolve_unreconciled(&ledger, &id, stated(Some(5), Some(1), false))
            .expect_err("in flight is refused");
        assert!(err.to_string().contains("--abandoned"), "{err}");
        assert!(ledger.is_in_flight(token.id()).expect("state"));

        let abandoned = ResolvedUsage {
            abandoned: true,
            ..stated(Some(5), Some(1), false)
        };
        resolve_unreconciled(&ledger, &id, abandoned).expect("resolve abandoned");
        assert!(
            token
                .settle(&kgraf_core::TokenUsage::default(), None)
                .is_err(),
            "a late settle must not overwrite the resolved figure"
        );
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.usage.uncached_input, 5);
    }

    #[test]
    fn resolve_with_no_charge_records_zero_and_an_inclusive_underflow_is_refused() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let free = open_call(&ledger, kgraf_core::Protocol::Inclusive);
        resolve_unreconciled(&ledger, &free, stated(None, None, true)).expect("no charge");
        let record = ledger
            .call(&kgraf_core::CallId::new(free))
            .expect("read")
            .expect("present");
        assert_eq!(record.usage.total_input(), 0);

        let other = open_call(&ledger, kgraf_core::Protocol::Inclusive);
        let bad = ResolvedUsage {
            cache_read: Some(50),
            ..stated(Some(10), None, false)
        };
        assert!(resolve_unreconciled(&ledger, &other, bad).is_err());
        assert_eq!(ledger.unreconciled().expect("open").len(), 1);
    }
}
