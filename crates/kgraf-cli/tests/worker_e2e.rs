//! The worker end to end, on the real pieces (the mechanism, not
//! the setting). Only the model is fake: a provider implementing the real
//! `Provider` and `Streaming` traits, settling its `CallToken` as an adapter does.
//! Everything else is what `kgraf worker` runs:
//!
//! - a real git repository, and the worker's own worktree from
//!   `kgraf-sandbox`'s `WorkspaceManager`;
//! - `kgraf-tools`' built-ins: `graph_slice` over a real index, `read_file`,
//!   `list_directory`, `apply_patch`, and `shell` sandboxed through the
//!   `Workspace`, with its `ShellApprovalHook` in the chain and a persisted
//!   `Scope::Exact` allow rule answering its ask;
//! - `kgraf-session`'s `JsonlTranscript` on disk and its masking;
//! - a real MCP server over stdio (`mcp-echo-server`), its every exchange in
//!   the ledger through `LedgerAttributionSink`;
//! - the ledger's per-call trace fields, and its chain verified at the end.
//!
//! Each segment's work is committed through `kgraf-sandbox`'s hardened
//! merge-back and left under `refs/kgraf/attempts/<worker>-<n>`; the
//! integration branch never moves. The second test cancels a worker mid-stream
//! and resumes it from the durable transcript, in a fresh worktree at the
//! attempt the first segment committed. The fourth shows a worker-planted
//! `.gitattributes` refused by merge-back rather than read by host git.

use kgraf_cli::worker::ask::Attendance;
use kgraf_cli::worker::assemble::{Assembled, Assembly, GraphParts, allow_store_path, assemble};
use kgraf_cli::worker::config::WorkerConfig;
use kgraf_cli::worker::run::{
    CancelWhen, OpenTranscript, Owner, Record, SegmentEnd, open_transcript, plan_segment,
    run_worker,
};
use kgraf_cli::worker::segment::{Fault, SegmentOptions, conclude, run_segment};
use kgraf_cli::worker::workspace::{self, Segment};
use kgraf_core::{NodeId, Protocol, TokenUsage};
use kgraf_graph::{GraphStore, Indexer};
use kgraf_mcp::{McpServerConfig, McpTransport};
use kgraf_provider::capability::{Capabilities, StreamEvent, Streaming};
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, Role, StopReason,
};
use kgraf_runtime::{
    CallMode, CancelPhase, ContextConfig, SessionMasker, WorkerBrief, WorkerEvent, WorkerOutcome,
};
use kgraf_sandbox::{Identity, MergeBack, Workspace, WorkspaceManager};
use kgraf_session::approval_store::PersistedAllowStore;
use kgraf_tools::ShellTool;
use kgraf_tools::execpolicy::ExecPolicy;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MODEL: &str = "fake-model";

// ------------------------------------------------------------- fake provider

#[derive(Clone, Default)]
struct Round {
    text: String,
    /// The deltas `text` streams as; one delta of `text` when empty.
    chunks: Vec<String>,
    calls: Vec<(String, String, Value)>,
    /// Stream one delta, then wait forever (a stream to cancel mid-flight).
    hang: bool,
}

impl Round {
    fn answer(text: &str) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// Tool calls, with ids `c<round>-<i>`.
    fn tools(round: usize, calls: Vec<(&str, Value)>) -> Self {
        Self {
            text: "working".into(),
            calls: calls
                .into_iter()
                .enumerate()
                .map(|(i, (name, input))| (format!("c{round}-{i}"), name.to_owned(), input))
                .collect(),
            hang: false,
            ..Self::default()
        }
    }
}

type Script = dyn Fn(usize) -> Round + Send + Sync;

struct Fake {
    script: Box<Script>,
    requests: Mutex<Vec<Request>>,
}

impl Fake {
    fn new(script: impl Fn(usize) -> Round + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }

    /// The round a request is for: one assistant message per earlier round.
    fn round(&self, request: &Request) -> Round {
        self.requests.lock().expect("lock").push(request.clone());
        let n = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        (self.script)(n)
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        uncached_input: 100,
        text_output: 10,
        ..TokenUsage::default()
    }
}

#[async_trait::async_trait]
impl Provider for Fake {
    fn adapter(&self) -> &'static str {
        "fake"
    }
    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let round = self.round(request);
        token.settle(&usage(), None)?;
        Ok(response(request, &round))
    }
    fn capabilities(&self) -> Capabilities<'_> {
        Capabilities::none().with_streaming(self)
    }
}

fn response(request: &Request, round: &Round) -> Response {
    let mut content = vec![Content::text(round.text.clone())];
    for (id, name, input) in &round.calls {
        content.push(Content::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            signature: None,
        });
    }
    Response {
        model: request.model.clone(),
        content,
        stop_reason: if round.calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        },
        usage: usage(),
        request_id: None,
    }
}

#[async_trait::async_trait]
impl Streaming for Fake {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        let round = self.round(request);
        sink(StreamEvent::Started {
            model: request.model.clone(),
            request_id: Some("req-fake".into()),
        });
        sink(StreamEvent::Usage {
            usage: TokenUsage {
                text_output: 0,
                ..usage()
            },
        });
        // A little latency before the first token, so the recorded time to
        // first token is a measurement, not a zero.
        tokio::time::sleep(Duration::from_millis(3)).await;
        if round.chunks.is_empty() {
            sink(StreamEvent::Delta {
                content: Content::text(round.text.clone()),
            });
        }
        for chunk in &round.chunks {
            sink(StreamEvent::Delta {
                content: Content::text(chunk.clone()),
            });
        }
        if round.hang {
            std::future::pending::<()>().await;
        }
        sink(StreamEvent::Usage { usage: usage() });
        sink(StreamEvent::Finished {
            stop_reason: response(request, &round).stop_reason,
        });
        token.settle(&usage(), None)?;
        Ok(response(request, &round))
    }
}

// ------------------------------------------------------------- fixtures

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

const LIB_RS: &str = "pub fn greet() -> &'static str {\n    \"hi\"\n}\n";

/// A repository with one commit, its state directory, its workspace manager,
/// and merge-back onto `main`.
struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    manager: Arc<WorkspaceManager>,
    merge_back: MergeBack,
    /// `main` before any worker ran; no worker moves it.
    main_tip: String,
}

fn repo() -> Repo {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).expect("src");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    std::fs::write(root.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    // `target/` deliberately NOT ignored: the seeded build
    // directory (0060) must stay out of the attempt anyway.
    std::fs::write(root.join(".gitignore"), ".kgraf/\n").expect("gitignore");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let root = root.canonicalize().expect("canonical");
    let state = root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let manager =
        Arc::new(WorkspaceManager::open(&root, &state.join("workspaces")).expect("manager"));
    let merge_back = MergeBack::open(
        manager.clone(),
        "main",
        Identity {
            name: "Test".into(),
            email: "test@example.com".into(),
        },
    )
    .expect("merge-back");
    let main_tip = git(&root, &["rev-parse", "main"]);
    Repo {
        _tmp: tmp,
        root,
        state,
        manager,
        merge_back,
        main_tip,
    }
}

fn echo_server_binary() -> PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        escargot::CargoBuild::new()
            .bin("mcp-echo-server")
            .manifest_path(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../kgraf-mcp-test-server/Cargo.toml"
            ))
            .current_release()
            .current_target()
            .run()
            .expect("build mcp-echo-server")
            .path()
            .to_path_buf()
    })
    .clone()
}

fn echo_server() -> McpServerConfig {
    McpServerConfig::new(
        "echoserver",
        McpTransport::Stdio {
            command: echo_server_binary().to_string_lossy().into_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        },
    )
    .expect("valid")
}

const INSTRUCTION: &str = "Make greet return hello.";

/// Plan, create and record the next segment for `worker`, as the command does.
async fn start(
    repo: &Repo,
    opened: &mut OpenTranscript,
    worker: &str,
    task: &str,
    resume: bool,
    warm: Option<&Path>,
) -> anyhow::Result<Segment> {
    let owner = Owner::new(worker, task, INSTRUCTION);
    let plan = plan_segment(opened, &owner, resume, "HEAD", &view(repo, worker))?;
    let segment = workspace::open(&repo.manager, &repo.merge_back, &plan, warm)?;
    opened.begin_segment(&owner, &plan).await?;
    Ok(segment)
}

/// Reopen a transcript whose previous handle was just dropped. A
/// `tokio::fs::File` closes its descriptor on the blocking pool after the
/// drop, so its `flock` can outlive the handle by a moment; a process exiting
/// (what these drops stand for) releases it at once.
async fn reopen(path: &Path) -> OpenTranscript {
    for _ in 0..100 {
        match open_transcript(path).await {
            Ok(opened) => return opened,
            Err(e) if format!("{e:#}").contains("already open for writing") => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("reopen: {e:#}"),
        }
    }
    panic!("the transcript lock was never released")
}

/// The repository as the planner reads it, for one worker.
fn view<'a>(repo: &'a Repo, worker: &'a str) -> workspace::GitView<'a> {
    workspace::GitView {
        repo: &repo.root,
        manager: &repo.manager,
        worker,
    }
}

/// End a segment the normal way: commit, record, remove.
async fn end_segment(
    repo: &Repo,
    segment: Segment,
    opened: &mut OpenTranscript,
    msg: &str,
) -> SegmentEnd {
    let (end, done) = conclude(
        &repo.manager,
        &repo.merge_back,
        segment,
        opened,
        msg,
        Fault::None,
    )
    .await;
    done.expect("concluded");
    end
}

/// Every record stored on disk, read back through the transcript.
async fn stored_records(opened: &OpenTranscript) -> Vec<Record> {
    use kgraf_session::transcript::Transcript;
    opened
        .transcript
        .load()
        .await
        .expect("load")
        .into_iter()
        .map(|e| e.event)
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn assemble_for(
    repo: &Repo,
    ws: &Workspace,
    provider: Arc<Fake>,
    ledger: Arc<Ledger>,
    graph: Option<GraphParts>,
    config: WorkerConfig,
    allow_store: PersistedAllowStore,
    worker: &str,
) -> Assembled {
    assemble(assembly_for(
        repo,
        ws,
        provider,
        ledger,
        graph,
        config,
        allow_store,
        worker,
    ))
    .await
    .expect("assemble")
}

#[allow(clippy::too_many_arguments)]
fn assembly_for(
    repo: &Repo,
    ws: &Workspace,
    provider: Arc<Fake>,
    ledger: Arc<Ledger>,
    graph: Option<GraphParts>,
    config: WorkerConfig,
    allow_store: PersistedAllowStore,
    worker: &str,
) -> Assembly {
    Assembly {
        ledger,
        provider,
        // One vendor per worker id: the prefix registry is process-wide, and
        // these tests run different tool sets in one process.
        vendor: format!("fake-vendor-{worker}"),
        model: MODEL.into(),
        workspace: ws.clone(),
        checkout: repo.root.clone(),
        graph,
        config,
        worker: worker.into(),
        task: "t1".into(),
        exec_policy: ExecPolicy::default_kgraf_policy(),
        allow_store,
        project: repo.root.to_string_lossy().into_owned(),
        attendance: Attendance::Unattended,
        call_mode: CallMode::Stream,
        max_output_tokens: 1_000,
        max_checkpoints: kgraf_host::limits::DEFAULT_MAX_CHECKPOINTS,
        path_case: kgraf_tools::detect_path_case(ws.root()).expect("case"),
        read_denies: vec![repo.state.clone()],
        mcp_home: repo.state.join("mcp-home"),
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: kgraf_runtime::PrefixRegistry::global(),
        machine_gate: None,
        external_executor: None,
        dependency_refetch: None,
        skills: None,
        permissions: std::sync::Arc::new(kgraf_tools::PermissionControl::manual()),
        protected_writes: None,
        refusals: None,
    }
}

fn brief(worker: &str, ws: &Workspace, slices: Vec<NodeId>, budget: u32) -> WorkerBrief {
    WorkerBrief {
        task: "t1".into(),
        worker: worker.into(),
        instruction: INSTRUCTION.into(),
        workspace_root: ws.root().to_path_buf(),
        model: MODEL.into(),
        slices,
        skills: vec![],
        step_budget: budget,
        context: ContextConfig::for_model(200_000).expect("context"),
        write_scope: None,
    }
}

/// Every finished tool call's output, by call id.
fn outputs(events: &[WorkerEvent]) -> BTreeMap<String, (String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
            } => Some((call_id.clone(), (output.clone(), *is_error))),
            _ => None,
        })
        .collect()
}

fn tool_result_bodies(request: &Request) -> Vec<(String, String)> {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.clone())),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_runs_end_to_end_on_the_real_tools_transcript_masking_mcp_and_ledger() {
    let repo = repo();
    // A warm build directory to seed the worktree's own target from (0060).
    let warm = repo._tmp.path().join("warm-target");
    std::fs::create_dir_all(warm.join("debug")).expect("warm");
    std::fs::write(warm.join("debug/marker"), "warm").expect("marker");
    let transcript_path = repo.state.join("worker-w1.jsonl");
    let mut opened = open_transcript(&transcript_path).await.expect("transcript");
    assert!(opened.records.is_empty());
    let segment = start(&repo, &mut opened, "w1", "t1", false, Some(&warm))
        .await
        .expect("worktree");
    let ws = segment.workspace().clone();
    assert_eq!(ws.name(), "w1-1");
    assert_ne!(ws.root(), repo.root.as_path(), "its own worktree");
    // Seeded into the workspace's scratch, outside the worktree (item 2).
    let target = ShellTool::target_dir(&ws);
    assert!(!target.starts_with(ws.root()));
    let seeded = segment.seeded.clone().expect("seeded");
    assert_eq!(seeded.dest, target);
    assert_eq!(
        std::fs::read_to_string(target.join("debug/marker")).expect("seeded marker"),
        "warm"
    );
    // A build product in the worktree's own `target/`, in a repository that
    // does not ignore it, must never be committed either... except that the
    // worker never builds there; `CARGO_TARGET_DIR` points at the scratch.

    // The worker configuration, with an MCP header secret, in the state
    // directory the worker's shell may not read.
    std::fs::write(
        repo.state.join("worker.json"),
        r#"{"mcp_servers": [], "note": "Bearer E2E-SECRET-TOKEN"}"#,
    )
    .expect("config");
    let secret_path = repo.state.join("worker.json");

    // The graph, indexed from the repository.
    let graph_path = repo.state.join("graph.db");
    let slices = GraphStore::open(&graph_path, &repo.root).expect("graph");
    Indexer::new(&slices).index().expect("index");
    let greet = slices
        .resolve_symbol("greet")
        .expect("resolve")
        .into_iter()
        .next()
        .expect("greet");
    let greet_node = NodeId::new(greet.id.clone());
    let tool_store = GraphStore::open(&graph_path, &repo.root).expect("graph");

    // A persisted exact rule answers the shell hook's ask for this one argv.
    let (mut store, _) =
        PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
    store
        .allow_exact(
            repo.root.to_string_lossy().into_owned(),
            "shell",
            r#"["git","status","--short"]"#,
            "e2e test",
        )
        .expect("rule");
    store
        .allow_exact(
            repo.root.to_string_lossy().into_owned(),
            "shell",
            r#"["printenv","CARGO_TARGET_DIR"]"#,
            "e2e test",
        )
        .expect("rule");
    // Approved by a rule, so it runs; the sandbox still denies the read.
    let head_secret =
        serde_json::to_string(&["head", secret_path.to_str().expect("utf-8")]).expect("json");
    store
        .allow_exact(
            repo.root.to_string_lossy().into_owned(),
            "shell",
            head_secret,
            "e2e test",
        )
        .expect("rule");
    let secret_arg = secret_path.to_string_lossy().into_owned();
    // MCP calls are reviewed (McpApprovalHook): this one tool is allowed.
    store
        .allow_any_call(
            repo.root.to_string_lossy().into_owned(),
            "mcp__echoserver__echo",
            "e2e test",
            kgraf_session::approval_store::ConfirmAllowAnyCall,
        )
        .expect("rule");

    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n pub fn greet() -> &'static str {\n-    \"hi\"\n+    \"hello\"\n*** End Patch";
    let provider = Fake::new(move |n| match n {
        // Round 2's two extra calls try to read the configuration's secret.
        0 => Round::tools(
            0,
            vec![
                ("graph_slice", json!({"symbol": "greet"})),
                ("read_file", json!({"path": "src/lib.rs"})),
                ("list_directory", json!({})),
            ],
        ),
        1 => Round::tools(1, vec![("apply_patch", json!({"patch": patch}))]),
        2 => Round::tools(
            2,
            vec![
                ("shell", json!({"command": ["git", "status", "--short"]})),
                ("shell", json!({"command": ["ls", "src"]})),
                ("shell", json!({"command": ["touch", "unasked.txt"]})),
                (
                    "shell",
                    json!({"command": ["printenv", "CARGO_TARGET_DIR"]}),
                ),
                ("shell", json!({"command": ["cat", "../../worker.json"]})),
                ("shell", json!({"command": ["head", secret_arg]})),
            ],
        ),
        3 => Round::tools(
            3,
            vec![("mcp__echoserver__echo", json!({"message": "ping"}))],
        ),
        // Distinct calls: three identical ones would be a stall (0056).
        4 => Round::tools(4, vec![("search_files", json!({"query": "hello"}))]),
        5 => Round::tools(5, vec![("read_file", json!({"path": "src/lib.rs"}))]),
        _ => Round::answer("greet now returns hello"),
    });
    let assembled = assemble_for(
        &repo,
        &ws,
        provider.clone(),
        ledger.clone(),
        Some(GraphParts {
            slices,
            tool: tool_store,
        }),
        WorkerConfig::with_servers(vec![echo_server()]).expect("config"),
        store,
        "w1",
    )
    .await;
    assert_eq!(assembled.registrations.len(), 1);
    assert!(
        assembled.registrations[0].1.is_complete(),
        "{:?}",
        assembled.registrations[0].1.refused
    );

    let mut seen = Vec::new();
    let result = run_worker(
        assembled.env.clone(),
        brief("w1", &ws, vec![greet_node.clone()], 10),
        &mut opened,
        false,
        None,
        false,
        |e| seen.push(e.clone()),
    )
    .await
    .expect("run");
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{seen:#?}");
    assert_eq!(result.answer, "greet now returns hello");

    // The real tools did the work, in the worker's own worktree.
    let out = outputs(&seen);
    assert!(
        out["c0-0"].0.contains("greet"),
        "graph slice: {:?}",
        out["c0-0"]
    );
    assert_eq!(out["c0-1"], (LIB_RS.to_owned(), false));
    assert!(out["c0-2"].0.contains("src/"), "{:?}", out["c0-2"]);
    assert!(!out["c1-0"].1, "apply_patch: {:?}", out["c1-0"]);
    assert!(
        std::fs::read_to_string(ws.root().join("src/lib.rs"))
            .expect("lib")
            .contains("\"hello\"")
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("src/lib.rs")).expect("main"),
        LIB_RS,
        "the main checkout is untouched"
    );
    assert_eq!(result.changed_paths, vec![PathBuf::from("src/lib.rs")]);
    // Shell, sandboxed through the Workspace: git reads the shared object store
    // (for_workspace's readable git dir); the persisted exact rule answered the
    // hook's ask; `ls` is allow-listed; an unlisted command nobody approved is
    // denied, and never ran.
    assert!(!out["c2-0"].1, "git status: {:?}", out["c2-0"]);
    assert!(out["c2-0"].0.contains("src/lib.rs"), "{:?}", out["c2-0"]);
    assert!(
        out["c2-1"]
            .0
            .starts_with("exit status: 0\nstdout:\nlib.rs\n"),
        "{:?}",
        out["c2-1"]
    );
    assert!(out["c2-2"].1, "{:?}", out["c2-2"]);
    assert!(out["c2-2"].0.contains("denied"), "{:?}", out["c2-2"]);
    assert!(!ws.root().join("unasked.txt").exists());
    // The sandboxed shell builds into the workspace's own target (0060).
    assert!(
        out["c2-3"].0.contains(&format!("{}\n", target.display())),
        "{:?}",
        out["c2-3"]
    );
    // The secret: the unasked `cat` is refused before it runs (its path climbs
    // out of the worktree); the approved `head` runs and the sandbox denies
    // the read. Neither output carries the secret.
    assert!(out["c2-4"].1, "{:?}", out["c2-4"]);
    assert!(
        out["c2-4"].0.contains("reads only the worktree"),
        "{:?}",
        out["c2-4"]
    );
    // Seatbelt denies with EPERM; Landlock's read allowlist with EACCES.
    let denial = if cfg!(target_os = "linux") {
        "Permission denied"
    } else {
        "Operation not permitted"
    };
    assert!(out["c2-5"].0.contains(denial), "{:?}", out["c2-5"]);
    for (id, (text, _)) in &out {
        assert!(
            !text.contains("E2E-SECRET"),
            "{id} leaked the secret: {text}"
        );
    }
    // The MCP tool, from the real server.
    assert_eq!(out["c3-0"], ("ping".to_owned(), false));

    // Masking: the last request masks the oldest tool outputs; the transcript
    // keeps them whole.
    let last = provider.requests().last().cloned().expect("requests");
    let bodies: BTreeMap<String, String> = tool_result_bodies(&last).into_iter().collect();
    let placeholder = SessionMasker::default().placeholder().to_owned();
    assert_eq!(bodies["c0-0"], placeholder, "the graph slice is masked");
    assert_eq!(bodies["c0-1"], placeholder);
    assert_eq!(bodies["c5-0"], LIB_RS.replace("\"hi\"", "\"hello\""));
    assert!(out["c0-0"].0.len() > placeholder.len());

    // The durable transcript holds its owner, the segment start, and every
    // event, in order.
    let stored = stored_records(&opened).await;
    assert_eq!(
        stored[0],
        Record::Owner(Owner::new("w1", "t1", INSTRUCTION))
    );
    assert!(matches!(&stored[1], Record::SegmentStart { segment, .. } if segment == "w1-1"));
    let events: Vec<WorkerEvent> = stored
        .iter()
        .filter_map(|r| match r {
            Record::Event { event } => Some(event.clone()),
            _ => None,
        })
        .collect();
    // The file holds each settled round once, not its deltas (decision 0071).
    assert_eq!(events, kgraf_runtime::settle_for_disk(&seen));

    // MCP attribution: connect, list and call, each a chained ledger row for
    // this worker and task, with duration, outcome and bytes.
    let mcp = ledger.mcp_calls().expect("mcp rows");
    let kinds: Vec<&str> = mcp.iter().map(|r| r.call.kind.as_str()).collect();
    assert_eq!(kinds, ["connect", "list_tools", "call_tool"]);
    assert!(mcp.iter().all(|r| r.call.server == "echoserver"
        && r.call.agent_id.as_deref() == Some("w1")
        && r.call.task_id.as_deref() == Some("t1")
        && r.call.outcome == "success"));
    let call = &mcp[2].call;
    assert_eq!(call.tool.as_deref(), Some("echo"));
    assert_eq!(call.tool_is_error, Some(false));
    assert!(call.bytes_in > 0 && call.bytes_out > 0, "{call:?}");
    assembled.attribution.check().expect("no lost MCP records");

    // Trace fields on every round's row.
    let rows: Vec<_> = result
        .calls
        .iter()
        .map(|c| ledger.call(c).expect("read").expect("row"))
        .collect();
    assert_eq!(rows.len(), 7);
    let digest = assembled.env.prefix.digest().as_str().to_owned();
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.trace.kind.as_deref(), Some("worker_round"));
        assert_eq!(row.trace.role.as_deref(), Some("worker"));
        assert_eq!(
            row.trace.given_prefix_hash.as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(
            row.trace.given_slice_ids,
            vec![greet_node.as_str().to_owned()]
        );
        assert!(row.trace.ttft_ms.is_some_and(|ms| ms >= 0), "row {i}");
        let parent = (i > 0).then(|| rows[i - 1].id.clone());
        assert_eq!(row.trace.parent_call, parent, "row {i}");
    }
    assert_eq!(
        rows[1].trace.tool_rounds,
        vec![vec!["graph_slice", "read_file", "list_directory"]]
    );
    assert_eq!(
        rows[4].trace.tool_rounds,
        vec![vec!["mcp__echoserver__echo"]]
    );
    ledger.flush_anchor();
    let verified = ledger.verify().expect("verify");
    assert!(verified.ok, "{:?}", verified.first_broken);

    // The run ends with its work committed through merge-back under an
    // attempt ref, no worktree left, the seeded target not committed, and the
    // integration branch unmoved: merging is the orchestrator's decision.
    let ws_root = ws.root().to_path_buf();
    let end = end_segment(&repo, segment, &mut opened, "e2e").await;
    let SegmentEnd::Committed {
        attempt_ref,
        commit,
    } = end
    else {
        panic!("the worker changed a file: {end:?}");
    };
    assert_eq!(attempt_ref, "refs/kgraf/attempts/w1-1");
    assert!(!ws_root.exists());
    assert_eq!(git(&repo.root, &["rev-parse", &attempt_ref]), commit);
    assert!(git(&repo.root, &["show", &format!("{commit}:src/lib.rs")]).contains("\"hello\""));
    assert_eq!(
        git(
            &repo.root,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", &commit]
        ),
        "src/lib.rs"
    );
    assert_eq!(git(&repo.root, &["rev-parse", "main"]), repo.main_tip);
    assembled.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_cancelled_mid_stream_resumes_from_its_durable_transcript() {
    let repo = repo();
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    let transcript_path = repo.state.join("worker-w2.jsonl");

    // Segment 1: one tool round that writes a file, then a stream that hangs
    // after its first delta, cancelled there. The cancel waits for that delta
    // rather than a fixed delay: under load a 400 ms timer fired while round 0
    // was still streaming, before `apply_patch` ran, and the segment had
    // nothing to commit (`Empty`).
    let mut opened = open_transcript(&transcript_path).await.expect("transcript");
    let segment = start(&repo, &mut opened, "w2", "t1", false, None)
        .await
        .expect("segment 1");
    let ws = segment.workspace().clone();
    assert_eq!(ws.name(), "w2-1");
    let first = Fake::new(|n| match n {
        0 => Round::tools(
            0,
            vec![(
                "apply_patch",
                json!({"patch": "*** Begin Patch\n*** Add File: notes.txt\n+first segment\n*** End Patch"}),
            )],
        ),
        _ => Round {
            text: "thinking".into(),
            hang: true,
            ..Round::default()
        },
    });
    let (store, _) =
        PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
    let assembled = assemble_for(
        &repo,
        &ws,
        first,
        ledger.clone(),
        None,
        WorkerConfig::default(),
        store,
        "w2",
    )
    .await;
    let mut seen = Vec::new();
    let cancelled = run_worker(
        assembled.env.clone(),
        brief("w2", &ws, vec![], 8),
        &mut opened,
        false,
        // Round 1's only delta: the tool round (text "working") has finished.
        Some(CancelWhen::OnEvent(Box::new(
            |e| matches!(e, WorkerEvent::TextDelta { text } if text == "thinking"),
        ))),
        false,
        |e| seen.push(e.clone()),
    )
    .await
    .expect("run");
    assert_eq!(cancelled.outcome, WorkerOutcome::Cancelled);
    assert!(seen.contains(&WorkerEvent::Cancelled {
        phase: CancelPhase::ModelStream
    }));
    assembled.close().await;
    let end = end_segment(&repo, segment, &mut opened, "segment 1").await;
    let SegmentEnd::Committed {
        attempt_ref,
        commit,
    } = end.clone()
    else {
        panic!("segment 1 wrote a file: {end:?}");
    };
    assert_eq!(attempt_ref, "refs/kgraf/attempts/w2-1");
    drop(opened); // releases the transcript's lock, as the process exiting would

    // Resume is bound to its owner: another task, or another
    // brief, is refused before any worktree is made.
    let mut opened = reopen(&transcript_path).await;
    for (task, instruction) in [("t2", INSTRUCTION), ("t1", "a different brief")] {
        let err = plan_segment(
            &opened,
            &Owner::new("w2", task, instruction),
            true,
            "HEAD",
            &view(&repo, "w2"),
        )
        .expect_err("another owner");
        assert!(err.to_string().contains("refused"), "{err}");
    }
    // A new run of the same worker is refused: the transcript holds a run.
    assert!(
        start(&repo, &mut opened, "w2", "t1", false, None)
            .await
            .is_err()
    );
    // An attempt ref that no longer matches the recorded end is refused (item 1).
    git(&repo.root, &["update-ref", &attempt_ref, &repo.main_tip]);
    let err = start(&repo, &mut opened, "w2", "t1", true, None)
        .await
        .expect_err("moved ref");
    assert!(format!("{err:#}").contains("refused"), "{err:#}");
    git(&repo.root, &["update-ref", &attempt_ref, &commit]);

    // Segment 2: a fresh worktree at the first segment's attempt, and the loop
    // rebuilt from what the transcript file holds.
    // The durable transcript holds the settled form of what was seen: the
    // cancelled round's partial text once, not delta by delta (decision 0071).
    let durable = kgraf_runtime::settle_for_disk(&seen);
    assert_eq!(
        opened.events(),
        durable,
        "resume reads the durable transcript"
    );
    assert!(
        durable.contains(&WorkerEvent::TextDelta {
            text: "thinking".into()
        }),
        "the cancelled round's partial text is kept: {durable:#?}"
    );
    let segment = start(&repo, &mut opened, "w2", "t1", true, None)
        .await
        .expect("resume");
    let ws = segment.workspace().clone();
    assert_eq!(ws.name(), "w2-2");
    assert_eq!(
        std::fs::read_to_string(ws.root().join("notes.txt")).expect("carried"),
        "first segment\n"
    );
    let second = Fake::new(|_| Round::answer("resumed and done"));
    let (store, _) =
        PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
    let assembled = assemble_for(
        &repo,
        &ws,
        second.clone(),
        ledger.clone(),
        None,
        WorkerConfig::default(),
        store,
        "w2",
    )
    .await;
    let mut resumed_events = Vec::new();
    let resumed = run_worker(
        assembled.env.clone(),
        brief("w2", &ws, vec![], 8),
        &mut opened,
        true,
        None,
        false,
        |e| resumed_events.push(e.clone()),
    )
    .await
    .expect("resume");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert_eq!(resumed.answer, "resumed and done");

    // The resumed request carries the first segment's conversation: the
    // pinned instruction first, and the tool round's result.
    let request = second.requests().first().cloned().expect("a request");
    assert_eq!(request.messages[0].content[0].as_text(), Some(INSTRUCTION));
    assert!(
        tool_result_bodies(&request)
            .iter()
            .any(|(id, body)| id == "c0-0" && body.contains("notes.txt")),
        "{request:#?}"
    );
    // The ledger continues the chain of calls across the resume.
    let first_call_of_segment_2 = resumed.calls.last().expect("a call");
    let row = ledger
        .call(first_call_of_segment_2)
        .expect("read")
        .expect("row");
    assert_eq!(row.trace.parent_call.as_ref(), cancelled.calls.last());

    assembled.close().await;
    // Segment 2 changed no file: merge-back leaves no attempt for it.
    let end = end_segment(&repo, segment, &mut opened, "segment 2").await;
    assert_eq!(end, SegmentEnd::Empty);
    assert_eq!(git(&repo.root, &["rev-parse", "main"]), repo.main_tip);
    // The file holds both segments, their starts and ends, and one owner.
    let stored = stored_records(&opened).await;
    let events = stored
        .iter()
        .filter(|r| matches!(r, Record::Event { .. }))
        .count();
    assert_eq!(
        events,
        durable.len() + kgraf_runtime::settle_for_disk(&resumed_events).len()
    );
    assert!(matches!(stored[0], Record::Owner(_)));
    let verified = ledger.verify().expect("verify");
    assert!(verified.ok, "{:?}", verified.first_broken);
}

/// A GitHub token, split where a provider's delta boundary would put it.
fn split_token() -> (String, String, String) {
    let token = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
    let (a, b) = token.split_at(12);
    (token.clone(), a.to_owned(), b.to_owned())
}

/// Decision 0071: a secret the provider streams across two
/// deltas is whole in neither, so each passes its own scrub. The live stream
/// still carries both halves; the file on disk holds neither, only the
/// settled message, scrubbed whole, and a redaction record. The worker's
/// answer is scrubbed too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_split_across_deltas_never_reaches_the_transcript_file() {
    let repo = repo();
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    let path = repo.state.join("worker-w6.jsonl");
    let mut opened = open_transcript(&path).await.expect("transcript");
    let segment = start(&repo, &mut opened, "w6", "t1", false, None)
        .await
        .expect("segment");
    let ws = segment.workspace().clone();
    let (token, a, b) = split_token();
    let (text, chunks) = (
        format!("the key is {token}"),
        vec!["the key is ".to_owned(), a.clone(), b.clone()],
    );
    let provider = Fake::new(move |_| Round {
        text: text.clone(),
        chunks: chunks.clone(),
        ..Round::default()
    });
    let (store, _) =
        PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
    let assembled = assemble_for(
        &repo,
        &ws,
        provider,
        ledger,
        None,
        WorkerConfig::default(),
        store,
        "w6",
    )
    .await;
    let mut seen = Vec::new();
    let result = run_worker(
        assembled.env.clone(),
        brief("w6", &ws, vec![], 4),
        &mut opened,
        false,
        None,
        false,
        |e| seen.push(e.clone()),
    )
    .await
    .expect("run");
    assembled.close().await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{seen:#?}");
    // The live stream is unchanged: each half arrived as its own delta.
    assert!(seen.contains(&WorkerEvent::TextDelta { text: a.clone() }));
    assert!(seen.contains(&WorkerEvent::TextDelta { text: b.clone() }));
    // The answer the run record and the shell show is scrubbed.
    assert!(!result.answer.contains(&token), "{}", result.answer);
    assert!(
        result.answer.contains("[redacted:github-pat;"),
        "{}",
        result.answer
    );
    let raw = std::fs::read_to_string(&path).expect("the transcript file");
    assert!(!raw.contains(&token), "the joined token is on disk");
    assert!(!raw.contains(&a), "a half of the token is on disk");
    assert!(raw.contains("[redacted:github-pat;"), "{raw}");
    assert!(
        raw.contains(r#""type":"redacted""#),
        "the redaction is recorded"
    );
    assert_eq!(opened.events(), kgraf_runtime::settle_for_disk(&seen));
    drop(segment);
}

/// The partial round is written once, joined and scrubbed as
/// one string, so the split token is caught there too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_round_writes_its_partial_text_whole_scrubbed() {
    let repo = repo();
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    let path = repo.state.join("worker-w7.jsonl");
    let mut opened = open_transcript(&path).await.expect("transcript");
    let segment = start(&repo, &mut opened, "w7", "t1", false, None)
        .await
        .expect("segment");
    let ws = segment.workspace().clone();
    let (token, a, b) = split_token();
    let chunks = vec!["the key is ".to_owned(), a.clone(), b.clone()];
    let provider = Fake::new(move |_| Round {
        chunks: chunks.clone(),
        hang: true,
        ..Round::default()
    });
    let (store, _) =
        PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
    let assembled = assemble_for(
        &repo,
        &ws,
        provider,
        ledger,
        None,
        WorkerConfig::default(),
        store,
        "w7",
    )
    .await;
    let last = b.clone();
    let mut seen = Vec::new();
    let result = run_worker(
        assembled.env.clone(),
        brief("w7", &ws, vec![], 4),
        &mut opened,
        false,
        Some(CancelWhen::OnEvent(Box::new(
            move |e| matches!(e, WorkerEvent::TextDelta { text } if *text == last),
        ))),
        false,
        |e| seen.push(e.clone()),
    )
    .await
    .expect("run");
    assembled.close().await;
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    let raw = std::fs::read_to_string(&path).expect("the transcript file");
    assert!(!raw.contains(&token), "the joined token is on disk");
    assert!(!raw.contains(&a), "a half of the token is on disk");
    let events = opened.events();
    let partial = events.iter().position(|e| {
        matches!(e, WorkerEvent::TextDelta { text }
            if text == "the key is [redacted:github-pat; re-read the source for exact text]")
    });
    let cancelled = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::Cancelled { .. }));
    assert!(partial.is_some() && partial < cancelled, "{events:#?}");
    assert_eq!(events, kgraf_runtime::settle_for_disk(&seen));
    drop(segment);
}

/// A segment that never reached its
/// end (the process died) left a worktree with uncommitted work, and the
/// resume's `create` purged it while the transcript claimed the edits. Now
/// the resume is refused, and neither it nor a new run replaces the worktree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfinished_segment_is_never_purged_by_a_resume_or_a_new_run() {
    let repo = repo();
    let path = repo.state.join("worker-w5.jsonl");
    let mut opened = open_transcript(&path).await.expect("transcript");
    let segment = start(&repo, &mut opened, "w5", "t1", false, None)
        .await
        .expect("segment");
    let root = segment.workspace().root().to_path_buf();
    std::fs::write(root.join("uncommitted.txt"), "work").expect("work");
    // The process dies here: no finish, no recorded end.
    drop(segment);
    drop(opened);

    let mut opened = reopen(&path).await;
    let err = start(&repo, &mut opened, "w5", "t1", true, None)
        .await
        .expect_err("an unended segment is not resumed over");
    assert!(format!("{err:#}").contains("never ended"), "{err:#}");
    // A new run from a new transcript must not reuse the name either.
    let mut fresh = open_transcript(&repo.state.join("other.jsonl"))
        .await
        .expect("fresh");
    let err = start(&repo, &mut fresh, "w5", "t1", false, None)
        .await
        .expect_err("the name exists");
    assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    assert_eq!(
        std::fs::read_to_string(root.join("uncommitted.txt")).expect("still there"),
        "work"
    );
}

/// The end-of-run commit ran only on success. An assembly
/// failure (an MCP server that cannot start) now still commits the worktree's
/// work, records the segment's end, and reports the error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_segment_that_fails_before_its_run_still_commits_and_records_its_end() {
    let repo = repo();
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    let path = repo.state.join("worker-w6.jsonl");
    let mut opened = open_transcript(&path).await.expect("transcript");
    let segment = start(&repo, &mut opened, "w6", "t1", false, None)
        .await
        .expect("segment");
    let ws = segment.workspace().clone();
    // Work already in the tree when the failure comes.
    std::fs::write(ws.root().join("partial.txt"), "partial\n").expect("partial");
    let broken = McpServerConfig::new(
        "missing",
        McpTransport::Stdio {
            command: "/nonexistent/mcp-server".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        },
    )
    .expect("valid");
    let (store, _) =
        PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
    let assembly = assembly_for(
        &repo,
        &ws,
        Fake::new(|_| Round::answer("unused")),
        ledger,
        None,
        WorkerConfig::with_servers(vec![broken]).expect("config"),
        store,
        "w6",
    );
    let report = run_segment(
        assembly,
        segment,
        &repo.manager,
        &repo.merge_back,
        &mut opened,
        brief("w6", &ws, vec![], 4),
        SegmentOptions {
            resume: false,
            cancel: None,
            cancel_on_interrupt: false,
            message: "w6".into(),
        },
        |_| {},
        |_| {},
    )
    .await;
    assert!(report.result.is_none());
    assert!(
        format!("{:#}", report.errors[0]).contains("missing"),
        "{:?}",
        report.errors
    );
    let SegmentEnd::Committed { commit, .. } = &report.end else {
        panic!("the partial work is committed: {:?}", report.end);
    };
    assert_eq!(
        git(&repo.root, &["show", &format!("{commit}:partial.txt")]),
        "partial"
    );
    assert!(!ws.root().exists());
    let stored = stored_records(&opened).await;
    assert!(matches!(
        stored.last(),
        Some(Record::SegmentEnd { segment, .. }) if segment == "w6-1"
    ));
}

/// A shell tool bound to one worker's `Workspace` refuses a call made for any
/// other tree, rather than running it under a policy made for another root.
#[tokio::test]
async fn a_shell_tool_bound_to_a_workspace_refuses_another_root() {
    use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
    use kgraf_tools::{
        CancellationToken, DiscoveredTools, Exposure, ToolContext, ToolRegistry, ToolView,
    };
    let repo = repo();
    let mut opened = open_transcript(&repo.state.join("w3.jsonl"))
        .await
        .expect("transcript");
    let segment = start(&repo, &mut opened, "w3", "t1", false, None)
        .await
        .expect("worktree");
    let ws = segment.workspace().clone();
    let (shell, hook) = ShellTool::new(ExecPolicy::default_kgraf_policy());
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(shell.with_workspace(ws.clone())), Exposure::Prefix)
        .expect("register");
    let chain = ApprovalChain::new(vec![Arc::new(hook)]);
    for (root, expect_ok) in [(ws.root().to_path_buf(), true), (repo.root.clone(), false)] {
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &registry,
            discovered: &discovered,
        };
        let Verdict::Run(approved) = chain
            .review(
                view,
                PendingCall {
                    call_id: "c".into(),
                    tool: "shell".into(),
                    input: json!({"command": ["pwd"]}),
                    worker: "w3".into(),
                    task: "t".into(),
                },
                &mut MemoryApprovalLog::default(),
            )
            .await
            .expect("review")
        else {
            panic!("`pwd` is allow-listed");
        };
        let ctx = ToolContext {
            worker: "w3".into(),
            task: "t".into(),
            workspace_root: root.clone(),
            cancel: CancellationToken::new(),
            progress: kgraf_tools::ProgressSink::default(),
        };
        let result = registry.execute(approved, &ctx).await;
        if expect_ok {
            let out = result.expect("runs in its own workspace").render();
            assert!(out.contains(&ws.root().display().to_string()), "{out}");
        } else {
            let err = result.expect_err("another root is refused").to_string();
            assert!(err.contains("bound to workspace"), "{err}");
        }
    }
    end_segment(&repo, segment, &mut opened, "w3").await;
}

/// A worker that plants `.gitattributes` (a filter driver git would run on the
/// host while hashing files) is refused by merge-back, naming the path; no
/// attempt ref is written and the worktree is kept for inspection.
#[tokio::test]
async fn a_planted_gitattributes_is_refused_at_the_end_of_run_commit() {
    let repo = repo();
    let mut opened = open_transcript(&repo.state.join("w4.jsonl"))
        .await
        .expect("transcript");
    let segment = start(&repo, &mut opened, "w4", "t1", false, None)
        .await
        .expect("worktree");
    let root = segment.workspace().root().to_path_buf();
    std::fs::write(root.join(".gitattributes"), "* filter=pwn\n").expect("plant");
    std::fs::write(root.join("src/lib.rs"), "pub fn greet() {}\n").expect("edit");
    let end = end_segment(&repo, segment, &mut opened, "w4").await;
    let SegmentEnd::Refused { reasons, kept } = &end else {
        panic!("refused: {end:?}");
    };
    assert!(reasons.contains(".gitattributes"), "{reasons}");
    assert_eq!(kept, &root.display().to_string());
    assert!(root.exists(), "kept for inspection");
    assert_eq!(
        git(
            &repo.root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/kgraf/attempts/"
            ]
        ),
        ""
    );
    assert_eq!(git(&repo.root, &["rev-parse", "main"]), repo.main_tip);
    // A resume after a refused end is refused: the kept worktree is not replaced.
    let err = start(&repo, &mut opened, "w4", "t1", true, None)
        .await
        .expect_err("refused end");
    assert!(format!("{err:#}").contains("kept"), "{err:#}");
}

/// The end was recorded after the attempt ref was written and
/// the worktree removed, so a kill in that window left an unended transcript
/// that resume refused and a new run could not start (the ref existed): the
/// worker was stuck. `conclude` now commits, records, then removes; a kill
/// after the commit leaves a state `plan_segment` recovers from, whether or
/// not the worktree is still there, and a moved ref is still refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_between_the_commit_and_the_end_record_is_recovered_on_resume() {
    let repo = repo();
    for (worker, remove_worktree) in [("w7", false), ("w8", true)] {
        let path = repo.state.join(format!("worker-{worker}.jsonl"));
        let mut opened = open_transcript(&path).await.expect("transcript");
        let segment = start(&repo, &mut opened, worker, "t1", false, None)
            .await
            .expect("segment");
        let ws = segment.workspace().clone();
        std::fs::write(ws.root().join("done.txt"), "done\n").expect("work");
        let (committed, _) = conclude(
            &repo.manager,
            &repo.merge_back,
            segment,
            &mut opened,
            worker,
            Fault::AfterCommit,
        )
        .await;
        let SegmentEnd::Committed { commit, .. } = committed.clone() else {
            panic!("committed: {committed:?}");
        };
        if remove_worktree {
            // The order before the fix: ref written, worktree removed, no record.
            repo.manager.remove(ws.clone()).expect("remove");
        }
        drop(opened);

        let mut opened = reopen(&path).await;
        let resumed = start(&repo, &mut opened, worker, "t1", true, None)
            .await
            .expect("recovered, not refused");
        assert_eq!(resumed.workspace().name(), format!("{worker}-2"));
        assert_eq!(
            std::fs::read_to_string(resumed.workspace().root().join("done.txt")).expect("carried"),
            "done\n"
        );
        // The recovered end is recorded before the new segment's start.
        let stored = stored_records(&opened).await;
        let n = stored.len();
        assert_eq!(
            stored[n - 2],
            Record::SegmentEnd {
                segment: format!("{worker}-1"),
                end: committed.clone(),
            }
        );
        assert!(matches!(&stored[n - 1], Record::SegmentStart { start, .. } if *start == commit));
        assert_eq!(
            ws.root().exists(),
            !remove_worktree,
            "a leftover is kept, never purged"
        );
        end_segment(&repo, resumed, &mut opened, "segment 2").await;
    }

    // Not recovered: an attempt ref that does not start from the recorded start.
    let path = repo.state.join("worker-w9.jsonl");
    let mut opened = open_transcript(&path).await.expect("transcript");
    let segment = start(&repo, &mut opened, "w9", "t1", false, None)
        .await
        .expect("segment");
    std::fs::write(segment.workspace().root().join("x.txt"), "x\n").expect("work");
    let (_, _) = conclude(
        &repo.manager,
        &repo.merge_back,
        segment,
        &mut opened,
        "w9",
        Fault::AfterCommit,
    )
    .await;
    git(
        &repo.root,
        &["update-ref", "refs/kgraf/attempts/w9-1", &repo.main_tip],
    );
    drop(opened);
    let mut opened = reopen(&path).await;
    let err = start(&repo, &mut opened, "w9", "t1", true, None)
        .await
        .expect_err("a foreign ref is not taken as the end");
    assert!(format!("{err:#}").contains("does not start"), "{err:#}");
}

/// The other window: killed after the end is recorded, before the worktree
/// is removed. The resume plans the next number and leaves the worktree alone.
/// An empty segment killed after merge-back removed its worktree, with no
/// record, is recovered as empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_between_the_end_record_and_the_removal_resumes_past_the_leftover() {
    let repo = repo();
    let path = repo.state.join("worker-w10.jsonl");
    let mut opened = open_transcript(&path).await.expect("transcript");
    let segment = start(&repo, &mut opened, "w10", "t1", false, None)
        .await
        .expect("segment");
    let leftover = segment.workspace().root().to_path_buf();
    std::fs::write(leftover.join("a.txt"), "a\n").expect("work");
    let (end1, _) = conclude(
        &repo.manager,
        &repo.merge_back,
        segment,
        &mut opened,
        "w10",
        Fault::AfterRecord,
    )
    .await;
    assert!(matches!(end1, SegmentEnd::Committed { .. }));
    drop(opened);
    let mut opened = reopen(&path).await;
    let second = start(&repo, &mut opened, "w10", "t1", true, None)
        .await
        .expect("resumes past the leftover");
    assert_eq!(second.workspace().name(), "w10-2");
    assert!(
        leftover.join("a.txt").exists(),
        "the leftover is not purged"
    );

    // Empty, killed after merge-back removed the worktree, before the record.
    let (empty, _) = conclude(
        &repo.manager,
        &repo.merge_back,
        second,
        &mut opened,
        "w10 empty",
        Fault::AfterCommit,
    )
    .await;
    assert_eq!(empty, SegmentEnd::Empty);
    drop(opened);
    let mut opened = reopen(&path).await;
    let third = start(&repo, &mut opened, "w10", "t1", true, None)
        .await
        .expect("recovered as empty");
    assert_eq!(third.workspace().name(), "w10-3");
    let stored = stored_records(&opened).await;
    assert!(stored.contains(&Record::SegmentEnd {
        segment: "w10-2".into(),
        end: SegmentEnd::Empty,
    }));
    end_segment(&repo, third, &mut opened, "w10 3").await;
}

/// A stdio MCP server without its own `cwd` used to start
/// in the worktree, which the worker writes (a reopened worktree keeps even
/// ignored files), so a planted module there ran on the host. It now starts in
/// a clean directory under the state directory and learns the worktree from
/// `KGRAF_WORKTREE`. The server here is a real process: a shell that
/// records its working directory, would run a planted script if it found one
/// in its working directory, then execs the real echo server.
#[tokio::test]
async fn a_stdio_mcp_server_starts_outside_the_worktree_and_never_runs_a_planted_module() {
    let repo = repo();
    let mut opened = open_transcript(&repo.state.join("w9.jsonl"))
        .await
        .expect("transcript");
    let segment = start(&repo, &mut opened, "w9", "t1", false, None)
        .await
        .expect("worktree");
    let ws = segment.workspace().clone();
    let out = repo._tmp.path().join("server-cwd.txt");
    let told = repo._tmp.path().join("server-worktree.txt");
    let marker = repo._tmp.path().join("planted-ran.txt");
    // What a worker could leave behind: a module the server would pick up
    // from its working directory.
    std::fs::write(
        ws.root().join("planted.sh"),
        format!("echo pwned > '{}'\n", marker.display()),
    )
    .expect("plant");
    let script = format!(
        "pwd -P > '{}'; printf %s \"$KGRAF_WORKTREE\" > '{}'; \
         if [ -f planted.sh ]; then sh planted.sh; fi; exec '{}'",
        out.display(),
        told.display(),
        echo_server_binary().display()
    );
    let server = McpServerConfig::new(
        "echoserver",
        McpTransport::Stdio {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            env: BTreeMap::new(),
            cwd: None,
        },
    )
    .expect("valid");
    let config = WorkerConfig::with_servers(vec![server]).expect("config");
    let (store, _) =
        PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    let assembled = assemble_for(
        &repo,
        &ws,
        Fake::new(|_| Round::answer("done")),
        ledger,
        None,
        config,
        store,
        "w9",
    )
    .await;
    assert!(
        assembled
            .env
            .tools
            .names()
            .any(|n| n == "mcp__echoserver__echo"),
        "the server connected"
    );
    let cwd = PathBuf::from(std::fs::read_to_string(&out).expect("cwd recorded").trim());
    let home = assembled
        .mcp_home
        .clone()
        .expect("a home")
        .canonicalize()
        .expect("home");
    assert!(home.starts_with(repo.state.canonicalize().expect("state")));
    assert_eq!(cwd, home, "a clean directory under the state directory");
    assert!(!cwd.starts_with(ws.root()), "never the worktree");
    assert_eq!(
        std::fs::read_to_string(&told).expect("told"),
        ws.root().to_string_lossy(),
        "the worktree is passed in KGRAF_WORKTREE"
    );
    assert!(!marker.exists(), "the planted module did not run");
    assembled.close().await;
    assert!(!home.exists(), "the home is removed at worker end");
    end_segment(&repo, segment, &mut opened, "w9").await;
}

/// The approval chain held only the shell hook, so MCP
/// calls ran unreviewed. `McpApprovalHook` now asks about each one, and the
/// assembled resolver answers the ask: denied with no persisted rule
/// (nobody attached), allowed with one.
#[tokio::test]
async fn an_mcp_call_is_denied_without_a_persisted_rule_and_allowed_with_one() {
    use kgraf_tools::approval::{MemoryApprovalLog, PendingCall, UserAnswer, Verdict};
    use kgraf_tools::{DiscoveredTools, ToolView};
    let repo = repo();
    let mut opened = open_transcript(&repo.state.join("w10.jsonl"))
        .await
        .expect("transcript");
    let segment = start(&repo, &mut opened, "w10", "t1", false, None)
        .await
        .expect("worktree");
    let ws = segment.workspace().clone();
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    for allowed in [false, true] {
        if allowed {
            // The supported path: `kgraf allow`.
            kgraf_cli::worker::ask::allow_mcp_tool(
                &repo.state,
                &repo.root.to_string_lossy(),
                "mcp__echoserver__echo",
                "review test",
                &["echoserver".to_owned()],
            )
            .expect("rule");
        }
        let (store, _) =
            PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
        let config = WorkerConfig::with_servers(vec![echo_server()]).expect("config");
        let assembled = assemble_for(
            &repo,
            &ws,
            Fake::new(|_| Round::answer("done")),
            ledger.clone(),
            None,
            config,
            store,
            &format!("w10-{allowed}"),
        )
        .await;
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &assembled.env.tools,
            discovered: &discovered,
        };
        let verdict = assembled
            .env
            .approval
            .review(
                view,
                PendingCall {
                    call_id: "c".into(),
                    tool: "mcp__echoserver__echo".into(),
                    input: json!({"message": "ping"}),
                    worker: "w10".into(),
                    task: "t1".into(),
                },
                &mut MemoryApprovalLog::default(),
            )
            .await
            .expect("review");
        let Verdict::Ask(paused) = verdict else {
            panic!("an MCP call is reviewed, not run unasked");
        };
        let answer = assembled
            .env
            .asks
            .as_ref()
            .expect("resolver")
            .answer(&paused)
            .await;
        match (allowed, answer) {
            (false, UserAnswer::Deny { reason }) => {
                assert!(reason.contains("no persisted allow rule"), "{reason}");
                assert!(
                    reason.contains("kgraf allow mcp__echoserver__echo"),
                    "the denial names the supported way to allow it: {reason}"
                );
            }
            (true, UserAnswer::Allow) => {}
            (a, other) => panic!("rule {a}: {other:?}"),
        }
        assembled.close().await;
    }
    end_segment(&repo, segment, &mut opened, "w10").await;
}

/// Answering `a` at the terminal for an MCP tool used to
/// allow it once while the denial text promised a recorded rule. It now records
/// an any-call rule, which later calls (here, a fresh unattended resolver)
/// are allowed by.
#[tokio::test]
async fn answering_always_at_the_terminal_records_an_mcp_rule() {
    use kgraf_cli::worker::ask::{Choice, RuleOrPromptResolver};
    use kgraf_runtime::AskResolver;
    use kgraf_tools::approval::{MemoryApprovalLog, PendingCall, UserAnswer, Verdict};
    use kgraf_tools::{DiscoveredTools, ToolView};
    let repo = repo();
    let mut opened = open_transcript(&repo.state.join("w11.jsonl"))
        .await
        .expect("transcript");
    let segment = start(&repo, &mut opened, "w11", "t1", false, None)
        .await
        .expect("worktree");
    let ws = segment.workspace().clone();
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    let project = repo.root.to_string_lossy().into_owned();
    let mut answers = Vec::new();
    for (attended, worker) in [(true, "w11-a"), (false, "w11-b")] {
        let (store, _) =
            PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
        let config = WorkerConfig::with_servers(vec![echo_server()]).expect("config");
        let (store_for_env, _) =
            PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
        let assembled = assemble_for(
            &repo,
            &ws,
            Fake::new(|_| Round::answer("done")),
            ledger.clone(),
            None,
            config,
            store_for_env,
            worker,
        )
        .await;
        let attendance = if attended {
            Attendance::Terminal {
                timeout: Duration::from_secs(5),
            }
        } else {
            Attendance::Unattended
        };
        let resolver = RuleOrPromptResolver::new(
            store,
            project.clone(),
            assembled.env.tools.clone(),
            attendance,
        )
        .with_prompter(Arc::new(|_: &str, _: &str| Ok(Choice::Always)));
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &assembled.env.tools,
            discovered: &discovered,
        };
        let Verdict::Ask(paused) = assembled
            .env
            .approval
            .review(
                view,
                PendingCall {
                    call_id: "c".into(),
                    tool: "mcp__echoserver__echo".into(),
                    input: json!({"message": "ping"}),
                    worker: worker.into(),
                    task: "t1".into(),
                },
                &mut MemoryApprovalLog::default(),
            )
            .await
            .expect("review")
        else {
            panic!("asked");
        };
        answers.push(resolver.answer(&paused).await);
        assembled.close().await;
    }
    assert!(matches!(answers[0], UserAnswer::Allow), "{:?}", answers[0]);
    assert!(
        matches!(answers[1], UserAnswer::Allow),
        "the recorded rule answers the unattended ask: {:?}",
        answers[1]
    );
    end_segment(&repo, segment, &mut opened, "w11").await;
}

/// A running `mcp-echo-server --http`: `rmcp`'s own Streamable HTTP server on
/// loopback, a hosted server as far as the worker can tell. Killed on drop.
struct HostedEchoServer {
    _child: tokio::process::Child,
    url: String,
}

impl HostedEchoServer {
    async fn spawn() -> Self {
        use tokio::io::AsyncBufReadExt;
        let mut child = tokio::process::Command::new(echo_server_binary())
            .arg("--http")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the HTTP echo server");
        let stdout = child.stdout.take().expect("stdout");
        let line = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::io::BufReader::new(stdout).lines().next_line(),
        )
        .await
        .expect("the server announces its address")
        .expect("read")
        .expect("a line");
        let url = line
            .strip_prefix("LISTENING ")
            .expect("LISTENING <url>")
            .to_owned();
        Self { _child: child, url }
    }

    /// Tool-call POSTs the server has answered, from its own counters.
    async fn posts(&self) -> u64 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let address = self
            .url
            .trim_start_matches("http://")
            .trim_end_matches("/mcp")
            .to_owned();
        let mut socket = tokio::net::TcpStream::connect(&address)
            .await
            .expect("connect");
        socket
            .write_all(
                format!(
                    "GET /admin/stats HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write");
        let mut response = String::new();
        socket.read_to_string(&mut response).await.expect("read");
        let body = response.split("\r\n\r\n").nth(1).expect("body");
        let stats: Value = serde_json::from_str(body).expect("stats json");
        stats["post_sse"].as_u64().expect("post_sse")
            + stats["post_json"].as_u64().expect("post_json")
    }
}

/// Decision 0051: a tool on a hosted (Streamable HTTP) server goes through
/// the same review as a stdio one. In an unattended run, with no persisted rule,
/// `McpApprovalHook`'s ask is denied and the call never reaches the server;
/// after `kgraf allow` records the rule, the same call is allowed and runs
/// against the server.
#[tokio::test]
async fn a_hosted_mcp_call_is_denied_without_a_rule_and_runs_after_kgraf_allow() {
    use kgraf_tools::approval::{MemoryApprovalLog, PendingCall, UserAnswer, Verdict};
    use kgraf_tools::{CancellationToken, DiscoveredTools, ToolContext, ToolView};
    let repo = repo();
    let server = HostedEchoServer::spawn().await;
    let hosted = McpServerConfig::new(
        "hosted",
        McpTransport::StreamableHttp {
            url: server.url.clone(),
            headers: BTreeMap::new(),
            proxy: None,
            oauth: false,
        },
    )
    .expect("valid http config");
    let mut opened = open_transcript(&repo.state.join("w12.jsonl"))
        .await
        .expect("transcript");
    let segment = start(&repo, &mut opened, "w12", "t1", false, None)
        .await
        .expect("worktree");
    let ws = segment.workspace().clone();
    let ledger = Arc::new(Ledger::open(&repo.state.join("ledger.db")).expect("ledger"));
    for allowed in [false, true] {
        if allowed {
            // The supported path: the same function `kgraf allow` runs.
            kgraf_cli::worker::ask::allow_mcp_tool(
                &repo.state,
                &repo.root.to_string_lossy(),
                "mcp__hosted__echo",
                "hosted review test",
                &["hosted".to_owned()],
            )
            .expect("rule");
        }
        let (store, _) =
            PersistedAllowStore::open(allow_store_path(&repo.state), ws.root()).expect("store");
        let config = WorkerConfig::with_servers(vec![hosted.clone()]).expect("config");
        let assembled = assemble_for(
            &repo,
            &ws,
            Fake::new(|_| Round::answer("done")),
            ledger.clone(),
            None,
            config,
            store,
            &format!("w12-{allowed}"),
        )
        .await;
        let discovered = DiscoveredTools::new();
        let view = || ToolView {
            registry: &assembled.env.tools,
            discovered: &discovered,
        };
        let mut log = MemoryApprovalLog::default();
        let verdict = assembled
            .env
            .approval
            .review(
                view(),
                PendingCall {
                    call_id: "c".into(),
                    tool: "mcp__hosted__echo".into(),
                    input: json!({"message": "hosted ping"}),
                    worker: "w12".into(),
                    task: "t1".into(),
                },
                &mut log,
            )
            .await
            .expect("review");
        let Verdict::Ask(paused) = verdict else {
            panic!("a hosted MCP call is reviewed, not run unasked");
        };
        let answer = assembled
            .env
            .asks
            .as_ref()
            .expect("resolver")
            .answer(&paused)
            .await;
        let posts_before = server.posts().await;
        let resumed = assembled
            .env
            .approval
            .answer(view(), paused, answer.clone(), &mut log)
            .await
            .expect("answer");
        match (allowed, answer, resumed) {
            (false, UserAnswer::Deny { reason }, Verdict::Denied { .. }) => {
                assert!(reason.contains("no persisted allow rule"), "{reason}");
                assert!(
                    reason.contains("kgraf allow mcp__hosted__echo"),
                    "the denial names the supported way to allow it: {reason}"
                );
                assert_eq!(
                    server.posts().await,
                    posts_before,
                    "a denied call never reaches the hosted server"
                );
            }
            (true, UserAnswer::Allow, Verdict::Run(approved)) => {
                let ctx = ToolContext {
                    worker: "w12".into(),
                    task: "t1".into(),
                    workspace_root: ws.root().to_path_buf(),
                    cancel: CancellationToken::new(),
                    progress: kgraf_tools::ProgressSink::default(),
                };
                let out = assembled
                    .env
                    .tools
                    .execute(approved, &ctx)
                    .await
                    .expect("the allowed call runs")
                    .render();
                assert!(out.contains("hosted ping"), "{out}");
                assert_eq!(
                    server.posts().await,
                    posts_before + 1,
                    "the call went to the hosted server"
                );
            }
            (a, answer, verdict) => panic!("rule {a}: {answer:?} then {verdict:?}"),
        }
        assembled.close().await;
    }
    end_segment(&repo, segment, &mut opened, "w12").await;
}
