//! A run end to end over the real git
//! worktrees, the real verifier and merge-back, not the orchestrator tests'
//! plain directories. A worker spawns a writer subagent; the parent's write
//! to the child's path is refused while the child lives; the child's work is
//! merged into the parent's worktree, verified with it, and lands on the
//! run's integration branch; the child's refs are swept at the end.

use crate::verifier::CommandVerifier;
use crate::workspaces::{GitWorkspaces, child, ensure_integration_branch};
use kgraf_core::{AgentId, ModelPrice, PriceTable, Protocol, RunId, TokenUsage};
use kgraf_graph::GraphStore;
use kgraf_orchestrator::{
    Admission, AlwaysReady, ChildMerge, Limits, ModelCaller, ModelDecomposer, ModelGrounder,
    Orchestrator, OrchestratorConfig, Route, RunStatus, RunStore, SUBAGENT_REPORT_MARKER, Services,
    SessionLine, SubagentReport, SystemClock, session_tools,
};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, Role, StopReason,
};
use kgraf_runtime::{
    CallMode, ContextConfig, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot, SharedPrefix,
    WorkerEnv,
};
use kgraf_sandbox::{MergeBack, WorkspaceManager};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) const MODEL: &str = "e2e-model";

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
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

/// A reply chosen from a request's text and round.
pub(crate) type Reply = dyn Fn(&str, usize) -> Vec<Content> + Send + Sync;

/// Every request's text, and a reply chosen from it.
pub(crate) struct Script {
    pub(crate) reply: Box<Reply>,
    pub(crate) seen: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for Script {
    fn adapter(&self) -> &'static str {
        "fake-e2e"
    }
    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let text: String = request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|c| match c {
                Content::Text { text, .. } => text.clone(),
                Content::ToolUse { id, .. } => format!("<use {id}>"),
                Content::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => format!("<result {tool_use_id}: {content}>"),
                Content::Reasoning { .. } => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let round = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        self.seen.lock().expect("lock").push(text.clone());
        let content = (self.reply)(&text, round);
        let stop = if content.iter().any(|c| matches!(c, Content::ToolUse { .. })) {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content,
            stop_reason: stop,
            usage,
            request_id: None,
        })
    }
}

pub(crate) struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        Access::none()
            .write_path(
                input["declares"]
                    .as_str()
                    .or(input["path"].as_str())
                    .unwrap_or_default(),
            )
            .map_err(|source| ToolError::Access {
                tool: "write_file".into(),
                source,
            })
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let path = ctx
            .workspace_root
            .join(input["path"].as_str().unwrap_or_default());
        let fail = |e: std::io::Error| ToolError::Internal {
            tool: "write_file".into(),
            reason: e.to_string(),
        };
        std::fs::create_dir_all(path.parent().unwrap_or(&ctx.workspace_root)).map_err(fail)?;
        std::fs::write(&path, input["content"].as_str().unwrap_or_default()).map_err(fail)?;
        Ok(ToolOutput::text("written"))
    }
}

struct Sleep;

#[async_trait::async_trait]
impl Tool for Sleep {
    fn name(&self) -> &str {
        "sleep"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "sleep".into(),
            description: "Wait".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok(ToolOutput::text("slept"))
    }
}

pub(crate) fn call(id: &str, name: &str, input: Value) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
        signature: None,
    }
}

/// Waits on a shared semaphore (`wait_latch`) or adds a permit to it
/// (`open_latch`), so a test orders a child's finish after a parent's action
/// without a clock. `entered` is notified when a waiter arrives.
struct Latch {
    name: &'static str,
    permits: Arc<tokio::sync::Semaphore>,
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Tool for Latch {
    fn name(&self) -> &str {
        self.name
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Latch".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        _input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        if self.name == "open_latch" {
            self.permits.add_permits(1);
            return Ok(ToolOutput::text("opened"));
        }
        self.entered.notify_one();
        // A cancelled worker's wait ends at once: a tool that ignored the
        // token would hold the cancelled child open.
        tokio::select! {
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled { tool: self.name.into() }),
            permit = self.permits.acquire() => {
                permit
                    .map_err(|e| ToolError::Internal {
                        tool: self.name.into(),
                        reason: e.to_string(),
                    })?
                    .forget();
                Ok(ToolOutput::text("passed"))
            }
        }
    }
}

/// Two callers must be inside `meet` at once to pass: a child that never
/// gets its peer sees an error result instead of `met`, so passing proves the
/// two subagents ran concurrently.
struct Meet(tokio::sync::Barrier);

#[async_trait::async_trait]
impl Tool for Meet {
    fn name(&self) -> &str {
        "meet"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "meet".into(),
            description: "Rendezvous".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        match tokio::time::timeout(Duration::from_secs(60), self.0.wait()).await {
            Ok(_) => Ok(ToolOutput::text("met")),
            Err(_) => Ok(ToolOutput::error("no peer arrived")),
        }
    }
}

/// Every subagent report the text carries, in order.
fn reports(text: &str) -> Vec<SubagentReport> {
    text.match_indices(SUBAGENT_REPORT_MARKER)
        .filter_map(|(at, _)| SubagentReport::parse(&text[at..]))
        .collect()
}

fn spawn(id: &str, task: &str, writes: &[&str]) -> Content {
    call(
        id,
        "spawn_subagent",
        json!({"task": task, "read_only": false, "writes": writes}),
    )
}

fn write(id: &str, path: &str, content: &str) -> Content {
    call(id, "write_file", json!({"path": path, "content": content}))
}

fn is_child(text: &str) -> bool {
    text.contains("You are subagent")
}

/// The id a child's instruction names, from the requests a script saw.
fn child_id(script: &Script) -> Option<String> {
    script.seen.lock().expect("lock").iter().find_map(|t| {
        let rest = t.split("You are subagent `").nth(1)?;
        Some(rest.split('`').next()?.to_owned())
    })
}

/// A git repository, its integration branch and an orchestrator wired to
/// the real worktrees, verifier and merge-back, with `reply` as the workers'
/// model. The contract may write `writes`.
struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    /// The run's state folder: its trace is `runs/r1/trace.jsonl` here.
    state: std::path::PathBuf,
    branch: String,
    worker: Arc<Script>,
    workspaces: Arc<GitWorkspaces>,
    graph: Arc<Mutex<GraphStore>>,
    admission: Arc<Admission>,
}

impl Fixture {
    fn new(
        reply: Box<Reply>,
        extra_tools: Vec<Arc<dyn Tool>>,
        writes: &[&str],
    ) -> (Orchestrator, Fixture) {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README"), "hello\n").expect("write");
        git(&root, &["add", "."]);
        git(
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
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).expect("mkdir");
        let manager =
            Arc::new(WorkspaceManager::open(&root, &dir.path().join("ws")).expect("manager"));
        let branch = ensure_integration_branch(&root, "r1").expect("branch");
        let merge_back = Arc::new(
            MergeBack::open(
                manager.clone(),
                &branch,
                kgraf_sandbox::Identity {
                    name: "Test Owner".into(),
                    email: "owner@example.invalid".into(),
                },
            )
            .expect("merge-back"),
        );
        let workspaces = Arc::new(GitWorkspaces::new(manager, merge_back.clone()));
        let graph = Arc::new(Mutex::new(
            GraphStore::open(&state.join("graph.db"), &root).expect("graph"),
        ));
        let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));

        let worker = Arc::new(Script {
            reply,
            seen: Mutex::new(Vec::new()),
        });
        let plan = json!({"contracts": [{
            "id": "a",
            "instruction": "make a",
            "acceptance": "src/a.rs exists",
            "writes": writes.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
            "depends_on": [],
            "tools": ["write_file"],
            "attempts": 1,
        }]})
        .to_string();
        let planner = Arc::new(Script {
            reply: Box::new(move |_, _| vec![Content::text(plan.clone())]),
            seen: Mutex::new(Vec::new()),
        });
        let grounder = Arc::new(Script {
            reply: Box::new(|_, _| vec![Content::text(r#"{"serves":true,"reason":"on track"}"#)]),
            seen: Mutex::new(Vec::new()),
        });

        let line = SessionLine::new();
        let mut registry = ToolRegistry::new();
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(WriteFile), Arc::new(Sleep)];
        tools.extend(extra_tools);
        tools.extend(session_tools(&line));
        for tool in tools {
            registry
                .register_builtin(tool, Exposure::Prefix)
                .expect("register");
        }
        let prefix = PrefixSnapshot::new(
            SharedPrefix {
                vendor: "vendor-e2e".into(),
                model: MODEL.into(),
                system: vec![Content::text("You are a kGraf worker.")],
                tools: registry.prefix_definitions(),
                skill_catalog: vec![],
                slice_pool: vec![],
                mcp_instructions: vec![],
            },
            &NeutralPrefixEncoder,
        )
        .expect("prefix");
        let mut env = WorkerEnv::new(ledger.clone(), worker.clone(), Arc::new(registry), prefix);
        env.prefixes = Arc::new(PrefixRegistry::new());
        env.settings.call_mode = CallMode::Complete;
        let mut prices = PriceTable::new();
        prices.set(
            MODEL,
            ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 100_000,
                cache_write: 1_250_000,
                output: 2_000_000,
                reasoning: 2_000_000,
            },
        );
        let caller = |p: Arc<Script>| ModelCaller {
            reasoning: Default::default(),
            provider: p,
            ledger: ledger.clone(),
            model: MODEL.into(),
            max_output_tokens: 2_000,
            agent: "orchestrator:r1".into(),
            task: "r1".into(),
        };
        let admission = Admission::new(Limits::default());
        let services = Services {
            graph: graph.clone(),
            ledger: ledger.clone(),
            prices,
            routes: vec![Route {
                name: "alpha".into(),
                vendor: "vendor-e2e".into(),
                env: Arc::new(env),
                context: ContextConfig::for_model(200_000).expect("context"),
                readiness: Arc::new(AlwaysReady),
                local: false,
            }],
            decomposer: Arc::new(ModelDecomposer(caller(planner))),
            grounder: Arc::new(ModelGrounder(caller(grounder))),
            verifier: Arc::new(CommandVerifier {
                command: "true".into(),
                repo: root.clone(),
                workspaces: workspaces.clone(),
                merge_back: merge_back.clone(),
                read_denies: Vec::new(),
                timeout: Duration::from_secs(120),
                permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
                root_gate: false,
            }),
            workspaces: workspaces.clone(),
            clock: Arc::new(SystemClock),
            admission: admission.clone(),
            runs_dir: state.join("runs"),
            profiles: Arc::new(Mutex::new(
                kgraf_core::model_profile::ProfileStore::default(),
            )),
            profiles_dir: state.join("profiles"),
        };
        let config = OrchestratorConfig {
            tick: Duration::from_millis(10),
            // Generous: a parent naps a round at a time while it waits, and a
            // paused child must not cost it its budget.
            step_budget: 600,
            ..OrchestratorConfig::default()
        };
        let o = Orchestrator::new(config, services).expect("orchestrator");
        line.bind(o.handle()).expect("bind");
        (
            o,
            Fixture {
                _dir: dir,
                root,
                state: state.clone(),
                branch,
                worker,
                workspaces,
                graph,
                admission,
            },
        )
    }

    /// The run completed. When it did not, the failure carries the verdict,
    /// notice and worker-finished lines of its trace: the report names only
    /// how many attempts failed, and the trace says why (a check that could
    /// not start, a refused merge, a worker out of rounds), so a run on a
    /// machine nobody can attach to still names its cause.
    #[track_caller]
    fn assert_completed(&self, report: &kgraf_orchestrator::RunReport) {
        if report.status == RunStatus::Completed {
            return;
        }
        panic!(
            "the run did not complete: {:?}\n{}\n{report:?}",
            report.status,
            self.trace_evidence()
        );
    }

    /// The verdict, notice and worker-finished lines of the run's trace.
    fn trace_evidence(&self) -> String {
        let path = self.state.join("runs").join("r1").join("trace.jsonl");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return format!("(no trace at {}: {e})", path.display()),
        };
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| {
                ["verdict", "notice", "worker_finished"]
                    .iter()
                    .any(|kind| l.contains(&format!("\"event\":\"{kind}\"")))
            })
            .collect();
        format!("verdicts and notices from the trace:\n{}", lines.join("\n"))
    }

    fn seen(&self) -> Vec<String> {
        self.worker.seen.lock().expect("lock").clone()
    }

    /// The most reports any one request of the workers carried.
    fn reports(&self) -> Vec<SubagentReport> {
        self.seen()
            .iter()
            .map(|t| reports(t))
            .max_by_key(Vec::len)
            .unwrap_or_default()
    }

    fn on_branch(&self, path: &str) -> String {
        git(&self.root, &["show", &format!("{}:{path}", self.branch)])
    }

    fn agents_in_flight(&self) -> usize {
        self.admission.gate().in_flight().1
    }

    /// The end-of-run sweep leaves no subagent ref behind.
    fn assert_swept(&self) {
        let record = RunStore::new(self.graph.clone())
            .load(&RunId::new("r1"))
            .expect("load");
        self.workspaces
            .sweep_refs(record.as_ref(), || Ok(Default::default()))
            .expect("sweep");
        assert_eq!(
            git(
                &self.root,
                &["for-each-ref", "--format=%(refname)", child::REF_ROOT]
            ),
            "",
            "no subagent refs remain after the run"
        );
    }
}

/// Run to the end within a generous bound; the bound only turns a hang into
/// a failure and is never what a test waits on.
async fn run_to_end(o: Orchestrator) -> kgraf_orchestrator::RunReport {
    tokio::time::timeout(
        Duration::from_secs(120),
        o.start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended in time")
    .expect("run")
}

#[test]
fn a_writer_subagent_over_git_worktrees_is_merged_verified_integrated_and_swept() {
    crate::e2e_rt::run(
        "a_writer_subagent_over_git_worktrees_is_merged_verified_integrated_and_swept",
        crate::e2e_rt::Flavor::MultiThread,
        async {
            let (o, fx) = Fixture::new(
                Box::new(|text, round| {
                    if is_child(text) {
                        return if round == 0 {
                            vec![write("c", "src/c.rs", "child c\n")]
                        } else {
                            vec![Content::text("wrote c")]
                        };
                    }
                    if !text.contains("<use spawn>") {
                        return vec![spawn("spawn", "write c", &["src/c.rs"])];
                    }
                    if !text.contains("<use early>") {
                        return vec![write("early", "src/c.rs", "parent early\n")];
                    }
                    if SubagentReport::parse(text).is_none() {
                        return vec![call(&format!("nap{round}"), "sleep", json!({"r": round}))];
                    }
                    if !text.contains("<use a>") {
                        return vec![write("a", "src/a.rs", "parent a\n")];
                    }
                    vec![Content::text("done")]
                }),
                vec![],
                &["src/a.rs", "src/c.rs"],
            );
            let report = run_to_end(o).await;
            fx.assert_completed(&report);

            let seen = fx.seen();
            // The refusal names the path in the platform's own form
            // (`src\c.rs` on Windows), so the check reads it with `/`.
            let early: Vec<String> = seen
                .iter()
                .filter(|t| t.contains("<result early:"))
                .map(|t| t.replace("\\\\", "/").replace('\\', "/"))
                .collect();
            assert!(
                early
                    .iter()
                    .any(|t| t.contains("<result early: ERROR: refused: ")
                        && t.contains("The refusal: src/c.rs is fenced")),
                "the parent's write to the child's path was refused while it lived; the \
                 parent saw: {early:?}"
            );
            let report = seen
                .iter()
                .rev()
                .find_map(|t| SubagentReport::parse(t))
                .expect("the child's report reached the parent");
            assert!(
                matches!(report.merge, ChildMerge::Merged { .. }),
                "{report:?}"
            );
            // On the integration branch: the child's work, verified with the parent's.
            assert_eq!(fx.on_branch("src/c.rs"), "child c");
            assert_eq!(fx.on_branch("src/a.rs"), "parent a");
            fx.assert_swept();
        },
    );
}

/// Two writer subagents with disjoint paths run at once (each waits in
/// `meet` for the other), and both merge into the parent, with the parent's
/// own work, on the integration branch.
#[test]
fn two_concurrent_writer_subagents_with_disjoint_writes_both_merge() {
    crate::e2e_rt::run(
        "two_concurrent_writer_subagents_with_disjoint_writes_both_merge",
        crate::e2e_rt::Flavor::MultiThread,
        async {
            let (o, fx) = Fixture::new(
                Box::new(|text, round| {
                    if is_child(text) {
                        let path = if text.contains("Your task:\nwrite c") {
                            "src/c.rs"
                        } else {
                            "src/d.rs"
                        };
                        let content = format!("child {path}\n");
                        if !text.contains("<use meet>") {
                            return vec![call("meet", "meet", json!({}))];
                        }
                        if !text.contains("<use w>") {
                            return vec![write("w", path, &content)];
                        }
                        return vec![Content::text("wrote")];
                    }
                    if !text.contains("<use spawn1>") {
                        return vec![
                            spawn("spawn1", "write c", &["src/c.rs"]),
                            spawn("spawn2", "write d", &["src/d.rs"]),
                        ];
                    }
                    if reports(text).len() < 2 {
                        return vec![call(&format!("nap{round}"), "sleep", json!({"r": round}))];
                    }
                    if !text.contains("<use a>") {
                        return vec![write("a", "src/a.rs", "parent a\n")];
                    }
                    vec![Content::text("done")]
                }),
                vec![Arc::new(Meet(tokio::sync::Barrier::new(2)))],
                &["src/a.rs", "src/c.rs", "src/d.rs"],
            );
            let report = run_to_end(o).await;
            fx.assert_completed(&report);

            let seen = fx.seen();
            assert!(
                !seen.iter().any(|t| t.contains("no peer arrived")),
                "each child met the other: they were live together"
            );
            let reports = fx.reports();
            assert_eq!(reports.len(), 2, "{reports:?}");
            for r in &reports {
                assert!(matches!(r.merge, ChildMerge::Merged { .. }), "{r:?}");
            }
            assert_eq!(fx.on_branch("src/c.rs"), "child src/c.rs");
            assert_eq!(fx.on_branch("src/d.rs"), "child src/d.rs");
            assert_eq!(fx.on_branch("src/a.rs"), "parent a");
            assert_eq!(fx.agents_in_flight(), 0, "every slot was released");
            fx.assert_swept();
        },
    );
}

/// A child stopped while it waits in a tool settles: its slot is released
/// and the run does not finish around it. Resumed, it continues from its
/// transcript and its work merges.
#[test]
fn a_subagent_stopped_mid_run_releases_its_slot_and_resumes_to_a_merge() {
    crate::e2e_rt::run(
        "a_subagent_stopped_mid_run_releases_its_slot_and_resumes_to_a_merge",
        crate::e2e_rt::Flavor::MultiThread,
        async {
            let permits = Arc::new(tokio::sync::Semaphore::new(0));
            let entered = Arc::new(tokio::sync::Notify::new());
            let (o, fx) = Fixture::new(
                Box::new(|text, round| {
                    if is_child(text) {
                        if !text.contains("<use hold>") {
                            return vec![call("hold", "wait_latch", json!({}))];
                        }
                        if !text.contains("<use w>") {
                            return vec![write("w", "src/c.rs", "child c\n")];
                        }
                        return vec![Content::text("wrote c")];
                    }
                    if !text.contains("<use spawn>") {
                        return vec![spawn("spawn", "write c", &["src/c.rs"])];
                    }
                    if reports(text).is_empty() {
                        return vec![call(&format!("nap{round}"), "sleep", json!({"r": round}))];
                    }
                    if !text.contains("<use a>") {
                        return vec![write("a", "src/a.rs", "parent a\n")];
                    }
                    vec![Content::text("done")]
                }),
                vec![Arc::new(Latch {
                    name: "wait_latch",
                    permits,
                    entered: entered.clone(),
                })],
                &["src/a.rs", "src/c.rs"],
            );
            let handle = o.handle();
            let run = tokio::spawn(run_to_end(o));

            // The child is inside its tool: mid-run, holding a slot.
            tokio::time::timeout(Duration::from_secs(120), entered.notified())
                .await
                .expect("the child reached its tool");
            let child = AgentId::new(child_id(&fx.worker).expect("the child's request was seen"));
            let live = fx.agents_in_flight();
            handle
                .stop_session(child.clone())
                .await
                .expect("the child stops");
            // The child's cancelled attempt settles and frees its slot. The bound
            // only turns a hang into a failure.
            let freed = async {
                while fx.agents_in_flight() >= live {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            tokio::time::timeout(Duration::from_secs(120), freed)
                .await
                .expect("the stopped child released its slot");
            assert!(
                !run.is_finished(),
                "the run waits for its stopped child instead of finishing around it"
            );
            assert!(
                fx.reports().is_empty(),
                "a stopped child has not reported to its parent"
            );

            handle
                .resume_session(child)
                .await
                .expect("the child resumes");
            let report = run.await.expect("the run task");
            fx.assert_completed(&report);
            let reports = fx.reports();
            assert_eq!(reports.len(), 1, "{reports:?}");
            assert!(
                matches!(reports[0].merge, ChildMerge::Merged { .. }),
                "{:?}",
                reports[0]
            );
            assert_eq!(fx.on_branch("src/c.rs"), "child c");
            assert_eq!(fx.on_branch("src/a.rs"), "parent a");
            assert_eq!(fx.agents_in_flight(), 0, "every slot was released");
            fx.assert_swept();
        },
    );
}

/// The parent changes a path the child owns without declaring it (a write
/// the fence cannot see), so the merge is refused: the parent gets a typed
/// conflict, nothing of the child is applied, and the parent's own version
/// is what lands.
#[test]
fn a_subagent_merge_the_parent_conflicts_with_is_refused_and_reported() {
    crate::e2e_rt::run(
        "a_subagent_merge_the_parent_conflicts_with_is_refused_and_reported",
        crate::e2e_rt::Flavor::MultiThread,
        async {
            let permits = Arc::new(tokio::sync::Semaphore::new(0));
            let entered = Arc::new(tokio::sync::Notify::new());
            let latch = |name| -> Arc<dyn Tool> {
                Arc::new(Latch {
                    name,
                    permits: permits.clone(),
                    entered: entered.clone(),
                })
            };
            let (o, fx) = Fixture::new(
                Box::new(|text, round| {
                    if is_child(text) {
                        // The child finishes only after the parent's undeclared write.
                        if !text.contains("<use wait>") {
                            return vec![call("wait", "wait_latch", json!({}))];
                        }
                        if !text.contains("<use w>") {
                            return vec![write("w", "src/c.rs", "child c\n")];
                        }
                        return vec![Content::text("wrote c")];
                    }
                    if !text.contains("<use spawn>") {
                        return vec![spawn("spawn", "write c", &["src/c.rs"])];
                    }
                    if !text.contains("<use sneak>") {
                        // Declares src/a.rs (in the parent's scope, not fenced) and
                        // writes src/c.rs, the child's path.
                        return vec![call(
                            "sneak",
                            "write_file",
                            json!({"path": "src/c.rs", "declares": "src/a.rs", "content": "parent sneaky\n"}),
                        )];
                    }
                    if !text.contains("<use open>") {
                        return vec![call("open", "open_latch", json!({}))];
                    }
                    if reports(text).is_empty() {
                        return vec![call(&format!("nap{round}"), "sleep", json!({"r": round}))];
                    }
                    if !text.contains("<use a>") {
                        return vec![write("a", "src/a.rs", "parent a\n")];
                    }
                    vec![Content::text("done")]
                }),
                vec![latch("wait_latch"), latch("open_latch")],
                &["src/a.rs", "src/c.rs"],
            );
            let report = run_to_end(o).await;
            fx.assert_completed(&report);

            let reports = fx.reports();
            assert_eq!(reports.len(), 1, "{reports:?}");
            let ChildMerge::Conflict { paths, .. } = &reports[0].merge else {
                panic!("expected a conflict, got {:?}", reports[0]);
            };
            assert_eq!(paths, &vec!["src/c.rs".to_owned()]);
            assert_eq!(
                fx.on_branch("src/c.rs"),
                "parent sneaky",
                "nothing of the refused child was applied"
            );
            assert_eq!(fx.on_branch("src/a.rs"), "parent a");
            assert_eq!(fx.agents_in_flight(), 0, "every slot was released");
            fx.assert_swept();
        },
    );
}

/// A child whose task panics is settled by the orchestrator's panic
/// supervisor: the parent is told it failed, its slot is released, and its
/// fence is lifted, so the parent can write the child's path afterwards.
#[test]
fn a_subagent_whose_task_panics_is_settled_and_its_slot_and_fence_released() {
    crate::e2e_rt::run(
        "a_subagent_whose_task_panics_is_settled_and_its_slot_and_fence_released",
        crate::e2e_rt::Flavor::MultiThread,
        async {
            let (o, fx) = Fixture::new(
                Box::new(|text, round| {
                    if is_child(text) {
                        panic!("the child's provider call panicked");
                    }
                    if !text.contains("<use spawn>") {
                        return vec![spawn("spawn", "write c", &["src/c.rs"])];
                    }
                    if reports(text).is_empty() {
                        return vec![call(&format!("nap{round}"), "sleep", json!({"r": round}))];
                    }
                    if !text.contains("<use after>") {
                        return vec![write("after", "src/c.rs", "parent c\n")];
                    }
                    vec![Content::text("done")]
                }),
                vec![],
                &["src/c.rs"],
            );
            let report = run_to_end(o).await;
            fx.assert_completed(&report);

            let reports = fx.reports();
            assert_eq!(reports.len(), 1, "{reports:?}");
            assert!(
                reports[0].outcome.contains("fail"),
                "the parent is told the child failed: {:?}",
                reports[0]
            );
            assert!(
                !matches!(reports[0].merge, ChildMerge::Merged { .. }),
                "a panicked child merges nothing: {:?}",
                reports[0]
            );
            let seen = fx.seen();
            assert!(
                seen.iter().any(|t| t.contains("<result after: written>")),
                "the panicked child's fence was lifted, so the parent's write went through"
            );
            assert_eq!(fx.on_branch("src/c.rs"), "parent c");
            assert_eq!(fx.agents_in_flight(), 0, "the child's slot was released");
            fx.assert_swept();
        },
    );
}
