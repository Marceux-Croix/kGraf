//! A worker refused the same command program twice that never calls
//! `report_shortfall`. Tested on the mechanism: a scripted provider plays the
//! model, an approval hook plays the policy that refuses, and a stand-in
//! `report_shortfall` records what it was called with.
//!
//! 1. Two refusals of one program: kGraf files the permission report, names
//!    the program and the command lines, marks it as filed by kGraf, and the
//!    result and the worker log say so.
//! 2. One refusal and then a report by the model: nothing is filed.
//! 3. Two different programs refused once each: nothing is filed.
//! 4. A program that later ran does not count its earlier refusal.
//! 5. A shell command that only writes a file in the worktree, refused twice,
//!    files nothing and its refusal carries the `apply_patch` skeleton; a write
//!    outside the worktree, or one that also runs a command, files as before.
//! 6. The same path refused for the write scope twice files a `file` report
//!    for it; two different paths refused once each file nothing.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, StopReason,
};
use kgraf_runtime::{
    CallMode, ContextConfig, KGRAF_FILED_LEAD, NeutralPrefixEncoder, NoticeKind, PrefixRegistry,
    PrefixSnapshot, SharedPrefix, SpawnOptions, ToolChoiceSupport, WorkerBrief, WorkerEnv,
    WorkerEvent, WorkerHandle, WorkerResult, spawn_worker,
};
use kgraf_tools::approval::{ApprovalChain, ApprovalHook, HookDecision, PendingCall};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- scripted provider

/// One reply: text, and the tool calls it carries.
struct Reply {
    text: String,
    calls: Vec<(String, String, Value)>,
}

fn says(text: &str) -> Reply {
    Reply {
        text: text.into(),
        calls: Vec::new(),
    }
}

/// A shell call of `argv`.
fn shell(id: &str, argv: &[&str]) -> Reply {
    Reply {
        text: String::new(),
        calls: vec![(id.into(), "shell".into(), json!({"command": argv}))],
    }
}

/// A call of the report tool by the model.
fn reports(id: &str, need: &str) -> Reply {
    Reply {
        text: String::new(),
        calls: vec![(
            id.into(),
            "report_shortfall".into(),
            json!({"kind": "permission", "need": need, "why": "refused"}),
        )],
    }
}

type Script = dyn Fn(&Request) -> Reply + Send + Sync;

struct Fake {
    script: Box<Script>,
}

impl Fake {
    fn new(script: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
        })
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
        let reply = (self.script)(request);
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        let stop = if reply.calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        };
        let mut content = vec![Content::text(reply.text)];
        for (id, name, input) in reply.calls {
            content.push(Content::ToolUse {
                id,
                name,
                input,
                signature: None,
            });
        }
        Ok(Response {
            finish_reason: Some("stop".into()),
            model: request.model.clone(),
            content,
            stop_reason: stop,
            usage,
            request_id: None,
        })
    }
}

/// How many tool results the request carries.
fn results_in(request: &Request) -> usize {
    request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|c| matches!(c, Content::ToolResult { .. }))
        .count()
}

// ---------------------------------------------------------------- tools

/// Refuses `mkdir` and `python3` as the unattended ask policy does, except
/// with `--allowed` as the first argument; allows the rest.
struct RefuseUnlisted;

#[async_trait::async_trait]
impl ApprovalHook for RefuseUnlisted {
    fn name(&self) -> &str {
        "user"
    }
    async fn review(&self, call: &PendingCall) -> HookDecision {
        let program = call.input["command"][0].as_str().unwrap_or_default();
        let allowed = call.input["command"][1] == "--allowed";
        if call.tool == "shell" && matches!(program, "mkdir" | "python3" | "sh") && !allowed {
            HookDecision::Deny {
                reason: "unattended run".into(),
            }
        } else {
            HookDecision::Allow
        }
    }
}

/// Runs nothing; stands in for a shell command that the policy allowed.
struct Shell;

#[async_trait::async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".into(),
            description: "Run a command".into(),
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
        Ok(ToolOutput::text("ok"))
    }
}

/// Writes nothing; declares the write of `path`, so a declared write scope
/// can refuse it.
struct WriteFile;

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
            .write_path(input["path"].as_str().unwrap_or_default())
            .map_err(|source| ToolError::Access {
                tool: "write_file".into(),
                source,
            })
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("written"))
    }
}

/// Stands in for the orchestrator's `report_shortfall`: records its input and
/// answers as a held decision does.
struct ShortfallStandIn {
    inputs: Arc<Mutex<Vec<Value>>>,
}

#[async_trait::async_trait]
impl Tool for ShortfallStandIn {
    fn name(&self) -> &str {
        "report_shortfall"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "report_shortfall".into(),
            description: "report a shortfall".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        self.inputs.lock().expect("lock").push(input);
        Ok(ToolOutput::text("held: stand-in. Stop now."))
    }
}

// ---------------------------------------------------------------- harness

const MODEL: &str = "fake-model";

fn env(provider: Arc<Fake>, inputs: Arc<Mutex<Vec<Value>>>) -> Arc<WorkerEnv> {
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(Shell), Exposure::Prefix)
        .expect("register");
    registry
        .register_builtin(Arc::new(WriteFile), Exposure::Prefix)
        .expect("register");
    registry
        .register_builtin(Arc::new(ShortfallStandIn { inputs }), Exposure::Prefix)
        .expect("register");
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "fake-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a kGraf worker.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("snapshot");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    env.settings.tool_choice = ToolChoiceSupport::default();
    env.approval = ApprovalChain::new(vec![Arc::new(RefuseUnlisted)]);
    Arc::new(env)
}

fn brief(root: &Path) -> WorkerBrief {
    WorkerBrief {
        task: "task-w1".into(),
        worker: "w1".into(),
        instruction: "Make out/ and run the script.".into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 20,
        context: ContextConfig::for_model(100_000).expect("config"),
        write_scope: None,
    }
}

async fn run(provider: Arc<Fake>) -> (Vec<WorkerEvent>, WorkerResult, Vec<Value>) {
    run_scoped(provider, None).await
}

/// [`run`] with the contract's declared write scope.
async fn run_scoped(
    provider: Arc<Fake>,
    write_scope: Option<Vec<std::path::PathBuf>>,
) -> (Vec<WorkerEvent>, WorkerResult, Vec<Value>) {
    let dir = tempfile::tempdir().expect("dir");
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let mut brief = brief(dir.path());
    brief.write_scope = write_scope;
    let handle = spawn_worker(
        env(provider, inputs.clone()),
        brief,
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    let filed = inputs.lock().expect("lock").clone();
    (events, result, filed)
}

async fn drain(mut handle: WorkerHandle) -> (Vec<WorkerEvent>, WorkerResult) {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = handle.next_event().await {
            events.push(event);
        }
        handle.result().await.expect("result")
    };
    let result = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("in time");
    (events, result)
}

fn notices_of(events: &[WorkerEvent], wanted: NoticeKind) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Notice { kind, text } if *kind == wanted => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn filed_notices(events: &[WorkerEvent]) -> Vec<String> {
    notices_of(events, NoticeKind::KgrafShortfall)
}

/// The permissions-manual run: `mkdir -p out` refused, other calls between,
/// then the same `mkdir -p out` refused again, with no report. kGraf files the
/// report for `mkdir`.
#[tokio::test]
async fn the_same_command_refused_twice_without_a_report_is_filed_by_kgraf() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => shell("t1", &["mkdir", "-p", "out"]),
        1 => shell("t2", &["python3", "-c", "print(1)"]),
        2 => shell("t3", &["mkdir", "-p", "out"]),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    // One report reached the tool, from kGraf, naming the program refused twice.
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert_eq!(filed[0]["kind"], "permission");
    assert_eq!(filed[0]["scope"], "command");
    assert_eq!(filed[0]["need"], "mkdir");
    let why = filed[0]["why"].as_str().expect("why");
    assert!(why.starts_with(KGRAF_FILED_LEAD), "{why}");
    assert!(why.contains("`mkdir` refused 2 time(s)"), "{why}");
    assert!(why.contains("`mkdir -p out`"), "{why}");
    assert!(why.contains("`python3` refused 1 time(s)"), "{why}");
    // The result carries the flag, with the tool's answer.
    let report = result.kgraf_shortfall.as_ref().expect("a filed report");
    assert_eq!(report.need, "mkdir");
    assert_eq!(report.reply.as_deref(), Some("held: stand-in. Stop now."));
    // The worker log shows it as kGraf's, then shows the answer.
    let notices = filed_notices(&events);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("kGraf reported it for you"),
        "{notices:?}"
    );
    let answers = notices_of(&events, NoticeKind::KgrafShortfallAnswer);
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert!(answers[0].contains("held: stand-in"), "{answers:?}");
    // The model never called the report tool.
    assert!(!events.iter().any(|e| matches!(
        e,
        WorkerEvent::ToolCallStarted { tool, .. } if tool == "report_shortfall"
    )));
}

/// One refusal and then the model's own report: nothing is filed.
#[tokio::test]
async fn one_refusal_then_a_report_by_the_model_files_nothing() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => shell("t1", &["python3", "-c", "print(1)"]),
        1 => reports("r1", "python3"),
        _ => says("stopped"),
    });
    let (events, result, filed) = run(provider).await;
    assert_eq!(filed.len(), 1, "only the model's own: {filed:?}");
    assert_eq!(filed[0]["why"], "refused");
    assert!(result.kgraf_shortfall.is_none());
    assert!(filed_notices(&events).is_empty());
}

/// A report by the model before the second refusal of a command ends the guard
/// for the attempt: the second refusal is then the model's to handle.
#[tokio::test]
async fn a_report_by_the_model_before_the_second_refusal_ends_the_guard() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => shell("t1", &["python3", "-c", "print(1)"]),
        1 => reports("r1", "python3"),
        2 => shell("t2", &["python3", "-c", "print(1)"]),
        _ => says("stopped"),
    });
    let (_, result, filed) = run(provider).await;
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
}

/// Two different programs refused once each are two first refusals, not a
/// repeat: nothing is filed.
#[tokio::test]
async fn two_different_programs_refused_once_each_file_nothing() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => shell("t1", &["mkdir", "-p", "out"]),
        1 => shell("t2", &["python3", "-c", "print(1)"]),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    assert!(filed.is_empty(), "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
    assert!(filed_notices(&events).is_empty());
    assert_eq!(result.refused.len(), 2);
}

/// A refused program that then runs has its count cleared.
#[tokio::test]
async fn a_program_that_later_ran_does_not_count_its_earlier_refusal() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => shell("t1", &["python3", "-c", "print(1)"]),
        1 => shell("t2", &["python3", "--allowed"]),
        2 => shell("t3", &["python3", "-c", "print(2)"]),
        _ => says("done"),
    });
    let (_, result, filed) = run(provider).await;
    assert!(filed.is_empty(), "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
}

/// The lang-python shape: two different exploratory commands of one program,
/// refused once each. The worker is exploring, not stuck on one command, so
/// nothing is filed.
#[tokio::test]
async fn two_different_commands_of_one_program_refused_once_each_file_nothing() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => shell(
            "t1",
            &["python3", "-c", "import re; print(re.findall('a', 'a'))"],
        ),
        1 => shell(
            "t2",
            &[
                "python3",
                "-c",
                "import re, collections; print(collections.Counter('a'))",
            ],
        ),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    assert!(filed.is_empty(), "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
    assert!(filed_notices(&events).is_empty());
    assert_eq!(result.refused.len(), 2);
}

/// The same command line with different spacing is the same command.
#[tokio::test]
async fn the_same_command_with_other_spacing_counts_as_the_same() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => shell("t1", &["python3", "-c", "print(1,   2)"]),
        1 => shell("t2", &["python3", "-c", "print(1, 2)"]),
        _ => says("done"),
    });
    let (_, result, filed) = run(provider).await;
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert_eq!(filed[0]["need"], "python3");
    assert!(result.kgraf_shortfall.is_some());
}

/// The refusal texts the worker saw, in order.
fn refusals_seen(events: &[WorkerEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::ToolCallFinished {
                output,
                is_error: true,
                ..
            } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

/// A worker that writes a whole file with `sh -c` and a heredoc, refused
/// twice with the identical line: the file is `apply_patch`'s to write, so no
/// shortfall is filed, and each refusal carries the whole-file patch.
#[tokio::test]
async fn a_heredoc_file_write_refused_twice_files_nothing_and_names_apply_patch() {
    let script = "cat > loyalty_points/src/lib.rs << 'EOF'\npub fn a() {}\nEOF";
    let provider = Fake::new(move |request| match results_in(request) {
        0 => shell("t1", &["sh", "-c", script]),
        1 => shell("t2", &["sh", "-c", script]),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    assert!(filed.is_empty(), "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
    assert!(filed_notices(&events).is_empty());
    let seen = refusals_seen(&events);
    assert_eq!(seen.len(), 2, "{seen:?}");
    for text in seen {
        assert!(text.contains("apply_patch"), "{text}");
        assert!(
            text.contains(
                "*** Begin Patch\n*** Add File: loyalty_points/src/lib.rs\n+full new content\n\
                 *** End Patch"
            ),
            "{text}"
        );
    }
}

/// A heredoc to a path outside the worktree is no pure write, and kGraf
/// cannot read the script command by command, so no grant covers it: nothing
/// is filed for it (not for `sh`), no patch is offered, and each refusal says
/// to split the script.
#[tokio::test]
async fn a_heredoc_write_outside_the_worktree_files_nothing_and_says_to_split() {
    let script = "cat > /etc/kgraf_probe << 'EOF'\nx\nEOF";
    let provider = Fake::new(move |request| match results_in(request) {
        0 | 1 => shell("t", &["sh", "-c", script]),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    assert!(filed.is_empty(), "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
    let seen = refusals_seen(&events);
    assert_eq!(seen.len(), 2, "{seen:?}");
    for text in seen {
        assert!(!text.contains("*** Begin Patch"), "{text}");
        assert!(text.contains("split it into separate commands"), "{text}");
    }
}

/// `sh -c "mkdir -p out && cp a b"` refused twice, unretried: the report names
/// the programs inside the script that a grant must name, `mkdir` first and
/// `cp` with it, and never `sh` (a grant of `sh` runs none of them).
#[tokio::test]
async fn a_script_refused_twice_files_for_the_programs_inside_it_not_the_shell() {
    let script = "mkdir -p out && cp templates/a.txt out/a.txt";
    let provider = Fake::new(move |request| match results_in(request) {
        0 | 1 => shell("t", &["sh", "-c", script]),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert_eq!(filed[0]["scope"], "command");
    assert_eq!(filed[0]["need"], "mkdir");
    let why = filed[0]["why"].as_str().expect("why");
    assert!(why.starts_with(KGRAF_FILED_LEAD), "{why}");
    assert!(why.contains("refused `mkdir`, `cp` and"), "{why}");
    assert!(why.contains("`mkdir` refused 2 time(s)"), "{why}");
    assert!(!why.contains("`sh`"), "{why}");
    let report = result.kgraf_shortfall.as_ref().expect("a filed report");
    assert_eq!(report.need, "mkdir");
    // A script that is read command by command is not told to split.
    assert!(
        refusals_seen(&events)
            .iter()
            .all(|t| !t.contains("split it into separate commands"))
    );
}

/// A script kGraf cannot read command by command (a substitution), refused
/// twice: no program grant covers it, so nothing is filed, and the refusal
/// says to split the script.
#[tokio::test]
async fn an_unparsable_script_refused_twice_files_nothing_and_says_to_split() {
    let script = "mkdir -p out && cp $(date) out/a.txt";
    let provider = Fake::new(move |request| match results_in(request) {
        0 | 1 => shell("t", &["sh", "-c", script]),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    assert!(filed.is_empty(), "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
    assert!(filed_notices(&events).is_empty());
    let seen = refusals_seen(&events);
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen.iter()
            .all(|t| t.contains("split it into separate commands")),
        "{seen:?}"
    );
}

// ---------------------------------------------------------------- write scope

/// A write of `path`, with `content`.
fn writes(id: &str, path: &str, content: &str) -> Reply {
    Reply {
        text: String::new(),
        calls: vec![(
            id.into(),
            "write_file".into(),
            json!({"path": path, "content": content}),
        )],
    }
}

fn scope_of(paths: &[&str]) -> Option<Vec<std::path::PathBuf>> {
    Some(paths.iter().map(std::path::PathBuf::from).collect())
}

/// The same path outside the write scope refused twice, with another call
/// between and different content, and no report. kGraf files a `file` report
/// for the path, once.
#[tokio::test]
async fn the_same_path_refused_for_the_write_scope_twice_files_a_file_report() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => writes("t1", "pyproject.toml", "a"),
        1 => shell("t2", &["python3", "--allowed"]),
        2 => writes("t3", "pyproject.toml", "b"),
        3 => writes("t4", "pyproject.toml", "c"),
        _ => says("done"),
    });
    let (events, result, filed) = run_scoped(provider, scope_of(&["src/a.rs"])).await;
    assert_eq!(filed.len(), 1, "one filing per attempt: {filed:?}");
    assert_eq!(filed[0]["kind"], "permission");
    assert_eq!(filed[0]["scope"], "file");
    assert_eq!(filed[0]["need"], "pyproject.toml");
    let why = filed[0]["why"].as_str().expect("why");
    assert!(why.starts_with(KGRAF_FILED_LEAD), "{why}");
    assert!(why.contains("`pyproject.toml` refused 2 time(s)"), "{why}");
    let report = result.kgraf_shortfall.as_ref().expect("a filed report");
    assert_eq!(report.scope, "file");
    assert_eq!(report.need, "pyproject.toml");
    assert_eq!(report.reply.as_deref(), Some("held: stand-in. Stop now."));
    assert_eq!(filed_notices(&events).len(), 1);
}

/// Two different paths outside the scope, refused once each, are two first
/// refusals: nothing is filed.
#[tokio::test]
async fn two_different_paths_refused_once_each_file_nothing() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => writes("t1", "pyproject.toml", "a"),
        1 => writes("t2", "setup.cfg", "a"),
        _ => says("done"),
    });
    let (events, result, filed) = run_scoped(provider, scope_of(&["src/a.rs"])).await;
    assert!(filed.is_empty(), "{filed:?}");
    assert!(result.kgraf_shortfall.is_none());
    assert!(filed_notices(&events).is_empty());
    assert_eq!(result.refused.len(), 2);
}

/// A report by the model before the second refusal of a path ends the guard.
#[tokio::test]
async fn a_report_by_the_model_ends_the_path_guard() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => writes("t1", "pyproject.toml", "a"),
        1 => reports("r1", "pyproject.toml"),
        2 => writes("t2", "pyproject.toml", "b"),
        _ => says("done"),
    });
    let (_, result, filed) = run_scoped(provider, scope_of(&["src/a.rs"])).await;
    assert_eq!(filed.len(), 1, "only the model's own: {filed:?}");
    assert!(result.kgraf_shortfall.is_none());
}

/// A script that writes and also runs a command is no pure write: it files,
/// for the programs inside it.
#[tokio::test]
async fn a_write_that_also_runs_a_command_still_files() {
    let script = "touch a.txt && ls";
    let provider = Fake::new(move |request| match results_in(request) {
        0 | 1 => shell("t", &["sh", "-c", script]),
        _ => says("done"),
    });
    let (events, result, filed) = run(provider).await;
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert_eq!(filed[0]["need"], "touch");
    assert!(result.kgraf_shortfall.is_some());
    assert!(
        refusals_seen(&events)
            .iter()
            .all(|t| !t.contains("*** Begin Patch"))
    );
}
