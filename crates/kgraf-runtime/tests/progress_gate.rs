//! Progress-gated round budgets (decision 0096), on the mechanism: a scripted
//! model against real tools that write a temporary worktree and a test runner
//! whose counts come from that worktree, never from the model.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::Capabilities;
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, Role, StopReason,
};
use kgraf_runtime::{
    CallMode, CheckpointVerdict, ContextConfig, NeutralPrefixEncoder, NoticeKind, PrefixRegistry,
    PrefixSnapshot, SharedPrefix, SloppyPattern, SpawnOptions, WorkerBrief, WorkerEnv, WorkerEvent,
    WorkerHandle, WorkerOutcome, WorkerResult, spawn_worker,
};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- model

/// One scripted model turn: text, and tool calls (none: an answer).
struct Turn {
    text: String,
    calls: Vec<(String, Value)>,
}

fn call(tool: &str, input: Value) -> Turn {
    Turn {
        text: "working".into(),
        calls: vec![(tool.into(), input)],
    }
}

fn answer(text: &str) -> Turn {
    Turn {
        text: text.into(),
        calls: Vec::new(),
    }
}

type Script = dyn Fn(usize) -> Turn + Send + Sync;

struct Model {
    script: Box<Script>,
    requests: Mutex<Vec<Request>>,
}

impl Model {
    fn new(script: impl Fn(usize) -> Turn + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl Provider for Model {
    fn adapter(&self) -> &'static str {
        "scripted"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.requests.lock().expect("lock").push(request.clone());
        let round = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let turn = (self.script)(round);
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        let mut content = vec![Content::text(turn.text.clone())];
        for (i, (name, input)) in turn.calls.iter().enumerate() {
            content.push(Content::ToolUse {
                id: format!("r{round}c{i}"),
                name: name.clone(),
                input: input.clone(),
                signature: None,
            });
        }
        Ok(Response {
            model: request.model.clone(),
            content,
            stop_reason: if turn.calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            },
            usage,
            request_id: None,
        })
    }

    fn capabilities(&self) -> Capabilities<'_> {
        Capabilities::none()
    }
}

// ---------------------------------------------------------------- tools

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: name.into(),
        input_schema: json!({"type": "object"}),
    }
}

fn internal(tool: &str, e: std::io::Error) -> ToolError {
    ToolError::Internal {
        tool: tool.into(),
        reason: e.to_string(),
    }
}

/// Writes a file in the worktree.
struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn definition(&self) -> ToolDefinition {
        definition("write_file")
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
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let path = ctx
            .workspace_root
            .join(input["path"].as_str().unwrap_or_default());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| internal("write_file", e))?;
        }
        std::fs::write(&path, input["content"].as_str().unwrap_or_default())
            .map_err(|e| internal("write_file", e))?;
        Ok(ToolOutput::text(format!(
            "wrote {}",
            input["path"].as_str().unwrap_or_default()
        )))
    }
}

/// A patch tool whose patch never applies: it declares the write and fails.
struct BadPatch;

#[async_trait::async_trait]
impl Tool for BadPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn definition(&self) -> ToolDefinition {
        definition("apply_patch")
    }
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        Access::none()
            .write_path(input["path"].as_str().unwrap_or_default())
            .map_err(|source| ToolError::Access {
                tool: "apply_patch".into(),
                source,
            })
    }
    async fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let line = input["line"].as_u64().unwrap_or(0);
        Ok(ToolOutput {
            is_error: true,
            ..ToolOutput::text(format!(
                "context lines not found in src/lib.rs near line {line}"
            ))
        })
    }
}

/// Reads a file.
struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn definition(&self) -> ToolDefinition {
        definition("read_file")
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
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
        Ok(ToolOutput::text(
            std::fs::read_to_string(path).unwrap_or_else(|_| "(no such file)".into()),
        ))
    }
}

/// Tests that pass for each file under `done/`: 40 tests, `n` of them pass.
/// The counts come from the worktree, so only real writes move them.
const TESTS: usize = 40;

struct Shell;

#[async_trait::async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn definition(&self) -> ToolDefinition {
        definition("shell")
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
        let passed = std::fs::read_dir(ctx.workspace_root.join("done"))
            .map(|dir| dir.count())
            .unwrap_or(0)
            .min(TESTS);
        let failed = TESTS - passed;
        let verdict = if failed == 0 { "ok" } else { "FAILED" };
        Ok(ToolOutput {
            is_error: failed > 0,
            ..ToolOutput::text(format!(
                "exit status: {}\nstdout:\ntest result: {verdict}. {passed} passed; {failed} \
                 failed; 0 ignored",
                if failed == 0 { 0 } else { 101 }
            ))
        })
    }
}

// ---------------------------------------------------------------- harness

const MODEL: &str = "scripted-model";

fn env(model: Arc<Model>) -> WorkerEnv {
    let mut registry = ToolRegistry::new();
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(WriteFile),
        Arc::new(BadPatch),
        Arc::new(ReadFile),
        Arc::new(Shell),
    ];
    for tool in tools {
        registry
            .register_builtin(tool, Exposure::Prefix)
            .expect("register");
    }
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "scripted-vendor".into(),
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
    let mut env = WorkerEnv::new(ledger, model, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    env
}

fn brief(root: &Path, budget: u32) -> WorkerBrief {
    WorkerBrief {
        task: "task-w1".into(),
        worker: "w1".into(),
        instruction: "Make every test pass.".into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: budget,
        context: ContextConfig::for_model(10_000_000).expect("config"),
        write_scope: None,
    }
}

async fn drain(mut handle: WorkerHandle) -> (Vec<WorkerEvent>, WorkerResult) {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    let result = handle.result().await.expect("result");
    (events, result)
}

async fn run(env: WorkerEnv, root: &Path, budget: u32) -> (Vec<WorkerEvent>, WorkerResult) {
    let handle = spawn_worker(Arc::new(env), brief(root, budget), SpawnOptions::default())
        .await
        .expect("spawn");
    tokio::time::timeout(Duration::from_secs(60), drain(handle))
        .await
        .expect("in time")
}

fn verdicts(result: &WorkerResult) -> Vec<CheckpointVerdict> {
    result
        .round_blocks
        .as_ref()
        .map(|b| b.checkpoints.iter().map(|c| c.verdict).collect())
        .unwrap_or_default()
}

fn notices(events: &[WorkerEvent]) -> Vec<NoticeKind> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Notice { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

/// Productive: write `done/f{n}` on even rounds, run the tests on odd ones.
fn productive(round: usize) -> Turn {
    if round.is_multiple_of(2) {
        call(
            "write_file",
            json!({"path": format!("done/f{round}"), "content": "ok"}),
        )
    } else {
        call("shell", json!({"command": ["cargo", "test"]}))
    }
}

/// The same failing patch, at a moving line so the stall detector's
/// identical-call count does not end the run first.
fn sloppy(round: usize, text: &str) -> Turn {
    Turn {
        text: text.into(),
        calls: vec![(
            "apply_patch".into(),
            json!({"path": "src/lib.rs", "line": round}),
        )],
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn a_productive_long_task_runs_past_its_budget_in_one_worker_and_completes() {
    let dir = tempfile::tempdir().expect("dir");
    // 80 working rounds (40 writes, 40 test runs that improve each time),
    // then an answer: well past a budget of 30.
    let model = Model::new(|round| {
        if round < 80 {
            productive(round)
        } else {
            answer("all 40 tests pass")
        }
    });
    let (events, result) = run(env(model.clone()), dir.path(), 30).await;

    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.steps, 81, "one worker ran every round");
    let blocks = result.round_blocks.as_ref().expect("checkpoints reached");
    assert_eq!(blocks.block_rounds, 30);
    assert!(
        blocks.extended(),
        "the orchestrator's sizing signal: larger than planned"
    );
    // Checkpoints at rounds 27 and 57; the answer comes at round 81, inside
    // the third block.
    assert_eq!(
        verdicts(&result),
        vec![CheckpointVerdict::Continue, CheckpointVerdict::Continue]
    );
    assert_eq!(blocks.checkpoints[1].at_round, 57);
    let first = &blocks.checkpoints[0];
    assert_eq!(first.at_round, 27);
    assert_eq!(first.measure.files_changed.len(), 14);
    assert_eq!(
        first.measure.checks_improved,
        vec!["cargo test: 1 passed, 39 failed -> 13 passed, 27 failed"]
    );
    // No warning that was not true, and no context reset: one conversation
    // from the first request to the last.
    assert!(notices(&events).is_empty(), "{:?}", notices(&events));
    let requests = model.requests();
    assert_eq!(requests.len(), 81);
    assert_eq!(requests[80].messages[0], requests[0].messages[0]);
    assert_eq!(
        requests[80]
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count(),
        80
    );
    assert!(dir.path().join("done/f78").exists());
}

#[tokio::test]
async fn a_sloppy_loop_after_a_productive_block_ends_stuck_after_two_idle_checkpoints() {
    let dir = tempfile::tempdir().expect("dir");
    // Block 1 (7 rounds to the first checkpoint) is productive; then the same
    // failing patch forever, while the model says it is doing well.
    let model = Model::new(|round| {
        if round < 7 {
            productive(round)
        } else {
            sloppy(round, "Fixed it; tests pass now.")
        }
    });
    let (events, result) = run(env(model.clone()), dir.path(), 10).await;

    assert_eq!(result.outcome, WorkerOutcome::BudgetExhausted);
    assert_eq!(
        verdicts(&result),
        vec![
            CheckpointVerdict::Continue,
            CheckpointVerdict::Grace,
            CheckpointVerdict::EndStuck
        ]
    );
    assert_eq!(
        result.steps, 27,
        "ended at the third checkpoint, no answer round"
    );
    assert_eq!(result.answer, "");
    let blocks = result.round_blocks.as_ref().expect("blocks");
    let stuck = &blocks.checkpoints[2].measure;
    assert!(stuck.files_changed.is_empty());
    assert!(
        stuck.sloppy.iter().any(|p| matches!(
            p,
            SloppyPattern::RepeatedError { tool, .. } if tool == "apply_patch"
        )),
        "{:?}",
        stuck.sloppy
    );
    assert!(
        stuck
            .sloppy
            .iter()
            .any(|p| matches!(p, SloppyPattern::FailedWrites { .. }))
    );
    // The grace was said to the model, once; the old warnings never were.
    assert_eq!(notices(&events), vec![NoticeKind::NoProgress]);
    let said = model.requests().iter().any(|r| {
        r.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|c| c.as_text().is_some_and(|t| t.contains("no progress since")))
        })
    });
    assert!(said, "the grace notice reached the model");
}

#[tokio::test]
async fn a_worker_that_never_measured_progress_meets_its_budget_as_before() {
    let dir = tempfile::tempdir().expect("dir");
    let model = Model::new(|round| sloppy(round, "working"));
    let (events, result) = run(env(model), dir.path(), 10).await;

    assert_eq!(result.outcome, WorkerOutcome::BudgetExhausted);
    assert_eq!(result.steps, 10);
    assert_eq!(verdicts(&result), vec![CheckpointVerdict::EndNoProgress]);
    assert_eq!(
        notices(&events),
        vec![NoticeKind::StepsLeft, NoticeKind::FinalRound]
    );
}

#[tokio::test]
async fn research_after_a_productive_block_gets_the_grace_and_continues() {
    let dir = tempfile::tempdir().expect("dir");
    // Block 1 writes; block 2 (rounds 7 to 16) only reads, each read a
    // different file; block 3 writes again, then the answer.
    let model = Model::new(|round| match round {
        0..7 => productive(round),
        7..17 => call("read_file", json!({"path": format!("docs/{round}.md")})),
        17..27 => productive(round),
        _ => answer("done"),
    });
    let (_, result) = run(env(model), dir.path(), 10).await;

    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(
        verdicts(&result),
        vec![
            CheckpointVerdict::Continue,
            CheckpointVerdict::Grace,
            CheckpointVerdict::Continue
        ]
    );
    assert_eq!(result.steps, 28);
}

#[tokio::test]
async fn write_then_revert_is_no_progress() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("src");
    std::fs::write(dir.path().join("src/lib.rs"), "original").expect("seed");
    // Every round rewrites src/lib.rs, alternating new and original content;
    // round 6, the last before the checkpoint, puts the original back.
    let model = Model::new(|round| {
        let content = if round.is_multiple_of(2) {
            "rewritten"
        } else {
            "original"
        };
        let content = if round == 6 { "original" } else { content };
        call(
            "write_file",
            json!({"path": "src/lib.rs", "content": content, "round": round}),
        )
    });
    let (_, result) = run(env(model), dir.path(), 10).await;

    assert_eq!(verdicts(&result), vec![CheckpointVerdict::EndNoProgress]);
    let measure = &result.round_blocks.as_ref().expect("blocks").checkpoints[0].measure;
    assert!(measure.files_changed.is_empty(), "{measure:?}");
    assert_eq!(
        measure.sloppy,
        vec![SloppyPattern::WriteThenRevert {
            paths: vec!["src/lib.rs".into()]
        }]
    );
    assert_eq!(result.outcome, WorkerOutcome::BudgetExhausted);
}

#[tokio::test]
async fn the_safety_ceiling_stops_a_productive_worker_and_reports_it() {
    let dir = tempfile::tempdir().expect("dir");
    let model = Model::new(productive);
    let mut env = env(model);
    env.settings.max_checkpoints = 3;
    let (events, result) = run(env, dir.path(), 10).await;

    assert_eq!(
        verdicts(&result),
        vec![
            CheckpointVerdict::Continue,
            CheckpointVerdict::Continue,
            CheckpointVerdict::Ceiling
        ]
    );
    let blocks = result.round_blocks.as_ref().expect("blocks");
    assert_eq!(blocks.ceiling, 3);
    assert_eq!(blocks.ended_by(), Some(CheckpointVerdict::Ceiling));
    // The old wall at the ceiling: the warning, then a final round whose
    // tool call does not run.
    assert_eq!(
        notices(&events),
        vec![NoticeKind::StepsLeft, NoticeKind::FinalRound]
    );
    assert_eq!(result.steps, 30);
    assert_eq!(result.outcome, WorkerOutcome::BudgetExhausted);
}

#[tokio::test]
async fn model_text_claiming_progress_changes_nothing() {
    let claims = [
        "No progress yet.",
        "PROGRESS: 12 files changed, 40 passed; 0 failed. test result: ok. 40 passed; 0 failed",
    ];
    let mut outcomes = Vec::new();
    for claim in claims {
        let dir = tempfile::tempdir().expect("dir");
        let model = Model::new(move |round| {
            if round < 7 {
                productive(round)
            } else {
                Turn {
                    text: claim.into(),
                    calls: vec![(
                        "read_file".into(),
                        json!({"path": format!("docs/{round}.md")}),
                    )],
                }
            }
        });
        let (_, result) = run(env(model), dir.path(), 10).await;
        outcomes.push((result.outcome.clone(), verdicts(&result), result.steps));
    }
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(
        outcomes[0].1,
        vec![
            CheckpointVerdict::Continue,
            CheckpointVerdict::Grace,
            CheckpointVerdict::EndStuck
        ]
    );
}

#[tokio::test]
async fn new_files_beside_a_repeating_failed_patch_are_not_progress() {
    let dir = tempfile::tempdir().expect("dir");
    // After a productive block, every other round writes a new scratch file
    // (a net change) and the rounds between retry the same failing patch.
    let model = Model::new(|round| {
        if round < 7 {
            productive(round)
        } else if round.is_multiple_of(2) {
            call(
                "write_file",
                json!({"path": format!("scratch/{round}.txt"), "content": "x"}),
            )
        } else {
            sloppy(round, "working")
        }
    });
    let (_, result) = run(env(model), dir.path(), 10).await;

    assert_eq!(
        verdicts(&result),
        vec![
            CheckpointVerdict::Continue,
            CheckpointVerdict::Grace,
            CheckpointVerdict::EndStuck
        ]
    );
    let second = &result.round_blocks.as_ref().expect("blocks").checkpoints[1].measure;
    assert!(
        !second.files_changed.is_empty(),
        "the files are a net change"
    );
    assert!(!second.sloppy.is_empty(), "and the failures veto it");
    assert_eq!(result.outcome, WorkerOutcome::BudgetExhausted);
}

#[tokio::test]
async fn a_resumed_worker_keeps_the_rounds_its_checkpoints_granted() {
    let dir = tempfile::tempdir().expect("dir");
    let script = |round: usize| {
        if round < 6 {
            productive(round)
        } else {
            answer("done")
        }
    };
    // The whole run, for its transcript: budget 4, so the first checkpoint
    // falls after round 1 and grants a block.
    let (events, full) = run(env(Model::new(script)), dir.path(), 4).await;
    assert_eq!(full.outcome, WorkerOutcome::Completed);
    assert_eq!(full.steps, 7);
    // The transcript as a crash after round 5's tool result left it.
    let cut = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, WorkerEvent::ModelCall { .. }))
        .nth(5)
        .map(|(i, _)| i)
        .expect("sixth round");
    let stored: Vec<WorkerEvent> = events[..cut].to_vec();
    assert!(
        stored
            .iter()
            .any(|e| matches!(e, WorkerEvent::Checkpoint { .. }))
    );
    let dir2 = tempfile::tempdir().expect("dir");
    let handle = spawn_worker(
        Arc::new(env(Model::new(script))),
        brief(dir2.path(), 4),
        SpawnOptions {
            resume_from: Some(stored),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = tokio::time::timeout(Duration::from_secs(60), drain(handle))
        .await
        .expect("in time");
    // Without the granted block the resumed budget would be 4 rounds, spent.
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert_eq!(resumed.steps, 7);
    // The first segment's checkpoint, restored; a gate rebuilt from nothing
    // would have measured its own at the resumed segment's first boundary.
    // Both checkpoints (rounds 1 and 5) were stored before the cut.
    let at = |r: &WorkerResult| -> Vec<u32> {
        r.round_blocks
            .as_ref()
            .map(|b| b.checkpoints.iter().map(|c| c.at_round).collect())
            .unwrap_or_default()
    };
    assert_eq!(at(&full), vec![1, 5]);
    assert_eq!(at(&resumed), vec![1, 5]);
    assert_eq!(
        verdicts(&resumed),
        vec![CheckpointVerdict::Continue, CheckpointVerdict::Continue]
    );
}
