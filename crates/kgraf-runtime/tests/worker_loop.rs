//! The worker loop against a fake provider that implements the real
//! `Provider` and `Streaming` traits and settles its `CallToken` as an adapter does.
//! No network; the live check against a running speaker is the CLI's `worker`
//! subcommand.

use kgraf_core::{NodeId, Protocol, TokenUsage};
use kgraf_provider::capability::{Capabilities, StreamEvent, Streaming};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Message, Provider, ProviderError, Request, Response, Role,
    Settlement, StopReason,
};
use kgraf_runtime::{
    CallMode, CancelPhase, ContextConfig, INTERRUPTED_RESULT, NeutralPrefixEncoder, NoticeKind,
    PointSource, PrefixCheckError, PrefixRegistry, PrefixSnapshot, ScopeFence, SessionMasker,
    SharedPrefix, SpawnOptions, TranscriptBuilder, WorkerBrief, WorkerEnv, WorkerError,
    WorkerEvent, WorkerHandle, WorkerOutcome, WorkerResult, spawn_worker,
};
use kgraf_tools::approval::{ApprovalChain, ApprovalHook, HookDecision, PendingCall};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- fake provider

#[derive(Clone, Debug, Default)]
struct Round {
    text: String,
    calls: Vec<(String, String, Value)>,
    stop: Option<StopReason>,
    usage: TokenUsage,
    deltas: usize,
    delay: Duration,
    /// Stop delivering and wait forever after this many deltas.
    hang_after: Option<usize>,
    /// Stream: wait forever right after `Started`, before any usage.
    hang_before_usage: bool,
    /// Fail after the request may have billed, closing the call as an adapter does.
    fail: bool,
    /// Stream: settle the call before any output, so a trace write after it
    /// is refused (an adapter that settles early).
    settle_early: bool,
    /// The provider token attached to each call this round makes.
    signature: Option<kgraf_provider::Signature>,
    /// The provider token attached to this round's text part.
    text_signature: Option<kgraf_provider::Signature>,
    /// The raw finish string the provider's wire carried, when it has one.
    finish: Option<String>,
    /// Reasoning text the round returns, ahead of its text.
    reasoning: String,
}

impl Round {
    fn answer(text: &str) -> Self {
        Self {
            text: text.into(),
            stop: Some(StopReason::EndTurn),
            usage: usage(100, 10),
            deltas: 1,
            ..Self::default()
        }
    }

    fn tool(calls: Vec<(&str, &str, Value)>) -> Self {
        Self {
            text: "working".into(),
            calls: calls
                .into_iter()
                .map(|(i, n, v)| (i.to_owned(), n.to_owned(), v))
                .collect(),
            stop: Some(StopReason::ToolUse),
            usage: usage(100, 10),
            deltas: 1,
            ..Self::default()
        }
    }
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        uncached_input: input,
        text_output: output,
        ..TokenUsage::default()
    }
}

type Script = dyn Fn(&Request) -> Round + Send + Sync;

struct Fake {
    script: Box<Script>,
    requests: Mutex<Vec<Request>>,
    streams: bool,
    /// What `Provider::resends_reasoning` answers.
    resends_reasoning: bool,
    /// Whether `Provider::undelivered_output_tokens` measures the reply, as a
    /// Chat Completions adapter does, or answers `None`.
    counts_delivery: bool,
    /// What `Provider::turns_reasoning_off` answers.
    turns_off: bool,
    /// Whether an answer to a reasoning-off request that holds reasoning is
    /// refused after it is settled, as the Chat Completions adapter does.
    checks_off: bool,
}

impl Fake {
    /// A provider whose route can turn reasoning off.
    fn turning_off(script: impl Fn(&Request) -> Round + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            streams: false,
            resends_reasoning: false,
            counts_delivery: false,
            turns_off: true,
            checks_off: false,
        })
    }

    /// A route that can turn reasoning off, ignores the request, and checks
    /// every answer to it: a server that does not know the setting.
    fn turning_off_but_ignoring(
        script: impl Fn(&Request) -> Round + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            streams: false,
            resends_reasoning: false,
            counts_delivery: false,
            turns_off: true,
            checks_off: true,
        })
    }

    fn new(streams: bool, script: impl Fn(&Request) -> Round + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            streams,
            resends_reasoning: false,
            counts_delivery: false,
            turns_off: false,
            checks_off: false,
        })
    }

    /// A provider whose replies carry all the output they count, so a reply
    /// that carries less is measured (`Response::undelivered_output_floor`).
    fn counting(script: impl Fn(&Request) -> Round + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            streams: false,
            resends_reasoning: false,
            counts_delivery: true,
            turns_off: false,
            checks_off: false,
        })
    }

    /// A provider that declares it sends reasoning back in the next request.
    fn resending(script: impl Fn(&Request) -> Round + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            streams: false,
            resends_reasoning: true,
            counts_delivery: false,
            turns_off: false,
            checks_off: false,
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }

    fn round(&self, request: &Request) -> Round {
        self.requests.lock().expect("lock").push(request.clone());
        (self.script)(request)
    }
}

fn response(request: &Request, round: &Round) -> Response {
    let mut content = Vec::new();
    if !round.reasoning.is_empty() {
        content.push(Content::Reasoning {
            text: round.reasoning.clone(),
        });
    }
    content.push(Content::Text {
        text: round.text.clone(),
        cache_breakpoint: false,
        signature: round.text_signature.clone(),
    });
    for (id, name, input) in &round.calls {
        content.push(Content::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            signature: round.signature.clone(),
        });
    }
    Response {
        finish_reason: round.finish.clone(),
        model: request.model.clone(),
        content,
        stop_reason: round.stop.unwrap_or(StopReason::EndTurn),
        usage: round.usage.clone(),
        request_id: None,
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

    fn resends_reasoning(&self) -> bool {
        self.resends_reasoning
    }

    fn turns_reasoning_off(&self) -> bool {
        self.turns_off
    }

    fn undelivered_output_tokens(&self, response: &Response) -> Option<u64> {
        self.counts_delivery
            .then(|| response.undelivered_output_floor())
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let round = self.round(request);
        if round.fail {
            let error = ProviderError::Transport {
                adapter: "fake",
                reason: "connection reset".into(),
            };
            token.close_on(&error, None)?;
            return Err(error);
        }
        if round.hang_after.is_some() || round.hang_before_usage {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(round.delay).await;
        token.settle(&round.usage, None)?;
        let answer = response(request, &round);
        if self.checks_off {
            kgraf_provider::check_reasoning_off("fake", request, &answer)?;
        }
        Ok(answer)
    }

    fn capabilities(&self) -> Capabilities<'_> {
        if self.streams {
            Capabilities::none().with_streaming(self)
        } else {
            Capabilities::none()
        }
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
            request_id: Some("req-fake-1".into()),
        });
        if round.hang_before_usage {
            std::future::pending::<()>().await;
        }
        // The input side first, as Anthropic's message_start does.
        sink(StreamEvent::Usage {
            usage: TokenUsage {
                text_output: 0,
                reasoning_output: 0,
                ..round.usage.clone()
            },
        });
        if round.settle_early {
            token.settle(&round.usage, None)?;
        }
        let pieces = round.deltas.max(1);
        let chars: Vec<char> = round.text.chars().collect();
        let size = chars.len().div_ceil(pieces).max(1);
        for (i, piece) in chars.chunks(size).enumerate() {
            if round.hang_after == Some(i) {
                std::future::pending::<()>().await;
            }
            sink(StreamEvent::Delta {
                content: Content::text(piece.iter().collect::<String>()),
            });
            if round.delay.is_zero() {
                tokio::task::yield_now().await;
            } else {
                tokio::time::sleep(round.delay).await;
            }
        }
        if round.hang_after.is_some() {
            std::future::pending::<()>().await;
        }
        sink(StreamEvent::Usage {
            usage: round.usage.clone(),
        });
        sink(StreamEvent::Finished {
            stop_reason: round.stop.unwrap_or(StopReason::EndTurn),
        });
        if !round.settle_early {
            token.settle(&round.usage, None)?;
        }
        Ok(response(request, &round))
    }
}

/// Model rounds already in the request: one assistant message per round.
fn round_of(request: &Request) -> usize {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count()
}

fn instruction_of(request: &Request) -> String {
    request.messages[0].content[0]
        .as_text()
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------- fake tools

/// Writes a file inside the worker's worktree.
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
        let path = input["path"].as_str().unwrap_or_default();
        Access::none()
            .write_path(path)
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(fail)?;
        }
        std::fs::write(&path, input["content"].as_str().unwrap_or_default()).map_err(fail)?;
        Ok(ToolOutput::text("written"))
    }
}

/// Reports a wait through its `ProgressSink`, then resumes (decision 0067):
/// proves the worker loop's own wiring, not only
/// `ShellTool`'s: `execute`/`start_ready`'s progress channel must reach
/// `self.emit` and produce a `WorkerEvent::Waiting`/`Resumed` pair, exactly
/// as `kgraf-tools::shell`'s own test proves `ShellTool` reports through the
/// sink it is given).
struct Waiter;

#[async_trait::async_trait]
impl Tool for Waiter {
    fn name(&self) -> &str {
        "waiter"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "waiter".into(),
            description: "Waits, visibly".into(),
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
        ctx.progress.waiting("waiting for a machine slot");
        tokio::time::sleep(Duration::from_millis(10)).await;
        ctx.progress.resumed();
        Ok(ToolOutput::text("waited"))
    }
}

/// Reads or writes a graph node for `ms`, recording overlap.
#[derive(Default)]
struct Probe {
    running: AtomicUsize,
    max_running: AtomicUsize,
    calls: AtomicUsize,
    spans: Mutex<Vec<(String, Instant, Instant)>>,
}

#[async_trait::async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "probe".into(),
            description: "Touch a node".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let node = input["node"].as_str().unwrap_or("none").to_owned();
        Ok(if input["write"].as_bool().unwrap_or(false) {
            Access::none().write_node(node)
        } else {
            Access::none().read_node(node)
        })
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(now, Ordering::SeqCst);
        let start = Instant::now();
        let ms = input["ms"].as_u64().unwrap_or(0);
        if input["ignore_cancel"].as_bool().unwrap_or(false) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        tokio::select! {
            () = ctx.cancel.cancelled() => {
                self.running.fetch_sub(1, Ordering::SeqCst);
                return Err(ToolError::Cancelled { tool: "probe".into() });
            }
            () = tokio::time::sleep(Duration::from_millis(ms)) => {}
        }
        self.running.fetch_sub(1, Ordering::SeqCst);
        self.spans.lock().expect("lock").push((
            input["label"].as_str().unwrap_or_default().to_owned(),
            start,
            Instant::now(),
        ));
        let lines = input["lines"].as_u64().unwrap_or(1) as usize;
        Ok(ToolOutput::text(
            vec![format!("out {}", input["label"]); lines].join("\n"),
        ))
    }
}

// ---------------------------------------------------------------- harness

const MODEL: &str = "fake-model";

fn snapshot(registry: &ToolRegistry, system: &str) -> PrefixSnapshot {
    PrefixSnapshot::new(
        SharedPrefix {
            vendor: "fake-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text(system)],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("snapshot")
}

fn env_with(provider: Arc<Fake>, tools: Vec<Arc<dyn Tool>>, mode: CallMode) -> WorkerEnv {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry
            .register_builtin(tool, Exposure::Prefix)
            .expect("register");
    }
    let prefix = snapshot(&registry, "You are a kGraf worker.");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    // Each test is its own process-wide world: tests run in one process and use
    // different tool sets under one fake vendor and model.
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = mode;
    env
}

fn brief(worker: &str, instruction: &str, root: &Path, budget: u32) -> WorkerBrief {
    WorkerBrief {
        task: format!("task-{worker}").as_str().into(),
        worker: worker.into(),
        instruction: instruction.into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: budget,
        context: ContextConfig::for_model(200_000).expect("config"),
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

fn with_timeout<F: std::future::Future>(f: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout(Duration::from_secs(10), f)
}

// ---------------------------------------------------------------- tests

/// Each round's ledger row names its parent call, kind and role,
/// what it was given (slice ids and the prefix hash), the tool round its
/// request carries, and, streamed, its time to first token.
#[tokio::test]
async fn every_round_is_traced_in_the_ledger() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![
            ("t1", "write_file", json!({"path": "a.txt", "content": "a"})),
            ("t2", "write_file", json!({"path": "b.txt", "content": "b"})),
        ]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let dispatcher = env
        .ledger
        .begin("orchestrator", Protocol::Additive, MODEL, "orch", "task-w1")
        .expect("dispatching call");
    let parent = dispatcher.id().clone();
    dispatcher.settle(&usage(1, 1), None).expect("settle");
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "write two files", worktree.path(), 6),
        SpawnOptions {
            parent_call: Some(parent.clone()),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    let rows: Vec<_> = result
        .calls
        .iter()
        .map(|c| env.ledger.call(c).expect("read").expect("row"))
        .collect();
    assert_eq!(rows.len(), 2);
    let digest = env.prefix.digest().as_str().to_owned();
    for row in &rows {
        assert_eq!(row.trace.kind.as_deref(), Some("worker_round"));
        assert_eq!(row.trace.role.as_deref(), Some("worker"));
        assert_eq!(
            row.trace.given_prefix_hash.as_deref(),
            Some(digest.as_str())
        );
        assert!(row.trace.ttft_ms.is_some(), "streamed rounds carry a TTFT");
    }
    assert_eq!(rows[0].trace.parent_call.as_ref(), Some(&parent));
    assert_eq!(rows[1].trace.parent_call.as_ref(), Some(&rows[0].id));
    assert!(rows[0].trace.tool_rounds.is_empty());
    assert_eq!(
        rows[1].trace.tool_rounds,
        vec![vec!["write_file".to_owned(), "write_file".to_owned()]]
    );
    assert!(env.ledger.verify().expect("verify").ok);
}

/// Each round's `Stop` records the provider's normalized stop reason and,
/// where the adapter read one, the raw finish string; the result carries the
/// last stop and the tool-call count. A transcript line written before the
/// field existed still parses.
#[tokio::test]
async fn a_stop_records_the_raw_finish_string_and_the_result_the_last_stop() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round {
            finish: Some("tool_calls".into()),
            ..Round::tool(vec![
                ("t1", "write_file", json!({"path": "a.txt", "content": "a"})),
                ("t2", "write_file", json!({"path": "b.txt", "content": "b"})),
            ])
        },
        _ => Round {
            finish: Some("stop".into()),
            ..Round::answer("done")
        },
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "write two files", worktree.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    let stops: Vec<(StopReason, Option<String>)> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Stop {
                reason,
                finish_reason,
            } => Some((*reason, finish_reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        stops,
        vec![
            (StopReason::ToolUse, Some("tool_calls".to_owned())),
            (StopReason::EndTurn, Some("stop".to_owned())),
        ]
    );
    assert_eq!(result.last_stop, Some(StopReason::EndTurn));
    assert_eq!(result.tool_calls, 2);

    // The written line names the field; an older line without it parses.
    let stop_line = serde_json::to_value(WorkerEvent::Stop {
        reason: StopReason::EndTurn,
        finish_reason: Some("stop".into()),
    })
    .expect("write");
    assert_eq!(stop_line["type"], "stop");
    assert_eq!(stop_line["reason"], "end_turn");
    assert_eq!(stop_line["finish_reason"], "stop");
    let old: WorkerEvent =
        serde_json::from_str(r#"{"type":"stop","reason":"tool_use"}"#).expect("old line");
    assert_eq!(
        old,
        WorkerEvent::Stop {
            reason: StopReason::ToolUse,
            finish_reason: None
        }
    );
}

/// A provider that pauses the turn for a server-side tool (Anthropic's
/// `pause_turn`) is a loud failure: the rebuilt transcript would drop the
/// server tool's blocks. The paused round is still billed, and no second
/// request is sent.
#[tokio::test]
async fn a_paused_turn_fails_loud_instead_of_resending_a_damaged_turn() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |request| match round_of(request) {
        0 => Round {
            text: "still searching".into(),
            stop: Some(StopReason::PauseTurn),
            usage: usage(100, 10),
            deltas: 1,
            ..Round::default()
        },
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "search", dir.path(), 5),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    match &result.outcome {
        WorkerOutcome::Failed { error } => {
            assert!(
                error.contains("server tool turns are not supported"),
                "{error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(result.calls.len(), 1);
    let row = env
        .ledger
        .call(&result.calls[0])
        .expect("read")
        .expect("row");
    assert_eq!(row.settlement, Settlement::Counted);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            WorkerEvent::Stop {
                reason: StopReason::PauseTurn,
                ..
            }
        )),
        "no paused Stop is recorded"
    );
}

/// A refused trace write was discarded. Here the adapter
/// settles before its first delta, so the time-to-first-token write is refused;
/// the run completes and the refusal is on the result.
#[tokio::test]
async fn a_refused_trace_write_is_reported_on_the_result() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |_| Round {
        settle_early: true,
        ..Round::answer("done")
    });
    let env = Arc::new(env_with(provider, vec![], CallMode::Stream));
    let handle = spawn_worker(
        env,
        brief("w1", "answer", worktree.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(
        result.trace_failures.len(),
        1,
        "{:?}",
        result.trace_failures
    );
    assert!(
        result.trace_failures[0].contains("time to first token"),
        "{:?}",
        result.trace_failures
    );
}

/// A resumed run lost the tool round of the earlier
/// segment's last round. Segment 1 spends its budget on a final round of tool
/// calls (refused, never answered); the resume's first row records that round.
#[tokio::test]
async fn a_resume_records_the_earlier_segments_last_tool_round() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "write_file",
            json!({"path": "a", "content": "a"}),
        )]),
        1 => Round::tool(vec![(
            "t2",
            "write_file",
            json!({"path": "b", "content": "b"}),
        )]),
        _ => Round::answer("done"),
    });
    let mut env = env_with(provider, vec![Arc::new(WriteFile)], CallMode::Stream);
    // The old wall (decision 0096): the write in round 1 is progress, and a
    // gate allowed past its first checkpoint would grant another block.
    env.settings.max_checkpoints = 1;
    let env = Arc::new(env);
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "write", worktree.path(), 2),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(first.outcome, WorkerOutcome::BudgetExhausted);
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "write", worktree.path(), 6),
        SpawnOptions {
            resume_from: Some(events),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    let new_call = resumed
        .calls
        .iter()
        .find(|c| !first.calls.contains(c))
        .expect("a new call");
    let row = env.ledger.call(new_call).expect("read").expect("row");
    assert_eq!(row.trace.tool_rounds, vec![vec!["write_file".to_owned()]]);
}

#[tokio::test]
async fn a_worker_makes_a_small_change_in_its_own_worktree_and_returns_a_structured_result() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "write_file",
            json!({"path": "src/greeting.txt", "content": "hello"}),
        )]),
        _ => Round::answer("wrote src/greeting.txt"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env.clone(),
        brief(
            "w1",
            "Create src/greeting.txt containing hello",
            worktree.path(),
            6,
        ),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");

    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.answer, "wrote src/greeting.txt");
    assert_eq!(result.steps, 2);
    assert_eq!(
        result.changed_paths,
        vec![std::path::PathBuf::from("src/greeting.txt")]
    );
    assert_eq!(
        std::fs::read_to_string(worktree.path().join("src/greeting.txt")).expect("file"),
        "hello"
    );
    // Every call is a counted ledger row, attributed to the worker and task.
    let rows = env.ledger.calls().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        result.calls,
        rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
    );
    assert!(rows.iter().all(|r| r.settlement == Settlement::Counted
        && r.agent_id == "w1"
        && r.task_id == "task-w1"));
    // The approval decision is part of the transcript.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Approval { .. }))
    );
    // The second request carried the tool result back to the model.
    let second = &provider.requests()[1];
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.content.iter().any(|c| matches!(
                c,
                Content::ToolResult { content, is_error: false, .. } if content == "written"
            )))
    );
}

/// A paid round is stored before the next request leaves. The reader here is a
/// slow transcript writer: it holds the first round's tool result back from
/// its store until the next request arrives, or for `HOLD` if none does. Each
/// request records what the store held when it left. A loop that sent its next
/// request while the reader still held the result would let a process kill at
/// that moment lose a round that was paid and answered, and the resume would
/// pay for it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_request_leaves_before_the_reader_has_stored_every_earlier_event() {
    const HOLD: Duration = Duration::from_millis(800);
    let worktree = tempfile::tempdir().expect("worktree");
    let stored: Arc<Mutex<Vec<WorkerEvent>>> = Arc::default();
    // Per request: (its round, the events stored when it left).
    type AtRequest = Vec<(usize, Vec<WorkerEvent>)>;
    let at_request: Arc<Mutex<AtRequest>> = Arc::default();
    let request_seen = Arc::new(tokio::sync::Notify::new());
    let provider = {
        let stored = stored.clone();
        let at_request = at_request.clone();
        let request_seen = request_seen.clone();
        Fake::new(false, move |request| {
            let round = round_of(request);
            let snapshot = stored.lock().expect("lock").clone();
            at_request.lock().expect("lock").push((round, snapshot));
            // Only a request after the held result releases the hold; a
            // stored permit from the first request would release it early.
            if round > 0 {
                request_seen.notify_one();
            }
            match round {
                0 => Round::tool(vec![(
                    "t1",
                    "write_file",
                    json!({"path": "src/a.txt", "content": "a"}),
                )]),
                1 => Round::tool(vec![(
                    "t2",
                    "write_file",
                    json!({"path": "src/b.txt", "content": "b"}),
                )]),
                _ => Round::answer("done"),
            }
        })
    };
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ));
    let mut handle = spawn_worker(
        env,
        brief("w1", "Write a and b", worktree.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let reader = async {
        let mut held_once = false;
        while let Some(event) = handle.next_event().await {
            if !held_once && matches!(event, WorkerEvent::ToolCallFinished { .. }) {
                held_once = true;
                let _ = tokio::time::timeout(HOLD, request_seen.notified()).await;
            }
            stored.lock().expect("lock").push(event);
        }
        handle.result().await.expect("result")
    };
    let result = with_timeout(reader).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");

    let at_request = at_request.lock().expect("lock").clone();
    assert_eq!(at_request.len(), 3, "three requests");
    for (round, snapshot) in &at_request {
        let calls = snapshot
            .iter()
            .filter(|e| matches!(e, WorkerEvent::ModelCall { .. }))
            .count();
        let finished = snapshot
            .iter()
            .filter(|e| matches!(e, WorkerEvent::ToolCallFinished { .. }))
            .count();
        assert_eq!(
            (calls, finished),
            (round + 1, *round),
            "request {round} left before every earlier round, and its own \
             `ModelCall`, was stored: {snapshot:#?}"
        );
    }
}

/// A provider's opaque token on a tool call reaches the event log and the next
/// request, so a resumed run sends it back.
#[tokio::test]
async fn a_tool_call_signature_reaches_the_event_and_the_next_request() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round {
            signature: Some(kgraf_provider::Signature::new("c2ln", "adapter:model")),
            ..Round::tool(vec![(
                "t1",
                "write_file",
                json!({"path": "a.txt", "content": "x"}),
            )])
        },
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "Write a.txt", worktree.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::ToolCallStarted { signature: Some(s), .. }
            if s.value == "c2ln" && s.is_from("adapter:model")
    )));
    let second = &provider.requests()[1];
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.content.iter().any(|c| matches!(
                c,
                Content::ToolUse { signature: Some(s), .. }
                    if s.value == "c2ln" && s.is_from("adapter:model")
            )))
    );
}

/// The token on a text part reaches the round's settled message and the next
/// request, so a resumed run sends it back on the same part.
#[tokio::test]
async fn a_text_signature_reaches_the_settled_message_and_the_next_request() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round {
            text: "checking".into(),
            text_signature: Some(kgraf_provider::Signature::new("dGV4dA==", "adapter:model")),
            ..Round::tool(vec![(
                "t1",
                "write_file",
                json!({"path": "a.txt", "content": "x"}),
            )])
        },
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "Write a.txt", worktree.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::AssistantMessage { signature: Some(s), .. }
            if s.value == "dGV4dA==" && s.is_from("adapter:model")
    )));
    let second = &provider.requests()[1];
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.content.iter().any(|c| matches!(
                c,
                Content::Text { signature: Some(s), .. } if s.value == "dGV4dA=="
            )))
    );
}

/// Decision 0067: a tool call's
/// `ProgressSink` report reaches the worker loop's own event emission, as a
/// `WorkerEvent::Waiting` then a matching `WorkerEvent::Resumed`, both
/// carrying the same call id `ToolCallStarted`/`ToolCallFinished` use. This
/// is the loop-level half of the fix; `kgraf-tools::shell`'s own test proves
/// `ShellTool` reports into whatever sink it is given.
#[tokio::test]
async fn a_tool_calls_progress_report_becomes_a_waiting_and_resumed_event() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "waiter", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Waiter)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "wait, visibly", worktree.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    let started_id = events
        .iter()
        .find_map(|e| match e {
            WorkerEvent::ToolCallStarted { call_id, tool, .. } if tool == "waiter" => {
                Some(call_id.clone())
            }
            _ => None,
        })
        .expect("the waiter call started");
    let waiting_at = events
        .iter()
        .position(|e| {
            matches!(e, WorkerEvent::Waiting { call_id, reason }
                if *call_id == started_id && reason == "waiting for a machine slot")
        })
        .expect("a Waiting event for this call, with the design's own wording");
    let resumed_at = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::Resumed { call_id } if *call_id == started_id))
        .expect("a matching Resumed event for the same call");
    assert!(
        resumed_at > waiting_at,
        "resumed must follow waiting: {events:#?}"
    );
    let finished_at = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::ToolCallFinished { call_id, .. } if *call_id == started_id))
        .expect("the call finished");
    assert!(
        resumed_at < finished_at,
        "resumed must arrive before the call's own finish: {events:#?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ten_concurrent_workers_on_one_thread_all_make_progress_before_any_finishes() {
    // One executor thread is the harshest case: a worker that never yields would
    // starve the other nine.
    let provider = Fake::new(true, |request| {
        let mut round = Round::answer(&"x".repeat(200));
        round.deltas = 40;
        round.delay = Duration::from_millis(1);
        let _ = request;
        round
    });
    let env = Arc::new(env_with(provider, vec![], CallMode::Stream));
    let dir = tempfile::tempdir().expect("dir");
    let begun = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..10 {
        let handle = spawn_worker(
            env.clone(),
            brief(&format!("w{i}"), &format!("task {i}"), dir.path(), 4),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        tasks.push(tokio::spawn(async move {
            let mut handle = handle;
            let mut first_delta = None;
            while let Some(event) = handle.next_event().await {
                if first_delta.is_none() && matches!(event, WorkerEvent::TextDelta { .. }) {
                    first_delta = Some(begun.elapsed());
                }
            }
            let result = handle.result().await.expect("result");
            (first_delta, begun.elapsed(), result)
        }));
    }
    let mut firsts = Vec::new();
    let mut ends = Vec::new();
    for task in tasks {
        let (first, end, result) = with_timeout(task).await.expect("in time").expect("join");
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert_eq!(result.answer.len(), 200);
        firsts.push(first.expect("every worker streamed"));
        ends.push(end);
    }
    let last_first = firsts.iter().max().expect("ten");
    let first_end = ends.iter().min().expect("ten");
    assert!(
        last_first < first_end,
        "a worker got its first delta ({last_first:?}) only after another had finished ({first_end:?})"
    );
}

#[tokio::test]
async fn a_cancel_mid_stream_settles_the_partial_usage_releases_and_resumes_from_the_transcript() {
    let dir = tempfile::tempdir().expect("dir");
    let hang = Fake::new(true, |_| {
        let mut round = Round::answer("a long answer that never finishes");
        round.deltas = 8;
        round.hang_after = Some(2);
        round.usage = usage(1_234, 0);
        round
    });
    let env = Arc::new(env_with(hang, vec![], CallMode::Stream));
    let mut handle = spawn_worker(
        env.clone(),
        brief("w1", "answer", dir.path(), 5),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");

    // Read until the stream is visibly under way, then cancel inside it.
    let mut events = Vec::new();
    loop {
        let event = with_timeout(handle.next_event())
            .await
            .expect("in time")
            .expect("event");
        let streaming = matches!(event, WorkerEvent::TextDelta { .. });
        events.push(event);
        if streaming {
            break;
        }
    }
    let cancelled_at = Instant::now();
    handle.cancel();
    while let Some(event) = with_timeout(handle.next_event()).await.expect("in time") {
        events.push(event);
    }
    let result = handle.result().await.expect("result");
    let released = cancelled_at.elapsed();
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    assert!(
        released < Duration::from_secs(1),
        "released in {released:?}"
    );

    // The cancel landed in the model stream, and the partial usage followed it.
    let cancel_at = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::Cancelled { .. }))
        .expect("cancelled event");
    assert_eq!(
        events[cancel_at],
        WorkerEvent::Cancelled {
            phase: CancelPhase::ModelStream
        }
    );
    assert!(matches!(
        &events[cancel_at + 1],
        WorkerEvent::Usage { usage, .. } if usage.uncached_input == 1_234
    ));

    // The ledger row is settled cancelled with the partial usage; no hole.
    let rows = env.ledger.calls().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].settlement, Settlement::Cancelled);
    assert_eq!(rows[0].usage.uncached_input, 1_234);
    assert!(env.ledger.unreconciled().expect("query").is_empty());

    // Resume from the transcript with a provider that now answers.
    let answering = Fake::new(true, |_| Round::answer("done"));
    let env2 = Arc::new(env_with(answering.clone(), vec![], CallMode::Stream));
    let handle = spawn_worker(
        env2,
        brief("w1", "answer", dir.path(), 5),
        SpawnOptions {
            resume_from: Some(events),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert_eq!(
        resumed.steps, 2,
        "the cancelled round counts against the budget"
    );
    // The partial text of the cancelled round is not replayed as an answer.
    assert_eq!(answering.requests()[0].messages.len(), 1);
}

#[tokio::test]
async fn a_cancel_while_a_tool_runs_marks_the_call_interrupted_for_resume() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |_| {
        Round::tool(vec![(
            "slow",
            "probe",
            json!({"node": "n1", "ms": 60_000, "label": "slow", "ignore_cancel": true}),
        )])
    });
    let mut env = env_with(provider, vec![probe.clone()], CallMode::Complete);
    env.settings.tool_cancel_grace = Duration::from_millis(50);
    let env = Arc::new(env);
    let mut handle = spawn_worker(
        env,
        brief("w1", "probe", dir.path(), 5),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut events = Vec::new();
    loop {
        let event = with_timeout(handle.next_event())
            .await
            .expect("in time")
            .expect("event");
        let started = matches!(event, WorkerEvent::ToolCallStarted { .. });
        events.push(event);
        if started {
            break;
        }
    }
    // Give the tool time to begin.
    while probe.running.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    handle.cancel();
    while let Some(event) = with_timeout(handle.next_event()).await.expect("in time") {
        events.push(event);
    }
    assert_eq!(
        handle.result().await.expect("result").outcome,
        WorkerOutcome::Cancelled
    );
    assert!(events.contains(&WorkerEvent::Cancelled {
        phase: CancelPhase::ToolCall
    }));
    let mut state = kgraf_runtime::ResumeState::from_events(
        kgraf_runtime::opening_messages("probe", &[]),
        &events,
    )
    .expect("resumable");
    assert_eq!(state.interrupted, vec![("slow".into(), "probe".into())]);
    let last = state.messages().last().cloned().expect("messages");
    assert_eq!(
        last.content,
        vec![Content::ToolResult {
            tool_use_id: "slow".into(),
            content: INTERRUPTED_RESULT.into(),
            is_error: true,
        }]
    );
}

/// A cancel that arrives while a finished result waits for a bulk slot must not
/// lose the result: the call ran, and the persisted transcript has to say so.
#[tokio::test]
async fn a_cancel_while_a_result_waits_for_a_bulk_slot_still_persists_the_result() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |_| {
        Round::tool(vec![
            ("a", "probe", json!({"node": "n1", "label": "a"})),
            ("b", "probe", json!({"node": "n2", "label": "b"})),
        ])
    });
    let mut env = env_with(provider, vec![probe.clone()], CallMode::Complete);
    // One slot: the first finished result fills it while nobody reads, so the
    // second result waits for a slot.
    env.settings.bulk_capacity = 1;
    let mut handle = spawn_worker(
        Arc::new(env),
        brief("w1", "probe", dir.path(), 5),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut events = Vec::new();
    let mut started = 0;
    while started < 2 {
        let event = with_timeout(handle.next_event())
            .await
            .expect("in time")
            .expect("event");
        started += usize::from(matches!(event, WorkerEvent::ToolCallStarted { .. }));
        events.push(event);
    }
    // Both calls finish; one result is queued and the other waits for a slot.
    while probe.calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.cancel();
    while let Some(event) = with_timeout(handle.next_event()).await.expect("in time") {
        events.push(event);
    }
    assert_eq!(
        handle.result().await.expect("result").outcome,
        WorkerOutcome::Cancelled
    );
    for id in ["a", "b"] {
        assert!(
            events.iter().any(|e| matches!(
                e,
                WorkerEvent::ToolCallFinished { call_id, output, is_error: false }
                    if call_id == id && output.starts_with("out ")
            )),
            "the result of {id} is in the persisted transcript: {events:#?}"
        );
    }
}

#[tokio::test]
async fn tools_touching_the_same_node_serialize_and_independent_ones_overlap() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![
            (
                "a",
                "probe",
                json!({"node": "n1", "write": true, "ms": 80, "label": "a"}),
            ),
            (
                "b",
                "probe",
                json!({"node": "n1", "write": true, "ms": 80, "label": "b"}),
            ),
            (
                "c",
                "probe",
                json!({"node": "n2", "write": false, "ms": 80, "label": "c"}),
            ),
        ]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(provider, vec![probe.clone()], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "probe", dir.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    let spans = probe.spans.lock().expect("lock").clone();
    let span = |label: &str| {
        spans
            .iter()
            .find(|(l, _, _)| l == label)
            .map(|(_, s, e)| (*s, *e))
            .expect("span")
    };
    let (a_start, a_end) = span("a");
    let (b_start, _) = span("b");
    let (c_start, c_end) = span("c");
    assert!(b_start >= a_end, "two writers of n1 overlapped");
    assert!(
        c_start < a_end && a_start < c_end,
        "an independent call waited"
    );
    assert_eq!(probe.max_running.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn the_step_budget_warns_at_three_left_and_a_final_round_tool_call_yields_no_answer() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| {
        let n = round_of(request);
        Round::tool(vec![(
            &*format!("t{n}").leak(),
            "probe",
            json!({"node": format!("n{n}"), "label": n}),
        )])
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![probe.clone()],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "keep probing", dir.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");

    assert_eq!(result.outcome, WorkerOutcome::BudgetExhausted);
    assert_eq!(result.answer, "");
    assert_eq!(result.steps, 4);
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        3,
        "the final round's call did not run"
    );
    let notices: Vec<NoticeKind> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Notice { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(notices, vec![NoticeKind::StepsLeft, NoticeKind::FinalRound]);
    // The model was told: round 2 carries the warning, round 4 the final notice.
    let requests = provider.requests();
    let says = |r: &Request, needle: &str| {
        r.messages
            .last()
            .expect("last")
            .content
            .iter()
            .any(|c| c.as_text().is_some_and(|t| t.contains(needle)))
    };
    assert!(says(&requests[1], "3 rounds left"));
    assert!(says(&requests[3], "final round"));
    assert!(!says(&requests[2], "rounds left"));
}

#[tokio::test]
async fn a_repeated_identical_call_ends_the_run_as_a_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| {
        let id: &'static str = format!("t{}", round_of(request)).leak();
        Round::tool(vec![(id, "probe", json!({"node": "n1", "label": "same"}))])
    });
    let env = Arc::new(env_with(provider, vec![probe.clone()], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "loop", dir.path(), 20),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    match result.outcome {
        WorkerOutcome::Stalled {
            repeats,
            call_digest,
            ..
        } => {
            assert_eq!(repeats, 3);
            assert_eq!(
                call_digest,
                kgraf_runtime::call_digest("probe", &json!({"label": "same", "node": "n1"}))
            );
        }
        other => panic!("expected a stall, got {other:?}"),
    }
    assert_eq!(result.steps, 3);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
}

/// Usage grows by 10,000 input tokens per round against a 100,000-token window.
async fn rounds_until_handback(point: u8, source: PointSource) -> (u32, WorkerOutcome) {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |request| {
        let n = round_of(request) as u64 + 1;
        let id: &'static str = format!("t{n}").leak();
        let mut round = Round::tool(vec![(
            id,
            "write_file",
            json!({"path": format!("f{n}.txt"), "content": "x"}),
        )]);
        round.usage = usage(10_000 * n, 0);
        round
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ));
    let mut b = brief("w1", "grow", dir.path(), 50);
    b.context = ContextConfig::for_model(100_000)
        .and_then(|c| c.with_point(point, source))
        .expect("config");
    let handle = spawn_worker(env, b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    (result.steps, result.outcome)
}

#[tokio::test]
async fn the_worker_hands_back_at_the_configured_point_and_moving_the_point_moves_the_stop() {
    // Move the setting and assert the stop moves, not that a value saved.
    let (steps_80, outcome_80) = rounds_until_handback(80, PointSource::UnmeasuredDefault).await;
    let (steps_50, outcome_50) = rounds_until_handback(50, PointSource::User).await;
    assert_eq!(steps_80, 8, "stops once 80,000 tokens are reached");
    assert_eq!(steps_50, 5, "stops once 50,000 tokens are reached");
    for (outcome, point) in [(outcome_80, 80_000), (outcome_50, 50_000)] {
        let WorkerOutcome::HandedBack { handoff } = outcome else {
            panic!("expected a handback, got {outcome:?}");
        };
        handoff.validate().expect("a handoff leaves something");
        assert!(handoff.context_tokens >= point);
        assert!(handoff.context_tokens < point + 10_000);
        assert!(
            handoff.remaining[0].contains("grow"),
            "the instruction, verbatim"
        );
        assert!(!handoff.done.is_empty());
        assert!(!handoff.files_touched.is_empty());
    }
}

/// A thinking model on a route that reports no reasoning split: each round's
/// 9,000 output tokens are almost all reasoning (36,000 bytes of it), and the
/// prompt stays at 6,000 tokens. The stop point is 12,000 tokens.
async fn thinking_rounds(provider: Arc<Fake>) -> WorkerResult {
    let dir = tempfile::tempdir().expect("dir");
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ));
    let mut b = brief("w1", "think", dir.path(), 50);
    b.context = ContextConfig::for_model(100_000)
        .and_then(|c| c.with_point(12, PointSource::User))
        .expect("config");
    let handle = spawn_worker(env, b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    result
}

fn thinking_script(request: &Request) -> Round {
    let n = round_of(request) as u64 + 1;
    if n > 4 {
        return Round::answer("done");
    }
    let id: &'static str = format!("t{n}").leak();
    let mut round = Round::tool(vec![(
        id,
        "write_file",
        json!({"path": format!("f{n}.txt"), "content": "x"}),
    )]);
    round.reasoning = "a".repeat(36_000);
    round.usage = usage(6_000, 9_000);
    round
}

#[tokio::test]
async fn reasoning_a_route_does_not_resend_does_not_move_the_stop_point() {
    // 6,000 prompt + 9,000 output is 15,000 against a 12,000 stop, but 7,200 or
    // more of that output is reasoning no later request carries.
    let result = thinking_rounds(Fake::new(false, thinking_script)).await;
    assert!(
        matches!(result.outcome, WorkerOutcome::Completed),
        "a worker below the stop point finishes: {:?}",
        result.outcome
    );
    assert_eq!(result.steps, 5);
    // The trace's peak is the figure the stop check read: above the prompt the
    // provider billed, below the point, and below what counting the reasoning
    // would have read.
    assert!(result.peak_context_tokens >= 7_800, "{result:?}");
    assert!(result.peak_context_tokens < 12_000, "{result:?}");
}

#[tokio::test]
async fn reasoning_a_route_resends_still_counts_toward_the_stop_point() {
    let result = thinking_rounds(Fake::resending(thinking_script)).await;
    let WorkerOutcome::HandedBack { handoff } = result.outcome.clone() else {
        panic!("expected a handback, got {:?}", result.outcome);
    };
    assert!(handoff.context_tokens >= 15_000, "{handoff:?}");
    assert_eq!(result.steps, 1);
    // The peak is the measured value at the check that stopped the worker.
    assert_eq!(result.peak_context_tokens, handoff.context_tokens);
}

#[tokio::test]
async fn old_tool_outputs_are_masked_in_requests_and_kept_whole_in_the_transcript() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| {
        let n = round_of(request);
        if n == 8 {
            return Round::answer("done");
        }
        let id: &'static str = format!("t{n}").leak();
        Round::tool(vec![(
            id,
            "probe",
            json!({"node": format!("n{n}"), "label": n, "lines": 20}),
        )])
    });
    // The environment's default masker: kgraf-session's reduction library.
    let env = env_with(provider.clone(), vec![probe], CallMode::Complete);
    let placeholder = SessionMasker::default().placeholder().to_owned();
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "probe a lot", dir.path(), 12),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    let last = provider.requests().last().cloned().expect("requests");
    let bodies: Vec<String> = last
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(bodies.len(), 8);
    assert!(bodies[..3].iter().all(|b| *b == placeholder), "{bodies:?}");
    assert!(bodies[3..].iter().all(|b| b.starts_with("out ")));
    // The instruction is pinned, verbatim, first.
    assert_eq!(instruction_of(&last), "probe a lot");
    // The transcript keeps every original output.
    let kept = events
        .iter()
        .filter(|e| matches!(e, WorkerEvent::ToolCallFinished { output, .. } if output.starts_with("out ")))
        .count();
    assert_eq!(kept, 8);
}

/// Decision 0071: a registered exact secret a tool
/// prints must be absent from the next request the model sees (model-facing
/// scrub, registry only) *and* from every persisted `WorkerEvent` (persist
/// scrub) -- `events` here is exactly what a real run's `JsonlTranscript`
/// would serialize, since both draw from the same `WorkerEvent` stream this
/// crate's own scrub call site (`Worker::emit`) produces; kgraf-cli's e2e
/// suite (`stored_records` in `worker_e2e.rs`) covers the on-disk file
/// itself.
#[tokio::test]
async fn a_registered_secret_a_tool_prints_is_absent_from_the_next_request_and_every_event() {
    let dir = tempfile::tempdir().expect("dir");
    let secret = "kgraf-e2e-registered-Wn4Jc7Ab2Xe9Gu5Ht";
    kgraf_secrets::register_exact_secret("test-registered-secret", secret);
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, move |request| {
        if round_of(request) == 0 {
            Round::tool(vec![(
                "t0",
                "probe",
                json!({"node": "n0", "label": secret, "lines": 1}),
            )])
        } else {
            Round::answer("done")
        }
    });
    let env = env_with(provider.clone(), vec![probe], CallMode::Complete);
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "print the secret", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    // Never reaches the model: the tool result that carried it is
    // registry-scrubbed before `TranscriptBuilder::apply` ever sees it.
    let last = provider.requests().last().cloned().expect("a request");
    let request_text = format!("{last:?}");
    assert!(
        !request_text.contains(secret),
        "the secret must not reach the model's next request: {request_text}"
    );
    assert!(request_text.contains("[redacted:test-registered-secret;"));

    // Never persisted either: every `WorkerEvent` (what a real run's
    // `JsonlTranscript` would serialize) is registry-scrubbed the same way.
    let found_marker = events.iter().any(|e| {
        matches!(e, WorkerEvent::ToolCallFinished { output, .. }
            if output.contains("[redacted:test-registered-secret;") && !output.contains(secret))
    });
    assert!(found_marker, "{events:#?}");
    for event in &events {
        assert!(
            !format!("{event:?}").contains(secret),
            "the secret must not survive into any persisted event: {event:?}"
        );
    }
}

/// Decision 0071: a worker that reads a secrets file deeper than the
/// checkout root (`config/.env.local`) with the real `read_file` tool still
/// sees its value (the model-facing path is unchanged), and no persisted
/// event keeps it: the read added the file's values to the record scrubber.
#[tokio::test]
async fn a_secrets_file_a_worker_reads_is_scrubbed_from_its_transcript_only() {
    let dir = tempfile::tempdir().expect("dir");
    let marker = "kGrafMarkerRead8Tq3Wv6Zn1Pc";
    std::fs::create_dir_all(dir.path().join("config")).expect("dir");
    std::fs::write(
        dir.path().join("config/.env.local"),
        format!("DB_PASSWORD={marker}\nLOG_LEVEL=info\n"),
    )
    .expect("env");
    let provider = Fake::new(false, move |request| {
        if round_of(request) == 0 {
            Round::tool(vec![(
                "t0",
                "read_file",
                json!({"path": "config/.env.local"}),
            )])
        } else {
            Round::answer(&format!("the password is {marker}"))
        }
    });
    let read: Arc<dyn Tool> = Arc::new(kgraf_tools::ReadFileTool::default());
    let env = env_with(provider.clone(), vec![read], CallMode::Complete);
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "read the env file", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    let last = provider.requests().last().cloned().expect("a request");
    assert!(
        format!("{last:?}").contains(marker),
        "the worker reads the file it asked for"
    );
    for event in &events {
        assert!(
            !format!("{event:?}").contains(marker),
            "the value must not survive into any persisted event: {event:?}"
        );
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, WorkerEvent::ToolCallFinished { output, .. }
            if output.contains("LOG_LEVEL=info"))),
        "ordinary settings stay: {events:#?}"
    );
}

/// A *pattern*-shaped secret (not registered) is
/// redacted on the persist path but, by design (decision 0071), is
/// left alone on the model-facing path -- the model still sees whatever a
/// tool's real output holds, since a pattern rule never runs on that path.
#[tokio::test]
async fn a_pattern_secret_a_tool_prints_is_redacted_only_on_the_persist_path() {
    let dir = tempfile::tempdir().expect("dir");
    let secret = format!("ghp_{}", "a".repeat(36));
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, {
        let secret = secret.clone();
        move |request| {
            if round_of(request) == 0 {
                Round::tool(vec![(
                    "t0",
                    "probe",
                    json!({"node": "n0", "label": secret.clone(), "lines": 1}),
                )])
            } else {
                Round::answer("done")
            }
        }
    });
    let env = env_with(provider.clone(), vec![probe], CallMode::Complete);
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "print a github token", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    // The model still sees it: inherent to giving a worker tool output at
    // all, and the whole reason pattern rules never run on this path (a
    // false positive there would corrupt what the worker reasons over).
    let last = provider.requests().last().cloned().expect("a request");
    assert!(format!("{last:?}").contains(&secret));

    // The persisted transcript redacts it.
    let persisted_redacted = events.iter().any(|e| {
        matches!(e, WorkerEvent::ToolCallFinished { output, .. }
            if output.contains("[redacted:github-pat;") && !output.contains(&secret))
    });
    assert!(persisted_redacted, "{events:#?}");
}

/// A key split across two
/// deltas is absent from the persisted transcript's authoritative record and
/// from the rebuilt request on resume, even though a naive per-chunk scrub of
/// either individual delta cannot see it (each half alone matches no pattern
/// rule).
///
/// Fixed by `WorkerEvent::AssistantMessage`, emitted once per round from the
/// assembled `Response` right before `Stop` (not by holding deltas back,
/// which was tried and reverted -- see `Worker::known_limits`): the settled
/// message is scrubbed as one whole string, and both the persisted transcript
/// and a resumed rebuild use it in place of the individually-scrubbed deltas.
#[tokio::test]
async fn a_secret_split_across_two_deltas_is_absent_from_the_persisted_transcript_and_the_resumed_rebuild()
 {
    let dir = tempfile::tempdir().expect("dir");
    let secret = format!("ghp_{}", "a".repeat(36)); // 40 bytes, split into two 20-byte deltas
    let provider = Fake::new(true, {
        let secret = secret.clone();
        move |_request| Round {
            text: secret.clone(),
            deltas: 2,
            stop: Some(StopReason::EndTurn),
            usage: usage(100, 10),
            ..Round::default()
        }
    });
    let env = env_with(provider.clone(), vec![], CallMode::Stream);
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "say the secret", dir.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    // Confirms the split actually happened and actually escapes a per-chunk
    // scrub: each individual persisted `TextDelta` is untouched.
    let delta_texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(delta_texts.len(), 2, "{delta_texts:?}");
    for delta in &delta_texts {
        assert!(!delta.contains("[redacted:"), "{delta_texts:?}");
    }

    // The settled record is redacted: absent from the persisted transcript's
    // authoritative account of what the round said.
    let settled = events
        .iter()
        .find_map(|e| match e {
            WorkerEvent::AssistantMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("a settled assistant message");
    assert!(!settled.contains(&secret), "{settled:?}");
    assert!(settled.contains("[redacted:github-pat;"), "{settled:?}");

    // Resume: rebuilding the model's messages from these same persisted
    // events (what a real `JsonlTranscript::load` would hand back, replayed
    // through the same `TranscriptBuilder::apply` a resume uses --
    // `ResumeState::from_events`, not used directly here, additionally
    // refuses a transcript ending in a completed round as nothing left to
    // resume, which is orthogonal to what this test proves) must not carry
    // the secret into the next request either.
    let mut builder = TranscriptBuilder::new(vec![Message::user("say the secret")]);
    for event in &events {
        builder.apply(event);
    }
    let rebuilt = format!("{:?}", builder.messages());
    assert!(!rebuilt.contains(&secret), "{rebuilt}");
    assert!(rebuilt.contains("[redacted:github-pat;"), "{rebuilt}");
}

struct DenyWrites;

#[async_trait::async_trait]
impl ApprovalHook for DenyWrites {
    fn name(&self) -> &str {
        "deny-writes"
    }
    async fn review(&self, call: &PendingCall) -> HookDecision {
        if call.tool == "write_file" {
            HookDecision::Deny {
                reason: "read-only run".into(),
            }
        } else {
            HookDecision::Allow
        }
    }
}

#[tokio::test]
async fn a_denied_call_is_not_run_and_the_model_is_told_why() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "write_file",
            json!({"path": "x.txt", "content": "no"}),
        )]),
        _ => Round::answer("could not write"),
    });
    let mut env = env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    );
    env.approval = ApprovalChain::new(vec![Arc::new(DenyWrites)]);
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "write", dir.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert!(!dir.path().join("x.txt").exists());
    assert!(result.changed_paths.is_empty());
    // Decision 0093: the denial says how to report a shortfall, ahead of the
    // policy's own text, and the result lists it.
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::ToolCallFinished { output, is_error: true, .. }
            if output.starts_with("ERROR: denied by deny-writes: If your instruction")
                && output.contains("do not work around it")
                && output.ends_with("The refusal: read-only run.")
    )));
    assert_eq!(result.refused.len(), 1, "{:?}", result.refused);
    assert_eq!(result.refused[0].capability, "write_file");
}

/// A hook whose denial quotes the call's argument back, secret and all.
struct DenyQuoting;

#[async_trait::async_trait]
impl ApprovalHook for DenyQuoting {
    fn name(&self) -> &str {
        "deny-quoting"
    }
    async fn review(&self, call: &PendingCall) -> HookDecision {
        HookDecision::Deny {
            reason: format!("refused content {}", call.input["content"]),
        }
    }
}

/// Decision 0071: an approval record is scrubbed
/// before it reaches the transcript and the shell (it never passes the
/// model-facing path, so this is its only scrub), and the redaction is
/// recorded as kinds and counts right after it.
#[tokio::test]
async fn an_approval_record_is_scrubbed_and_its_redaction_recorded() {
    let dir = tempfile::tempdir().expect("dir");
    let secret = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
    let content = secret.clone();
    let provider = Fake::new(false, move |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "write_file",
            json!({"path": "x.txt", "content": content}),
        )]),
        _ => Round::answer("could not write"),
    });
    let mut env = env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    );
    env.approval = ApprovalChain::new(vec![Arc::new(DenyQuoting)]);
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "write", dir.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    let at = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::Approval { .. }))
        .expect("an approval record");
    let WorkerEvent::Approval { record } = &events[at] else {
        unreachable!()
    };
    let decision = serde_json::to_string(&record.decision).expect("json");
    assert!(!decision.contains(&secret), "{decision}");
    assert!(decision.contains("[redacted:github-pat;"), "{decision}");
    assert!(
        matches!(&events[at + 1], WorkerEvent::Redacted { counts }
            if counts.get("github-pat") == Some(&1)),
        "{:?}",
        events.get(at + 1)
    );
    let all = format!("{events:?}");
    assert!(!all.contains(&secret), "no event carries the secret");
}

/// The non-delta path of `Worker::scrub_both`. A tool call whose
/// input carries a token is followed at once by a `Redacted` record of it;
/// the model's copy of the call is not redacted by a pattern rule.
#[tokio::test]
async fn a_redacted_tool_call_is_followed_by_its_redaction_record() {
    let dir = tempfile::tempdir().expect("dir");
    let secret = format!("ghp_{}", "Nd1Lt2Ap3Th4Ab5Cd6Ef7Gh8Ij9Kl0Mn1Op2");
    let content = secret.clone();
    let provider = Fake::new(false, move |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "write_file",
            json!({"path": "x.txt", "content": content}),
        )]),
        _ => Round::answer("wrote it"),
    });
    let env = env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    );
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "write", dir.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    let at = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::ToolCallStarted { .. }))
        .expect("a tool call");
    assert!(
        format!("{:?}", events[at]).contains("[redacted:github-pat;"),
        "{:?}",
        events[at]
    );
    assert!(
        matches!(&events[at + 1], WorkerEvent::Redacted { counts }
            if counts.get("github-pat") == Some(&1) && counts.len() == 1),
        "{:?}",
        events.get(at + 1)
    );
    // The file the model asked for holds the real text: the model's copy of
    // the call went through the registry only.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("x.txt")).expect("written"),
        secret
    );
}

#[tokio::test]
async fn silent_degradations_are_refused() {
    let dir = tempfile::tempdir().expect("dir");
    // Streaming requested of a provider without it.
    let env = Arc::new(env_with(
        Fake::new(false, |_| Round::answer("x")),
        vec![],
        CallMode::Stream,
    ));
    assert!(matches!(
        spawn_worker(
            env,
            brief("w1", "x", dir.path(), 3),
            SpawnOptions::default()
        )
        .await,
        Err(WorkerError::StreamingUnavailable { .. })
    ));

    // A brief cannot activate skills (a worker uses the `skill` tool): refused, not ignored.
    let env = Arc::new(env_with(
        Fake::new(true, |_| Round::answer("x")),
        vec![],
        CallMode::Stream,
    ));
    let mut b = brief("w1", "x", dir.path(), 3);
    b.skills = vec!["review".into()];
    assert!(matches!(
        spawn_worker(env.clone(), b, SpawnOptions::default()).await,
        Err(WorkerError::SkillsUnsupported { .. })
    ));

    // A slice named with no graph configured is left out by name, loudly;
    // the spawn does not fail (see tests/slice_without_graph.rs).
    let mut b = brief("w1", "x", dir.path(), 3);
    b.slices = vec![NodeId::new("n1")];
    let mut handle = spawn_worker(env, b, SpawnOptions::default())
        .await
        .expect("a slice that cannot be rendered does not fail the spawn");
    while handle.next_event().await.is_some() {}
    let left_out = handle.result().await.expect("result").slices_left_out;
    assert_eq!(left_out.len(), 1);
    assert_eq!(left_out[0].node, NodeId::new("n1"));

    // A tool-use stop whose response carries no tool call (the shape of a stream
    // parser that drops tool blocks) fails the run instead of ending it empty.
    let env = Arc::new(env_with(
        Fake::new(true, |_| Round {
            stop: Some(StopReason::ToolUse),
            ..Round::answer("calling a tool")
        }),
        vec![],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "x", dir.path(), 3),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(matches!(
        result.outcome,
        WorkerOutcome::Failed { ref error } if error.contains("carries none")
    ));
}

#[tokio::test]
async fn a_cancelled_worker_reports_cancelled_even_when_its_events_are_not_read() {
    // `WorkerHandle::result` drops the event receiver. A cancel must still be
    // reported as a cancel, not as a failure to emit.
    let dir = tempfile::tempdir().expect("dir");
    let hang = Fake::new(true, |_| Round {
        hang_after: Some(0),
        ..Round::answer("never")
    });
    let env = Arc::new(env_with(hang, vec![], CallMode::Stream));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "x", dir.path(), 3),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    handle.cancel();
    let result = with_timeout(handle.result())
        .await
        .expect("in time")
        .expect("result");
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    assert!(env.ledger.unreconciled().expect("query").is_empty());
}

/// Writes its file at once, then keeps working for `ms`, ignoring the cancel: the
/// write is done whatever happens next.
struct SlowWriter;

#[async_trait::async_trait]
impl Tool for SlowWriter {
    fn name(&self) -> &str {
        "slow_write"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "slow_write".into(),
            description: "Write a file, then keep working".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        Access::none()
            .write_path(input["path"].as_str().unwrap_or_default())
            .map_err(|source| ToolError::Access {
                tool: "slow_write".into(),
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
        std::fs::write(&path, "done").map_err(|e| ToolError::Internal {
            tool: "slow_write".into(),
            reason: e.to_string(),
        })?;
        tokio::time::sleep(Duration::from_millis(input["ms"].as_u64().unwrap_or(0))).await;
        Ok(ToolOutput::text("written"))
    }
}

#[tokio::test]
async fn a_write_that_completes_during_the_cancel_grace_is_recorded_not_interrupted() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| {
        Round::tool(vec![(
            "w",
            "slow_write",
            json!({"path": "out.txt", "ms": 150}),
        )])
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(SlowWriter)],
        CallMode::Complete,
    ));
    let mut handle = spawn_worker(
        env,
        brief("w1", "write", dir.path(), 5),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut events = Vec::new();
    loop {
        let event = with_timeout(handle.next_event())
            .await
            .expect("in time")
            .expect("event");
        let started = matches!(event, WorkerEvent::ToolCallStarted { .. });
        events.push(event);
        if started {
            break;
        }
    }
    while !dir.path().join("out.txt").exists() {
        tokio::task::yield_now().await;
    }
    handle.cancel();
    while let Some(event) = with_timeout(handle.next_event()).await.expect("in time") {
        events.push(event);
    }
    let result = handle.result().await.expect("result");
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    assert_eq!(
        result.changed_paths,
        vec![std::path::PathBuf::from("out.txt")]
    );
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::ToolCallFinished { call_id, output, is_error: false }
            if call_id == "w" && output == "written"
    )));
    let state = kgraf_runtime::ResumeState::from_events(
        kgraf_runtime::opening_messages("write", &[]),
        &events,
    )
    .expect("resumable");
    assert!(state.interrupted.is_empty(), "{:?}", state.interrupted);
}

#[tokio::test]
async fn a_cancel_before_any_usage_leaves_the_call_unreconciled_in_both_modes() {
    for (mode, streams) in [(CallMode::Stream, true), (CallMode::Complete, false)] {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Fake::new(streams, |_| Round {
            hang_before_usage: true,
            ..Round::answer("never")
        });
        let env = Arc::new(env_with(provider.clone(), vec![], mode));
        let mut handle = spawn_worker(
            env.clone(),
            brief("w1", "x", dir.path(), 3),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        // Read while waiting: a request leaves only once the reader has
        // asked past every event before it (`WorkerEndpoint::stored`).
        with_timeout(async {
            while provider.requests().is_empty() {
                tokio::select! {
                    _ = handle.next_event() => {}
                    () = tokio::task::yield_now() => {}
                }
            }
        })
        .await
        .expect("the request left");
        handle.cancel();
        let result = with_timeout(handle.result())
            .await
            .expect("in time")
            .expect("result");
        assert_eq!(result.outcome, WorkerOutcome::Cancelled);
        let rows = env.ledger.calls().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].settlement,
            Settlement::Unreconciled,
            "{mode:?}: an unknown spend is not a zero spend"
        );
        assert_eq!(env.ledger.unreconciled().expect("query").len(), 1);
        if mode == CallMode::Stream {
            assert_eq!(rows[0].request_id.as_deref(), Some("req-fake-1"));
        }
    }
}

#[tokio::test]
async fn resume_restores_calls_and_writes_counts_a_failed_round_and_refuses_changed_context() {
    let dir = tempfile::tempdir().expect("dir");
    // Round 1 writes a file; round 2 fails at the provider without usage.
    let failing = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "write_file",
            json!({"path": "a.txt", "content": "a"}),
        )]),
        _ => Round {
            fail: true,
            ..Round::answer("x")
        },
    });
    let env = Arc::new(env_with(
        failing,
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "write a", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first) = with_timeout(drain(handle)).await.expect("in time");
    assert!(matches!(first.outcome, WorkerOutcome::Failed { .. }));
    assert_eq!(first.steps, 2);
    assert_eq!(first.calls.len(), 2);

    let answering = Fake::new(false, |_| Round::answer("done"));
    let mut env2 = env_with(answering, vec![Arc::new(WriteFile)], CallMode::Complete);
    env2.ledger = env.ledger.clone();
    let env2 = Arc::new(env2);
    let handle = spawn_worker(
        env2.clone(),
        brief("w1", "write a", dir.path(), 6),
        SpawnOptions {
            resume_from: Some(events.clone()),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert_eq!(
        resumed.steps, 3,
        "the failed round counted against the budget"
    );
    assert_eq!(resumed.calls.len(), 3);
    assert_eq!(&resumed.calls[..2], first.calls.as_slice());
    assert_eq!(
        resumed.changed_paths,
        vec![std::path::PathBuf::from("a.txt")]
    );

    // A different prefix or different slices: refused, not resumed.
    let other = Fake::new(false, |_| Round::answer("done"));
    let env3 = Arc::new(env_with(other.clone(), vec![], CallMode::Complete));
    let refused = spawn_worker(
        env3,
        brief("w1", "write a", dir.path(), 6),
        SpawnOptions {
            resume_from: Some(events.clone()),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect_err("different prefix");
    assert!(matches!(
        refused,
        WorkerError::ResumeMismatch {
            what: "prefix digest",
            ..
        }
    ));
    // A transcript recorded before the rename (its prefix named
    // Haustoria) is refused with a reason naming the rename.
    let former = env2
        .prefix
        .former_digest()
        .expect("the prompt names the product")
        .as_str()
        .to_owned();
    let mut before_rename = events.clone();
    for event in &mut before_rename {
        if let WorkerEvent::Context { prefix_digest, .. } = event {
            *prefix_digest = former.clone();
        }
    }
    let refused = spawn_worker(
        env2.clone(),
        brief("w1", "write a", dir.path(), 6),
        SpawnOptions {
            resume_from: Some(before_rename),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect_err("recorded before the rename");
    assert!(
        matches!(refused, WorkerError::ResumeFromBeforeRename { .. }),
        "{refused}"
    );
    assert!(refused.to_string().contains("decision 0062"), "{refused}");
    let mut with_slices = events;
    for event in &mut with_slices {
        if let WorkerEvent::Context { slices, .. } = event {
            slices.push(NodeId::new("n-gone"));
        }
    }
    let refused = spawn_worker(
        env2,
        brief("w1", "write a", dir.path(), 6),
        SpawnOptions {
            resume_from: Some(with_slices),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect_err("different slices");
    assert!(matches!(
        refused,
        WorkerError::ResumeMismatch { what: "slices", .. }
    ));
    assert!(other.requests().is_empty());
}

#[tokio::test]
async fn repeated_denied_calls_are_counted_apart_and_do_not_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |request| match round_of(request) {
        n @ 0..=3 => {
            let id: &'static str = format!("t{n}").leak();
            Round::tool(vec![(
                id,
                "write_file",
                json!({"path": "x.txt", "content": "same"}),
            )])
        }
        _ => Round::answer("gave up writing"),
    });
    let mut env = env_with(provider, vec![Arc::new(WriteFile)], CallMode::Complete);
    env.approval = ApprovalChain::new(vec![Arc::new(DenyWrites)]);
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "write", dir.path(), 10),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.denied, 4);
}

/// Edit, test, edit, test, edit, test is progress: each edit changes what the
/// test sees, so the identical test call does not count toward a stall.
#[tokio::test]
async fn a_test_run_after_each_edit_is_not_a_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |request| {
        let n = round_of(request);
        let id: &'static str = format!("t{n}").leak();
        match n {
            0 | 2 | 4 => Round::tool(vec![(
                id,
                "write_file",
                json!({"path": "a.txt", "content": format!("v{n}")}),
            )]),
            1 | 3 | 5 => Round::tool(vec![(id, "build", json!({}))]),
            _ => Round::answer("done"),
        }
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(WriteFile), Arc::new(SandboxedBuild)],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "edit and test", dir.path(), 12),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
}

/// A passing build or test run since the last write is not progress for the
/// stall check: it changes nothing an identical read would see. A worker that
/// delivered, then re-reads the same file between passing builds and edits
/// that never land, is looping (read, test, build, read, two failed patches,
/// read) and is still ended. That attempt's tree is judged by the
/// orchestrator's verifier instead (`verifies_unfinished`).
#[tokio::test]
async fn passing_builds_between_identical_reads_do_not_reset_the_stall_count() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| {
        let n = round_of(request);
        let id: &'static str = format!("t{n}").leak();
        match n {
            0 => Round::tool(vec![(
                id,
                "write_file",
                json!({"path": "a.txt", "content": "delivered"}),
            )]),
            2 | 4 => Round::tool(vec![(id, "build", json!({}))]),
            _ => Round::tool(vec![(id, "probe", json!({"node": "a.txt"}))]),
        }
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(WriteFile), Arc::new(SandboxedBuild), probe.clone()],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "deliver, then loop", dir.path(), 20),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(
        matches!(result.outcome, WorkerOutcome::Stalled { repeats: 3, .. }),
        "{:?}",
        result.outcome
    );
    // write, read, build, read, build, read: the third read is the stall.
    assert_eq!(result.steps, 6);
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        2,
        "the third read never ran"
    );
    assert_eq!(
        result.changed_paths,
        vec![std::path::PathBuf::from("a.txt")]
    );
}

/// With no write between them, the same two reads alternating are still a loop.
#[tokio::test]
async fn alternating_identical_calls_with_no_write_between_still_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| {
        let n = round_of(request);
        let id: &'static str = format!("t{n}").leak();
        let node = if n.is_multiple_of(2) { "n1" } else { "n2" };
        Round::tool(vec![(id, "probe", json!({"node": node}))])
    });
    let env = Arc::new(env_with(provider, vec![probe], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "loop", dir.path(), 20),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(
        matches!(result.outcome, WorkerOutcome::Stalled { repeats: 3, .. }),
        "{:?}",
        result.outcome
    );
    assert_eq!(result.steps, 5);
}

/// A resume replays only the calls that ran and succeeded into the stall
/// detector. Three denied copies of a call in the earlier segment did nothing,
/// so the same call issued once after the resume is not a repeat.
#[tokio::test]
async fn a_resume_does_not_count_denied_calls_toward_a_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let call = json!({"path": "x.txt", "content": "same"});
    let first_call = call.clone();
    let denying = Fake::new(false, move |request| match round_of(request) {
        n @ 0..=2 => {
            let id: &'static str = format!("t{n}").leak();
            Round::tool(vec![(id, "write_file", first_call.clone())])
        }
        _ => Round {
            fail: true,
            ..Round::answer("x")
        },
    });
    let mut env = env_with(denying, vec![Arc::new(WriteFile)], CallMode::Complete);
    env.approval = ApprovalChain::new(vec![Arc::new(DenyWrites)]);
    let env = Arc::new(env);
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "write", dir.path(), 12),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first) = with_timeout(drain(handle)).await.expect("in time");
    assert!(matches!(first.outcome, WorkerOutcome::Failed { .. }));
    assert_eq!(first.denied, 3);

    let second_call = call.clone();
    let allowing = Fake::new(false, move |request| match round_of(request) {
        3 => Round::tool(vec![("t3", "write_file", second_call.clone())]),
        _ => Round::answer("done"),
    });
    let mut env2 = env_with(allowing, vec![Arc::new(WriteFile)], CallMode::Complete);
    env2.ledger = env.ledger.clone();
    let handle = spawn_worker(
        Arc::new(env2),
        brief("w1", "write", dir.path(), 12),
        SpawnOptions {
            resume_from: Some(events),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert!(dir.path().join("x.txt").exists());
}

#[tokio::test]
async fn a_prefix_naming_the_worker_is_refused_and_environments_share_the_global_registry() {
    let dir = tempfile::tempdir().expect("dir");
    let registry = ToolRegistry::new();
    let personal = snapshot(&registry, "You are worker-alpha.");
    let provider = Fake::new(false, |_| Round::answer("x"));
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let env = WorkerEnv::new(
        ledger.clone(),
        provider.clone(),
        Arc::new(registry),
        personal,
    );
    assert!(Arc::ptr_eq(&env.prefixes, &PrefixRegistry::global()));
    let mut env = env;
    env.settings.call_mode = CallMode::Complete;
    let refused = spawn_worker(
        Arc::new(env),
        brief("worker-alpha", "x", dir.path(), 3),
        SpawnOptions::default(),
    )
    .await
    .expect_err("worker id in the prefix");
    assert!(matches!(
        refused,
        WorkerError::Prefix(PrefixCheckError::WorkerSpecific { kind: "worker", .. })
    ));
    assert!(provider.requests().is_empty());

    // Two environments built separately for one vendor and model meet in the
    // global registry: the second, different prefix is refused.
    let group = |system: &str| {
        let registry = ToolRegistry::new();
        let mut prefix = snapshot(&registry, system).prefix().clone();
        prefix.vendor = "global-registry-test-vendor".into();
        let snap = PrefixSnapshot::new(prefix, &NeutralPrefixEncoder).expect("snap");
        let mut env = WorkerEnv::new(ledger.clone(), provider.clone(), Arc::new(registry), snap);
        env.settings.call_mode = CallMode::Complete;
        Arc::new(env)
    };
    let first = spawn_worker(
        group("Prefix one."),
        brief("w1", "x", dir.path(), 3),
        SpawnOptions::default(),
    )
    .await
    .expect("first");
    let _ = with_timeout(drain(first)).await.expect("in time");
    let second = spawn_worker(
        group("Prefix two."),
        brief("w2", "x", dir.path(), 3),
        SpawnOptions::default(),
    )
    .await
    .expect_err("a second prefix in the global registry");
    assert!(matches!(
        second,
        WorkerError::Prefix(PrefixCheckError::SecondPrefix { .. })
    ));
}

// ---------------------------------------------------------------- spend and scope

#[tokio::test]
async fn writes_outside_the_declared_scope_or_in_the_repository_metadata_are_refused() {
    let worktree = tempfile::tempdir().expect("worktree");
    let meta = format!("{}/config", [".", "git"].concat());
    let meta_for_script = meta.clone();
    let provider = Fake::new(true, move |request| match round_of(request) {
        0 => Round::tool(vec![
            (
                "t1",
                "write_file",
                json!({"path": "src/allowed.rs", "content": "ok"}),
            ),
            (
                "t2",
                "write_file",
                json!({"path": "src/other.rs", "content": "no"}),
            ),
            (
                "t3",
                "write_file",
                json!({"path": meta_for_script, "content": "no"}),
            ),
        ]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let mut b = brief("w1", "write src/allowed.rs", worktree.path(), 6);
    // Even a scope that lists the metadata path does not open it.
    b.write_scope = Some(vec![
        std::path::PathBuf::from("src/allowed.rs"),
        std::path::PathBuf::from(&meta),
    ]);
    let handle = spawn_worker(env, b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.denied, 2);
    assert!(worktree.path().join("src/allowed.rs").exists());
    assert!(!worktree.path().join("src/other.rs").exists());
    assert!(!worktree.path().join(&meta).exists());
    let refusals: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::ToolCallFinished {
                output, is_error, ..
            } if *is_error => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        refusals
            .iter()
            .any(|o| o.contains("outside the declared write scope"))
    );
    assert!(
        refusals
            .iter()
            .any(|o| o.contains("not writable by a worker") && o.contains("steers git"))
    );
}

/// A plan declares the repository's metadata in another
/// case, so the scope admits it; on a case-insensitive volume the write would
/// rewrite the worktree's gitlink. Every spelling is refused before it runs.
#[tokio::test]
async fn metadata_spelled_in_another_case_or_with_ignorable_characters_is_refused() {
    let worktree = tempfile::tempdir().expect("worktree");
    let dot = |s: &str| format!(".{s}");
    let spellings = vec![
        format!("{}/config", dot("GIT")),
        format!("{}/HEAD", dot("Git")),
        format!("{}\u{200C}/config", dot("git")),
        format!("{}./config", dot("git")),
        format!("{}/x", dot("KGRAF")),
        dot("GITATTRIBUTES"),
    ];
    let calls: Vec<(String, String, Value)> = spellings
        .iter()
        .enumerate()
        .map(|(i, p)| {
            (
                format!("t{i}"),
                "write_file".to_owned(),
                json!({"path": p, "content": "gitdir: /tmp/evil"}),
            )
        })
        .collect();
    let script_calls = calls.clone();
    let provider = Fake::new(true, move |request| match round_of(request) {
        0 => Round {
            calls: script_calls.clone(),
            ..Round::tool(vec![])
        },
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let mut b = brief("w1", "edit the metadata", worktree.path(), 6);
    // The scope lists every spelling: the refusal must not depend on the scope.
    b.write_scope = Some(spellings.iter().map(std::path::PathBuf::from).collect());
    let handle = spawn_worker(env, b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.denied as usize, spellings.len());
    for p in &spellings {
        assert!(!worktree.path().join(p).exists(), "{p:?} was written");
    }
}

/// Writes anywhere and declares nothing, as a shell or MCP tool does.
struct Anywhere;

#[async_trait::async_trait]
impl Tool for Anywhere {
    fn name(&self) -> &str {
        "anywhere"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "anywhere".into(),
            description: "Run something".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::exclusive())
    }
    async fn call(
        &self,
        _input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        std::fs::write(ctx.workspace_root.join("outside.rs"), "x").map_err(|e| {
            ToolError::Internal {
                tool: "anywhere".into(),
                reason: e.to_string(),
            }
        })?;
        Ok(ToolOutput::text("ran"))
    }
}

#[tokio::test]
async fn a_tool_that_may_write_anywhere_is_refused_under_a_declared_scope() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "anywhere", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Anywhere)],
        CallMode::Stream,
    ));
    let mut b = brief("w1", "run it", worktree.path(), 6);
    b.write_scope = Some(vec![std::path::PathBuf::from("src/a.rs")]);
    let handle = spawn_worker(env.clone(), b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.denied, 1);
    assert!(!worktree.path().join("outside.rs").exists());
}

/// A contract that declares no writes is a check-only contract of its plan
/// (plan intake accepts one next to contracts that write). A write it attempts
/// is refused, and the refusal says why in terms of the plan: the contract
/// declares no writes, and a file the work needs belongs to the contract that
/// lists it. A shell call that may write anywhere says so too.
#[tokio::test]
async fn a_write_by_a_contract_that_declares_no_writes_names_the_plan_shape() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![
            (
                "t1",
                "write_file",
                json!({"path": "dist/index.html", "content": "x"}),
            ),
            ("t2", "anywhere", json!({})),
        ]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile), Arc::new(Anywhere)],
        CallMode::Stream,
    ));
    let mut b = brief("w1", "install and verify", worktree.path(), 6);
    b.write_scope = Some(Vec::new());
    let handle = spawn_worker(env, b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.denied, 2);
    assert!(!worktree.path().join("dist/index.html").exists());
    assert!(!worktree.path().join("outside.rs").exists());
    let refusals: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::ToolCallFinished {
                output, is_error, ..
            } if *is_error => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    // The refusal names the path in the platform's own form.
    let named = std::path::Path::new("dist").join("index.html");
    let write = refusals
        .iter()
        .find(|o| o.contains(&*named.to_string_lossy()))
        .unwrap_or_else(|| panic!("{refusals:?}"));
    // The phrase other readers key on stays.
    assert!(
        write.contains("is outside the declared write scope"),
        "{write}"
    );
    assert!(
        write.contains("this contract declares no writes"),
        "{write}"
    );
    assert!(write.contains("lists it in its writes"), "{write}");
    let shell = refusals
        .iter()
        .find(|o| o.contains("may write anywhere"))
        .unwrap_or_else(|| panic!("{refusals:?}"));
    assert!(shell.contains("declares no writes"), "{shell}");
}

/// Stands in for the orchestrator's `report_shortfall` tool: only its name
/// matters to the worker loop's refusal text.
struct ShortfallStandIn;

#[async_trait::async_trait]
impl Tool for ShortfallStandIn {
    fn name(&self) -> &str {
        kgraf_runtime::SHORTFALL_TOOL
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: kgraf_runtime::SHORTFALL_TOOL.into(),
            description: "report a shortfall".into(),
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
        Ok(ToolOutput::text("recorded"))
    }
}

/// Decision 0093: a call refused under a declared scope tells the worker to
/// report the shortfall rather than work around it, naming
/// `report_shortfall` when the environment offers it, and the result lists
/// the refusal as not retried. A call of the same capability that later
/// runs (the sandboxed build, unscoped) leaves nothing listed.
#[tokio::test]
async fn a_policy_refusal_says_how_to_report_it_and_is_listed_on_the_result() {
    for offered in [false, true] {
        let worktree = tempfile::tempdir().expect("worktree");
        let provider = Fake::new(true, |request| match round_of(request) {
            0 => Round::tool(vec![("t1", "anywhere", json!({}))]),
            _ => Round::answer("done"),
        });
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Anywhere)];
        if offered {
            tools.push(Arc::new(ShortfallStandIn));
        }
        let env = Arc::new(env_with(provider.clone(), tools, CallMode::Stream));
        let mut b = brief("w1", "run it", worktree.path(), 6);
        b.write_scope = Some(vec![std::path::PathBuf::from("src/a.rs")]);
        let handle = spawn_worker(env, b, SpawnOptions::default())
            .await
            .expect("spawn");
        let (events, result) = with_timeout(drain(handle)).await.expect("in time");
        let (output, _) = finished_output(&events, "t1");
        assert!(output.starts_with("ERROR: refused: "), "{output}");
        assert!(output.contains("do not work around it"), "{output}");
        assert_eq!(output.contains("report_shortfall"), offered, "{output}");
        assert_eq!(result.refused.len(), 1, "{:?}", result.refused);
        assert_eq!(result.refused[0].capability, "anywhere");
        assert!(!result.refused[0].retried_ok);
    }
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "anywhere", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Anywhere)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "run it", worktree.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(result.refused.is_empty(), "{:?}", result.refused);
}

/// A malformed patch is the worker's mistake, not a permission refusal: its
/// result says so and does not read "denied by", it is not listed as a
/// policy refusal, and the run's refusal directory holds it as the last
/// input error while the worker runs. A write outside the declared scope is
/// listed with its path, by the scope (no `by`).
#[tokio::test]
async fn a_malformed_patch_is_an_invalid_call_not_a_policy_refusal() {
    let worktree = tempfile::tempdir().expect("worktree");
    let directory = kgraf_runtime::RefusalDirectory::new();
    let seen = Arc::new(std::sync::Mutex::new(None));
    let (seen_in, dir_in) = (seen.clone(), directory.clone());
    let provider = Fake::new(true, move |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "apply_patch",
            json!({"patch": "--- a/discount_flat/src/lib.rs\n+++ b/discount_flat/src/lib.rs"}),
        )]),
        1 => {
            *seen_in.lock().expect("lock") = Some(dir_in.record_of("w1"));
            Round::tool(vec![(
                "t2",
                "apply_patch",
                json!({"patch": "*** Begin Patch\n*** Add File: checkout/src/lib.rs\n+x\n*** End Patch"}),
            )])
        }
        _ => Round::answer("done"),
    });
    let mut env = env_with(
        provider.clone(),
        vec![
            Arc::new(kgraf_tools::ApplyPatchTool),
            Arc::new(ShortfallStandIn),
        ],
        CallMode::Stream,
    );
    env.refusals = Some(directory.clone());
    let mut b = brief("w1", "edit it", worktree.path(), 6);
    b.write_scope = Some(vec![std::path::PathBuf::from("discount_flat/src/lib.rs")]);
    let handle = spawn_worker(Arc::new(env), b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    let (output, is_error) = finished_output(&events, "t1");
    assert!(is_error);
    assert!(output.starts_with("ERROR: invalid call: "), "{output}");
    assert!(
        output.contains("This is not a permission refusal"),
        "{output}"
    );
    assert!(!output.contains("denied by"), "{output}");
    assert!(!output.contains("report_shortfall"), "{output}");
    let record = seen.lock().expect("lock").clone().expect("read mid-run");
    assert!(record.refused.is_empty(), "{record:?}");
    assert!(
        record
            .last_input_error
            .as_deref()
            .is_some_and(|e| e.contains("Begin Patch")),
        "{record:?}"
    );
    assert_eq!(result.refused.len(), 1, "{:?}", result.refused);
    assert_eq!(
        result.refused[0].path.as_deref(),
        Some("checkout/src/lib.rs")
    );
    assert_eq!(result.refused[0].by, "");
    assert_eq!(
        directory.record_of("w1"),
        kgraf_runtime::RefusalRecord::default(),
        "a finished worker is forgotten"
    );
}

/// Declares what an allowlisted build from a workspace-bound shell tool
/// declares (decision 0084): exclusive, confined by the sandbox.
struct SandboxedBuild;

#[async_trait::async_trait]
impl Tool for SandboxedBuild {
    fn name(&self) -> &str {
        "build"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "build".into(),
            description: "a sandboxed build".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::sandboxed_build())
    }
    async fn call(
        &self,
        _input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        std::fs::write(ctx.workspace_root.join("built"), "x").map_err(|e| ToolError::Internal {
            tool: "build".into(),
            reason: e.to_string(),
        })?;
        Ok(ToolOutput::text("built"))
    }
}

/// Decision 0084: under a declared write scope, an allowlisted build runs,
/// where any other call that may write anywhere is refused (the test above).
#[tokio::test]
async fn a_sandboxed_build_runs_under_a_declared_scope() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "build", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(SandboxedBuild)],
        CallMode::Stream,
    ));
    let mut b = brief("w1", "build it", worktree.path(), 6);
    b.write_scope = Some(vec![std::path::PathBuf::from("src/a.rs")]);
    let handle = spawn_worker(env.clone(), b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.denied, 0);
    assert!(worktree.path().join("built").exists(), "the build ran");
}

/// A sandboxed build declares no paths, so the loop reads the worktree status
/// after it: a file the build wrote is a changed path in the result.
#[tokio::test]
async fn a_sandboxed_build_records_the_paths_it_changed() {
    let dir = tempfile::tempdir().expect("dir");
    let worktree = linked_worktree(dir.path());
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "build", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(SandboxedBuild)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "build it", &worktree, 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(
        result.changed_paths,
        vec![std::path::PathBuf::from("built")]
    );
    assert!(
        result.trace_failures.is_empty(),
        "{:?}",
        result.trace_failures
    );
}

/// A worktree git cannot read is reported on the result, not skipped.
#[tokio::test]
async fn an_unreadable_worktree_status_after_a_build_is_reported() {
    let worktree = tempfile::tempdir().expect("worktree");
    let missing = worktree.path().join("not-a-repository");
    std::fs::create_dir(&missing).expect("dir");
    // A fresh temp directory is not inside any repository on the test machine,
    // so git refuses to read its status.
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "build", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(SandboxedBuild)],
        CallMode::Stream,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "build it", &missing, 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(
        result
            .trace_failures
            .iter()
            .any(|f| f.contains("worktree status after build not read")),
        "{:?}",
        result.trace_failures
    );
}

/// A response cut off at the output ceiling with no tool call gets one
/// continuation round asking the model to split large patches.
#[tokio::test]
async fn an_output_ceiling_stop_gets_one_continuation_round() {
    let dir = tempfile::tempdir().expect("dir");
    // The abandoned round leaves no assistant turn, so count requests instead.
    let sent = AtomicUsize::new(0);
    let provider = Fake::new(false, move |_| match sent.fetch_add(1, Ordering::SeqCst) {
        0 => Round {
            text: "a very large patch, cut o".into(),
            stop: Some(StopReason::MaxTokens),
            usage: usage(100, 10),
            deltas: 1,
            ..Round::default()
        },
        _ => Round::answer("done in pieces"),
    });
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "patch", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.answer, "done in pieces");
    // The continuation is a counted round, with its own ledger row.
    assert_eq!(result.steps, 2);
    assert_eq!(result.calls.len(), 2);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let last = requests[1].messages.last().expect("last message");
    assert!(
        last.content.iter().any(|c| c
            .as_text()
            .is_some_and(|t| t.contains("split it into several smaller apply_patch"))),
        "{last:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            WorkerEvent::Notice {
                kind: NoticeKind::OutputLimit,
                ..
            }
        )),
        "the notice is recorded"
    );
    // The truncated text is not kept as an assistant turn.
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant),
        "the discarded response is not resent"
    );
}

/// A second ceiling stop in a row fails loud; there is no third attempt.
#[tokio::test]
async fn a_second_output_ceiling_stop_in_a_row_fails_the_run() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| Round {
        text: "cut o".into(),
        stop: Some(StopReason::MaxTokens),
        usage: usage(100, 10),
        deltas: 1,
        ..Round::default()
    });
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "patch", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    match &result.outcome {
        WorkerOutcome::Failed { error } => assert!(error.contains("output ceiling"), "{error}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(result.steps, 2);
}

/// A turn with no tool call and no visible text (a plan only in the model's
/// reasoning) is not a completed attempt: it is dropped, the model is told to
/// continue, and the notice round is counted.
#[tokio::test]
async fn an_empty_turn_gets_one_notice_round_and_is_not_completed() {
    let dir = tempfile::tempdir().expect("dir");
    let sent = AtomicUsize::new(0);
    let provider = Fake::new(false, move |_| match sent.fetch_add(1, Ordering::SeqCst) {
        0 => Round::answer(""),
        _ => Round::answer("done after the notice"),
    });
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "task", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.answer, "done after the notice");
    assert_eq!(result.steps, 2, "the notice round is counted");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let last = requests[1].messages.last().expect("last message");
    assert!(
        last.content.iter().any(|c| c
            .as_text()
            .is_some_and(|t| t.contains("without calling a tool or giving an answer"))),
        "{last:?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::Notice {
            kind: NoticeKind::EmptyTurn,
            ..
        }
    )));
}

/// A reply whose tool call the server dropped: no tool call, a short
/// announcement, and 1,500 output tokens counted for it.
fn lost_reply() -> Round {
    Round {
        usage: usage(100, 1500),
        ..Round::answer("I have the full picture now. Let me write the file.")
    }
}

fn lost_output_notices(events: &[WorkerEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                WorkerEvent::Notice {
                    kind: NoticeKind::LostOutput,
                    ..
                }
            )
        })
        .count()
}

/// A local server that cannot parse the model's tool call can drop it and
/// answer 200 with a plain stop. Taken as it arrived, the announcement would
/// end the attempt as completed with the file never written. The provider's
/// count shows the reply is short: the round is dropped, the model is told
/// its call did not arrive, and it sends the call again.
#[tokio::test]
async fn a_reply_the_server_cut_down_is_sent_again_not_taken_as_the_answer() {
    let dir = tempfile::tempdir().expect("dir");
    let sent = AtomicUsize::new(0);
    let provider = Fake::counting(move |_| match sent.fetch_add(1, Ordering::SeqCst) {
        0 => lost_reply(),
        1 => Round::tool(vec![(
            "t1",
            "write_file",
            json!({"path": "out.txt", "content": "written"}),
        )]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "write out.txt", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.answer, "done");
    assert!(dir.path().join("out.txt").exists(), "the resent call ran");
    assert_eq!(lost_output_notices(&events), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::RoundAbandoned { reason, .. }
            if reason == kgraf_runtime::LOST_OUTPUT_ABANDON_REASON
    )));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let told = requests[1].messages.last().expect("last message");
    assert!(
        told.content.iter().any(|c| c.as_text().is_some_and(|t| {
            t.contains("did not arrive complete") && t.contains("counted 1500 output tokens")
        })),
        "{told:?}"
    );
}

/// The retry is bounded per attempt: after `MAX_LOST_OUTPUT_RETRIES` notices a
/// short reply is judged as it arrived. A provider that cannot measure its
/// replies (the default) never triggers it.
#[tokio::test]
async fn lost_output_retries_are_bounded_and_need_a_measuring_provider() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::counting(|_| lost_reply());
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "task", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    let bound = usize::try_from(kgraf_runtime::MAX_LOST_OUTPUT_RETRIES).expect("small");
    assert_eq!(lost_output_notices(&events), bound);
    assert_eq!(provider.requests().len(), bound + 1);
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| lost_reply());
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "task", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(lost_output_notices(&events), 0);
    assert_eq!(provider.requests().len(), 1);
}

/// A reply that carries what it counts is an ordinary end of turn.
#[tokio::test]
async fn a_reply_that_carries_its_count_is_not_lost_output() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::counting(|_| Round {
        usage: usage(100, 200),
        ..Round::answer(&"x".repeat(800))
    });
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "task", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(lost_output_notices(&events), 0);
    assert_eq!(provider.requests().len(), 1);
}

/// A second empty turn in a row ends the attempt as failed, naming the cause.
#[tokio::test]
async fn two_empty_turns_in_a_row_fail_the_attempt() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| Round::answer("  \n"));
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "task", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    match &result.outcome {
        WorkerOutcome::Failed { error } => {
            assert!(error.contains("no tool call and no answer"), "{error}");
            assert!(
                error.contains("EndTurn"),
                "the stop reason is named: {error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 2, "one notice round, no third");
}

/// A turn with a tool call between two empty turns resets the count: only two
/// empty turns in a row fail.
#[tokio::test]
async fn a_tool_call_between_empty_turns_resets_the_empty_turn_count() {
    let dir = tempfile::tempdir().expect("dir");
    let sent = AtomicUsize::new(0);
    let provider = Fake::new(false, move |_| match sent.fetch_add(1, Ordering::SeqCst) {
        0 | 2 => Round::answer(""),
        1 => Round::tool(vec![("t1", "list_directory", json!({"path": "."}))]),
        _ => Round::answer("finished"),
    });
    let env = Arc::new(env_with(provider, vec![], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "task", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(result.answer, "finished");
}

/// Decision 0084: a subagent's fence still refuses a sandboxed build, which
/// could write the fenced paths inside the worktree.
#[tokio::test]
async fn a_sandboxed_build_is_still_refused_under_a_fence() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "build", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(SandboxedBuild)],
        CallMode::Stream,
    ));
    let mut b = brief("w1", "build it", worktree.path(), 6);
    b.write_scope = Some(vec![std::path::PathBuf::from("src/a.rs")]);
    let fence = ScopeFence::default();
    let _guard = fence.fence(&["src/a.rs".to_owned()]);
    let handle = spawn_worker(
        env.clone(),
        b,
        SpawnOptions {
            fence: Some(fence),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.denied, 1);
    assert!(!worktree.path().join("built").exists());
}

/// Decision 0067, point 5: a fenced worker with no declared write scope must
/// still refuse an exclusive call (a shell command, an MCP tool) rather than
/// let it write anywhere, because `ScopeFence::is_fenced` is never asked about
/// a specific path for a call that declares none.
#[tokio::test]
async fn a_tool_that_may_write_anywhere_is_refused_under_a_fence_with_no_declared_scope() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "anywhere", json!({}))]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Anywhere)],
        CallMode::Stream,
    ));
    let b = brief("w1", "run it", worktree.path(), 6);
    assert!(b.write_scope.is_none(), "no declared scope in this test");
    let fence = ScopeFence::default();
    let _guard = fence.fence(&["src/a.rs".to_owned()]);
    let handle = spawn_worker(
        env.clone(),
        b,
        SpawnOptions {
            fence: Some(fence),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(
        result.denied, 1,
        "the fence must refuse it, not let it through"
    );
    assert!(!worktree.path().join("outside.rs").exists());
}

/// Fences its declared path on a shared fence while it runs, as
/// `spawn_subagent` fences a writer child's paths on its parent (decision
/// 0067, point 5). The guard is kept, so the path stays fenced.
struct FenceIt {
    fence: ScopeFence,
    held: Mutex<Vec<kgraf_runtime::FenceGuard>>,
}

#[async_trait::async_trait]
impl Tool for FenceIt {
    fn name(&self) -> &str {
        "fence_it"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "fence_it".into(),
            description: "Hand a path to a child".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let path = input["path"].as_str().unwrap_or_default();
        Access::none()
            .write_path(path)
            .map_err(|source| ToolError::Access {
                tool: "fence_it".into(),
                source,
            })
    }
    async fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let path = input["path"].as_str().unwrap_or_default().to_owned();
        let guard = self.fence.fence(&[path]);
        self.held.lock().expect("lock").push(guard);
        Ok(ToolOutput::text("fenced"))
    }
}

/// Decision 0067 (disjoint writes): every call of a round is approved before any
/// of them runs, so a write issued in the same round as, and after, the call
/// that fences its path was approved while the path was still free. It must
/// be refused when it starts, not run while the child holds the path.
#[tokio::test]
async fn a_write_approved_before_its_path_was_fenced_is_refused_when_it_starts() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, |request| match round_of(request) {
        0 => Round::tool(vec![
            ("t1", "fence_it", json!({"path": "src/a.rs"})),
            (
                "t2",
                "write_file",
                json!({"path": "src/a.rs", "content": "parent"}),
            ),
            (
                "t3",
                "write_file",
                json!({"path": "src/b.rs", "content": "free"}),
            ),
        ]),
        _ => Round::answer("done"),
    });
    let fence = ScopeFence::default();
    let fence_it = Arc::new(FenceIt {
        fence: fence.clone(),
        held: Mutex::new(Vec::new()),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![fence_it.clone(), Arc::new(WriteFile)],
        CallMode::Stream,
    ));
    let b = brief("w1", "hand src/a.rs to a child", worktree.path(), 6);
    let handle = spawn_worker(
        env,
        b,
        SpawnOptions {
            fence: Some(fence.clone()),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("spawn");
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    let output_of = |id: &str| {
        events
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ToolCallFinished {
                    call_id,
                    output,
                    is_error,
                } if call_id == id => Some((output.clone(), *is_error)),
                _ => None,
            })
            .expect("a result")
    };
    let (out, is_error) = output_of("t2");
    assert!(is_error, "the write ran after its path was fenced: {out}");
    assert!(out.contains("fenced"), "{out}");
    assert!(
        !worktree.path().join("src/a.rs").exists(),
        "the fenced path was written"
    );
    assert!(
        !output_of("t3").1,
        "an unfenced write in the same round runs"
    );
    assert!(worktree.path().join("src/b.rs").exists());
    assert!(fence.is_fenced("src/a.rs"));
}

// ---------------------------------------------------------------- stall, ceiling, cancel and resume

/// A cancel that lands while a finished result waits for a bulk slot still
/// runs the grace: a sibling that finishes after the cancel is recorded, and the
/// path it wrote is a changed path.
#[tokio::test]
async fn a_cancel_during_a_bulk_emit_still_records_the_sibling_calls() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| {
        Round::tool(vec![
            ("a", "slow_write", json!({"path": "a.txt", "ms": 0})),
            ("b", "slow_write", json!({"path": "b.txt", "ms": 0})),
            ("c", "slow_write", json!({"path": "c.txt", "ms": 300})),
        ])
    });
    let mut env = env_with(provider, vec![Arc::new(SlowWriter)], CallMode::Complete);
    // One slot and nobody reading: the result of `a` fills it and the result of
    // `b` waits for a slot when the cancel arrives.
    env.settings.bulk_capacity = 1;
    let mut handle = spawn_worker(
        Arc::new(env),
        brief("w1", "write", dir.path(), 5),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut events = Vec::new();
    let mut started = 0;
    while started < 3 {
        let event = with_timeout(handle.next_event())
            .await
            .expect("in time")
            .expect("event");
        started += usize::from(matches!(event, WorkerEvent::ToolCallStarted { .. }));
        events.push(event);
    }
    for name in ["a.txt", "b.txt", "c.txt"] {
        while !dir.path().join(name).exists() {
            tokio::task::yield_now().await;
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.cancel();
    while let Some(event) = with_timeout(handle.next_event()).await.expect("in time") {
        events.push(event);
    }
    let result = handle.result().await.expect("result");
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    for id in ["a", "b", "c"] {
        assert!(
            events.iter().any(|e| matches!(
                e,
                WorkerEvent::ToolCallFinished { call_id, output, is_error: false }
                    if call_id == id && output == "written"
            )),
            "the result of {id} is recorded: {events:#?}"
        );
    }
    let mut paths = result.changed_paths.clone();
    paths.sort();
    assert_eq!(
        paths,
        ["a.txt", "b.txt", "c.txt"]
            .map(std::path::PathBuf::from)
            .to_vec()
    );
}

/// A cancel that arrives before the request leaves the process settles the call
/// as a known zero. An unreconciled row is for a request that may have billed.
#[tokio::test]
async fn a_cancel_before_the_request_leaves_never_leaves_an_unreconciled_row() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| Round::answer("done"));
    let mut env = env_with(provider.clone(), vec![], CallMode::Complete);
    // A file ledger writes on its own thread; an in-memory one writes inline and
    // leaves no gap for a cancel to land in.
    env.ledger = Arc::new(Ledger::open(&dir.path().join("ledger.db")).expect("ledger"));
    let env = Arc::new(env);
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "x", dir.path(), 3),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    // Queue rows ahead of the worker's own, so its ledger write waits and the
    // cancel arrives while it does: before the request can leave. The rows
    // stay open under another agent's name.
    let flood: Vec<_> = (0..4_000)
        .map(|_| {
            env.ledger
                .begin_async("fake", Protocol::Additive, MODEL, "flood", "flood-task")
        })
        .collect();
    // The worker now waits on the ledger write it needs to open its row.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    handle.cancel();
    let mut handle = handle;
    let mut events = Vec::new();
    while let Some(event) = with_timeout(handle.next_event()).await.expect("in time") {
        events.push(event);
    }
    let result = with_timeout(handle.result())
        .await
        .expect("in time")
        .expect("result");
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    drop(flood);
    assert!(
        provider.requests().is_empty(),
        "the cancel came before the request left"
    );
    // The round never left, so a resume from these events spends no round on it.
    let state =
        kgraf_runtime::ResumeState::from_events(kgraf_runtime::opening_messages("x", &[]), &events)
            .expect("resumable");
    assert!(
        events.iter().any(|e| matches!(
            e,
            WorkerEvent::RoundAbandoned { reason, .. }
                if reason == kgraf_runtime::CANCEL_BEFORE_SEND_REASON
        )),
        "{events:?}"
    );
    assert_eq!(state.rounds_used(), 0, "{events:?}");
    let rows: Vec<_> = env
        .ledger
        .calls()
        .expect("rows")
        .into_iter()
        .filter(|r| r.agent_id == "w1")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_ne!(
        rows[0].settlement,
        Settlement::Unreconciled,
        "no request was sent, so no spend is unknown"
    );
}

/// A tool call cut off at the output ceiling (the adapter drops it and reports
/// `MaxTokens`), with or without a complete call beside it, gets the
/// continuation round. No call of the round runs.
#[tokio::test]
async fn a_truncated_tool_call_at_the_output_ceiling_gets_the_continuation_round() {
    let dir = tempfile::tempdir().expect("dir");
    let sent = AtomicUsize::new(0);
    let provider = Fake::new(false, move |_| match sent.fetch_add(1, Ordering::SeqCst) {
        0 => Round {
            text: "writing".into(),
            calls: vec![(
                "t1".into(),
                "write_file".into(),
                json!({"path": "a.txt", "content": "x"}),
            )],
            stop: Some(StopReason::MaxTokens),
            usage: usage(100, 10),
            deltas: 1,
            ..Round::default()
        },
        _ => Round::answer("done in pieces"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env,
        brief("w1", "patch", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{events:#?}");
    assert!(
        !dir.path().join("a.txt").exists(),
        "a call of a round that hit the ceiling does not run"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::Notice {
            kind: NoticeKind::OutputLimit,
            ..
        }
    )));
    assert_eq!(provider.requests().len(), 2);
}

/// The continuation is a round of its own. When it would be the final round
/// (no tool calls run), the notices would contradict each other, so the ceiling
/// stop fails the run instead.
#[tokio::test]
async fn an_output_ceiling_stop_before_the_final_round_fails_instead_of_continuing() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| Round {
        text: "cut o".into(),
        stop: Some(StopReason::MaxTokens),
        usage: usage(100, 10),
        deltas: 1,
        ..Round::default()
    });
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    // A budget of two: the round after the first is the final round.
    let handle = spawn_worker(
        env,
        brief("w1", "patch", dir.path(), 2),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    match &result.outcome {
        WorkerOutcome::Failed { error } => assert!(error.contains("output ceiling"), "{error}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            WorkerEvent::Notice {
                kind: NoticeKind::OutputLimit,
                ..
            }
        )),
        "no continuation notice"
    );
}

/// A read-only worker never writes, so nothing resets its stall count. Once a
/// result is masked, re-reading the same file is not a loop.
#[tokio::test]
async fn re_reading_a_file_whose_result_was_masked_is_not_a_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| {
        let n = round_of(request);
        if n >= 13 {
            return Round::answer("done");
        }
        let id: &'static str = format!("t{n}").leak();
        let label = if n.is_multiple_of(6) {
            "same".to_owned()
        } else {
            format!("other-{n}")
        };
        // Long enough that the masker replaces it once it leaves the window.
        Round::tool(vec![(
            id,
            "probe",
            json!({"node": "n1", "label": label, "lines": 20}),
        )])
    });
    let env = Arc::new(env_with(provider, vec![probe], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "read", dir.path(), 30),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
}

/// A result short enough that the masker never replaces it stays in front of the
/// model, so a cycle longer than the window is still a loop.
#[tokio::test]
async fn a_cycle_of_short_results_longer_than_the_window_is_a_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let probe = Arc::new(Probe::default());
    let provider = Fake::new(false, |request| {
        let n = round_of(request);
        if n >= 25 {
            return Round::answer("done");
        }
        let id: &'static str = format!("t{n}").leak();
        let label = if n.is_multiple_of(6) {
            "same".to_owned()
        } else {
            format!("other-{n}")
        };
        Round::tool(vec![(id, "probe", json!({"node": "n1", "label": label}))])
    });
    let env = Arc::new(env_with(provider, vec![probe], CallMode::Complete));
    let handle = spawn_worker(
        env,
        brief("w1", "read", dir.path(), 40),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(
        matches!(result.outcome, WorkerOutcome::Stalled { .. }),
        "{:?}",
        result.outcome
    );
}

/// A slow build: writes at once, then keeps running and ignores the cancel.
struct SlowBuild;

#[async_trait::async_trait]
impl Tool for SlowBuild {
    fn name(&self) -> &str {
        "build"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "build".into(),
            description: "a slow sandboxed build".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::sandboxed_build())
    }
    async fn call(
        &self,
        _input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        std::fs::write(ctx.workspace_root.join("built"), "x").map_err(|e| ToolError::Internal {
            tool: "build".into(),
            reason: e.to_string(),
        })?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(ToolOutput::text("built"))
    }
}

/// A linked worktree under `dir`, as the host gives a worker: the status read
/// after a build runs only on a root holding a gitlink.
fn linked_worktree(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    let wt = dir.join("wt");
    std::fs::create_dir(&repo).expect("repo");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost.invalid"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&["worktree", "add", "-q", wt.to_str().expect("utf-8")]);
    wt
}

/// A build that finishes during the cancel grace still has its worktree changes
/// read: the paths it wrote are in the result.
#[tokio::test]
async fn a_build_that_finishes_during_the_cancel_grace_has_its_changes_recorded() {
    let dir = tempfile::tempdir().expect("dir");
    let worktree = linked_worktree(dir.path());
    let provider = Fake::new(false, |_| Round::tool(vec![("t1", "build", json!({}))]));
    let env = Arc::new(env_with(
        provider,
        vec![Arc::new(SlowBuild)],
        CallMode::Complete,
    ));
    let mut handle = spawn_worker(
        env,
        brief("w1", "build it", &worktree, 5),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    loop {
        let event = with_timeout(handle.next_event())
            .await
            .expect("in time")
            .expect("event");
        if matches!(event, WorkerEvent::ToolCallStarted { .. }) {
            break;
        }
    }
    while !worktree.join("built").exists() {
        tokio::task::yield_now().await;
    }
    handle.cancel();
    while with_timeout(handle.next_event())
        .await
        .expect("in time")
        .is_some()
    {}
    let result = handle.result().await.expect("result");
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    assert_eq!(
        result.changed_paths,
        vec![std::path::PathBuf::from("built")]
    );
}

/// A resumed worker learns what an earlier segment's build wrote from the
/// worktree, since the build declared no paths.
#[tokio::test]
async fn a_resume_recovers_the_paths_an_earlier_build_wrote() {
    let dir = tempfile::tempdir().expect("dir");
    let worktree = linked_worktree(dir.path());
    let resumed = build_then_resume(&worktree, |_| {}).await;
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert_eq!(
        resumed.changed_paths,
        vec![std::path::PathBuf::from("built")]
    );
}

/// A transcript written before the `Context` event recorded the gitlink has
/// neither `gitlink` nor `no_gitlink`. Its resume reads the entry the host's
/// reopen verified, still lists what the earlier build wrote, and records the
/// entry in its own `Context` for the next resume.
#[tokio::test]
async fn a_resume_of_an_old_transcript_reads_the_verified_gitlink_and_records_it() {
    let dir = tempfile::tempdir().expect("dir");
    let worktree = linked_worktree(dir.path());
    let gitlink = std::fs::read_to_string(worktree.join(".git")).expect("gitlink");
    let (second_segment, resumed) = build_then_resume_from(
        &worktree,
        |_| {},
        |events| {
            events
                .into_iter()
                .map(|event| {
                    // The stored form, less the fields the old format lacked.
                    let mut value = serde_json::to_value(&event).expect("to json");
                    if value["type"] == "context" {
                        let fields = value.as_object_mut().expect("object");
                        assert!(fields.remove("gitlink").is_some(), "{fields:?}");
                        fields.remove("no_gitlink");
                    }
                    serde_json::from_value(value).expect("an old transcript still reads")
                })
                .collect()
        },
    )
    .await;
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert_eq!(
        resumed.changed_paths,
        vec![std::path::PathBuf::from("built")]
    );
    let recorded = second_segment.iter().find_map(|event| match event {
        WorkerEvent::Context { gitlink, .. } => Some(gitlink.clone()),
        _ => None,
    });
    assert_eq!(recorded, Some(Some(gitlink.trim().to_owned())));
}

/// Run one segment whose build writes `built` and then fails, call
/// `between` (a crash window), and resume the worker to its answer.
async fn build_then_resume(root: &Path, between: impl FnOnce(&Path)) -> WorkerResult {
    build_then_resume_from(root, between, |events| events)
        .await
        .1
}

/// [`build_then_resume`], resuming from `stored(events)` (the transcript as
/// a store hands it back), and also returning the resumed segment's events.
async fn build_then_resume_from(
    root: &Path,
    between: impl FnOnce(&Path),
    stored: impl FnOnce(Vec<WorkerEvent>) -> Vec<WorkerEvent>,
) -> (Vec<WorkerEvent>, WorkerResult) {
    let first = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![("t0", "build", json!({}))]),
        _ => Round {
            fail: true,
            ..Round::answer("x")
        },
    });
    let env = Arc::new(env_with(
        first,
        vec![Arc::new(SandboxedBuild)],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "build it", root, 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first_result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(matches!(first_result.outcome, WorkerOutcome::Failed { .. }));
    between(root);

    let second = Fake::new(false, |_| Round::answer("done"));
    let mut env2 = env_with(second, vec![Arc::new(SandboxedBuild)], CallMode::Complete);
    env2.ledger = env.ledger.clone();
    let handle = spawn_worker(
        Arc::new(env2),
        brief("w1", "build it", root, 6),
        SpawnOptions {
            resume_from: Some(stored(events)),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    with_timeout(drain(handle)).await.expect("in time")
}

/// A root below the top of its work tree has no gitlink the host recorded.
/// A `.git` an earlier run planted there, naming a git directory whose
/// config runs a program, is not used on resume: the status is not read,
/// the result says so, and the program never runs.
#[cfg(unix)]
#[tokio::test]
async fn a_resume_does_not_trust_a_git_entry_planted_below_the_top() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("dir");
    let worktree = linked_worktree(dir.path());
    let root = worktree.join("sub");
    std::fs::create_dir(&root).expect("sub");
    let marker = dir.path().join("hook-ran");
    let hook = dir.path().join("hook.sh");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).expect("hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let planted = dir.path().join("planted");
    let resumed = build_then_resume(&root, |root| {
        // What an earlier command could leave: a repository outside the root
        // (a scratch directory is writable) and a gitlink naming it.
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&planted)
            .output()
            .expect("git init");
        assert!(out.status.success());
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&planted)
            .args(["config", "core.fsmonitor", hook.to_str().expect("utf-8")])
            .output()
            .expect("git config");
        assert!(out.status.success());
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", planted.join(".git").display()),
        )
        .expect("plant");
    })
    .await;
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    assert!(!marker.exists(), "a planted git directory must not be used");
    assert!(
        resumed
            .trace_failures
            .iter()
            .any(|f| f.contains("worktree status on resume not read")),
        "{:?}",
        resumed.trace_failures
    );
}

/// A worker whose prefix and opening alone reach its stop point can only hand
/// back at once with the same remainder. It is refused at spawn.
#[tokio::test]
async fn a_worker_that_starts_at_its_stop_point_is_refused_not_handed_back() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| Round::answer("never"));
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let mut b = brief("w1", "a task", dir.path(), 4);
    b.context = ContextConfig::for_model(10).expect("config");
    let Err(error) = spawn_worker(env, b, SpawnOptions::default()).await else {
        panic!("the worker must be refused");
    };
    assert!(
        matches!(
            error,
            kgraf_runtime::WorkerError::PinnedContextTooLarge { .. }
        ),
        "{error}"
    );
    assert!(provider.requests().is_empty());
}

/// Hook that rewrites the path of a write.
struct RedirectWrites;

#[async_trait::async_trait]
impl ApprovalHook for RedirectWrites {
    fn name(&self) -> &str {
        "redirect-writes"
    }
    async fn review(&self, call: &PendingCall) -> HookDecision {
        if call.tool == "write_file" && call.input["path"] == "a.txt" {
            let mut input = call.input.clone();
            input["path"] = json!("b.txt");
            HookDecision::Rewrite {
                input,
                reason: "redirect".into(),
            }
        } else {
            HookDecision::Allow
        }
    }
}

/// The write a rewritten call made is replayed from the input it ran with, not
/// from the input the model issued.
#[tokio::test]
async fn a_resume_replays_a_rewritten_call_from_the_input_it_ran_with() {
    let dir = tempfile::tempdir().expect("dir");
    let first = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t0",
            "write_file",
            json!({"path": "a.txt", "content": "x"}),
        )]),
        _ => Round {
            fail: true,
            ..Round::answer("x")
        },
    });
    let mut env = env_with(first, vec![Arc::new(WriteFile)], CallMode::Complete);
    env.approval = ApprovalChain::new(vec![Arc::new(RedirectWrites)]);
    let env = Arc::new(env);
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "write", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first_result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(matches!(first_result.outcome, WorkerOutcome::Failed { .. }));
    assert!(dir.path().join("b.txt").exists());
    assert!(!dir.path().join("a.txt").exists());

    let second = Fake::new(false, |_| Round::answer("done"));
    let mut env2 = env_with(second, vec![Arc::new(WriteFile)], CallMode::Complete);
    env2.ledger = env.ledger.clone();
    let handle = spawn_worker(
        Arc::new(env2),
        brief("w1", "write", dir.path(), 6),
        SpawnOptions {
            resume_from: Some(events),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(
        resumed.changed_paths,
        vec![std::path::PathBuf::from("b.txt")]
    );
}

/// The stall count of a resumed worker matches the live count: a round's calls
/// are counted together, and a write that succeeded in the round clears them.
#[tokio::test]
async fn a_resumed_stall_count_matches_the_live_count() {
    let dir = tempfile::tempdir().expect("dir");
    let same = json!({"node": "n1", "label": "same"});
    let probe = Arc::new(Probe::default());
    let first_same = same.clone();
    let first = Fake::new(false, move |request| match round_of(request) {
        0 => Round::tool(vec![
            (
                "t0w",
                "write_file",
                json!({"path": "x.txt", "content": "x"}),
            ),
            ("t0p", "probe", first_same.clone()),
        ]),
        1 => Round::tool(vec![("t1", "probe", first_same.clone())]),
        _ => Round {
            fail: true,
            ..Round::answer("x")
        },
    });
    let env = Arc::new(env_with(
        first,
        vec![Arc::new(WriteFile), probe.clone()],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "work", dir.path(), 12),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first_result) = with_timeout(drain(handle)).await.expect("in time");
    assert!(matches!(first_result.outcome, WorkerOutcome::Failed { .. }));

    // Live count of `same`: round 0 counted it and the write cleared it, round 1
    // counted it once. One more is the second, below the threshold of three.
    let second_same = same.clone();
    let second = Fake::new(false, move |request| match round_of(request) {
        2 => Round::tool(vec![("t2", "probe", second_same.clone())]),
        _ => Round::answer("done"),
    });
    let mut env2 = env_with(
        second,
        vec![Arc::new(WriteFile), probe.clone()],
        CallMode::Complete,
    );
    env2.ledger = env.ledger.clone();
    let handle = spawn_worker(
        Arc::new(env2),
        brief("w1", "work", dir.path(), 12),
        SpawnOptions {
            resume_from: Some(events),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
}

/// A result read back from storage that carried a pattern-scrub marker tells the
/// model to re-read; the file the marker stood for is not trusted from the
/// stored text.
#[tokio::test]
async fn a_resumed_history_marks_a_call_whose_stored_text_was_redacted() {
    let dir = tempfile::tempdir().expect("dir");
    let secret = format!("ghp_{}", "a".repeat(36));
    let first_secret = secret.clone();
    let first = Fake::new(false, move |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t0",
            "write_file",
            json!({"path": "x.txt", "content": first_secret.clone()}),
        )]),
        _ => Round {
            fail: true,
            ..Round::answer("x")
        },
    });
    let env = Arc::new(env_with(
        first,
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "write", dir.path(), 6),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    assert!(
        events.iter().any(|e| matches!(
            e,
            WorkerEvent::ToolCallStarted { input, .. }
                if input.to_string().contains("[redacted:github-pat")
        )),
        "the stored input is scrubbed"
    );

    let second = Fake::new(false, |_| Round::answer("done"));
    let mut env2 = env_with(
        second.clone(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    );
    env2.ledger = env.ledger.clone();
    let handle = spawn_worker(
        Arc::new(env2),
        brief("w1", "write", dir.path(), 6),
        SpawnOptions {
            resume_from: Some(events),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    // The path is intact, so the write is still replayed.
    assert_eq!(
        resumed.changed_paths,
        vec![std::path::PathBuf::from("x.txt")]
    );
    let request = second.requests().remove(0);
    let note = request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|c| {
            matches!(
                c,
                Content::ToolResult { content, .. } if content.contains("read back from storage")
            )
        });
    assert!(note, "the stored result says to re-read: {request:#?}");
}

// ---------------------------------------------------------------- skills (decision 0053)

/// The content hash of the `no-writes` skill in [`no_writes_catalog`]: a full
/// blake3 hex digest, as a real inventory records.
const NO_WRITES_HASH: &str = "4b1c7d6e0f3a92858e5b1f0c2d7a6e3940b8c1d2e3f4a5b6c7d8e9f0a1b2c3d4";

/// A skill catalog with `no-writes`, whose `allowed-tools` admits only the
/// `skill` tool, read the way a run reads the project's inventory.
fn no_writes_catalog() -> Arc<kgraf_session::skills::SkillCatalog> {
    no_writes_catalog_at(NO_WRITES_HASH)
}

/// [`no_writes_catalog`] with the skill at another version.
fn no_writes_catalog_at(hash: &str) -> Arc<kgraf_session::skills::SkillCatalog> {
    catalog_of(json!([{
        "name": "no-writes",
        "description": "look, do not touch",
        "frontmatter": {},
        "body": "Do not write files.",
        "files": [],
        "allowed_tools": ["skill"],
        "disable_model_invocation": false,
        "user_invocable": true,
        "hash": hash,
        "sources": [],
    }]))
}

/// A catalog of `skills`, read through an inventory file.
fn catalog_of(skills: Value) -> Arc<kgraf_session::skills::SkillCatalog> {
    let dir = tempfile::tempdir().expect("dir");
    let inventory = json!({
        "layers": [],
        "skills": skills,
        "conflicts": [],
        "problems": [],
    });
    std::fs::write(dir.path().join("inventory.json"), inventory.to_string()).expect("write");
    Arc::new(
        kgraf_session::skills::SkillCatalog::load(dir.path())
            .expect("load")
            .expect("present"),
    )
}

/// An environment with `WriteFile`, the `skill` tool and the catalog listed
/// in its prefix, as the host assembles one.
fn env_with_skills(provider: Arc<Fake>) -> WorkerEnv {
    env_with_catalog(provider, no_writes_catalog())
}

/// [`env_with_skills`] over `catalog`.
fn env_with_catalog(
    provider: Arc<Fake>,
    catalog: Arc<kgraf_session::skills::SkillCatalog>,
) -> WorkerEnv {
    let session = kgraf_session::skills::SkillSession::new(catalog);
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(WriteFile), Exposure::Prefix)
        .expect("register");
    registry
        .register_builtin(session.tool(), Exposure::Prefix)
        .expect("register");
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "fake-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a kGraf worker.")],
            tools: registry.prefix_definitions(),
            skill_catalog: session.model_catalog(),
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
    env.skills = Some(session);
    env
}

fn finished_output(events: &[WorkerEvent], id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            WorkerEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
            } if call_id == id => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("call {id} finished"))
}

/// A restriction survives a restart: the resumed segment re-activates the
/// skill its first segment activated, and a write it attempts is refused.
#[tokio::test]
async fn a_resumed_worker_keeps_the_skill_restriction_its_earlier_segment_activated() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![("t0", "skill", json!({"name": "no-writes"}))]),
        1 => Round::tool(vec![("t1", "skill", json!({"name": "no-writes"}))]),
        2 => Round::tool(vec![(
            "t2",
            "write_file",
            json!({"path": "b", "content": "b"}),
        )]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with_skills(provider));
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "look", worktree.path(), 2),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(first.outcome, WorkerOutcome::BudgetExhausted);
    let (activated, is_error) = finished_output(&events, "t0");
    assert!(
        !is_error && activated.contains("Do not write files."),
        "{activated}"
    );

    let handle = spawn_worker(
        env.clone(),
        brief("w1", "look", worktree.path(), 6),
        SpawnOptions {
            resume_from: Some(events),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (events, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
    let (refused, is_error) = finished_output(&events, "t2");
    assert!(is_error, "{refused}");
    assert!(
        refused.contains("not permitted by skill `no-writes`"),
        "{refused}"
    );
    assert!(!worktree.path().join("b").exists(), "the refused write ran");
}

/// Without the resume, the same environment lets a fresh worker write: the
/// restriction is the worker's, not the environment's.
#[tokio::test]
async fn a_skill_restriction_is_per_worker() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(false, |request| match round_of(request) {
        0 if instruction_of(request) == "look" => {
            Round::tool(vec![("t0", "skill", json!({"name": "no-writes"}))])
        }
        0 => Round::tool(vec![(
            "t0",
            "write_file",
            json!({"path": "c", "content": "c"}),
        )]),
        _ => Round::answer("done"),
    });
    let env = Arc::new(env_with_skills(provider));
    let restricted = spawn_worker(
        env.clone(),
        brief("w1", "look", worktree.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, first) = with_timeout(drain(restricted)).await.expect("in time");
    assert_eq!(first.outcome, WorkerOutcome::Completed);
    let other = spawn_worker(
        env,
        brief("w2", "write", worktree.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _) = with_timeout(drain(other)).await.expect("in time");
    let (written, is_error) = finished_output(&events, "t0");
    assert!(!is_error, "{written}");
    assert!(worktree.path().join("c").exists());
}

/// A prefix that lists skills the environment cannot activate, and an
/// environment with skills but no `skill` tool, are refused at spawn.
#[tokio::test]
async fn an_environment_whose_skills_and_prefix_disagree_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| Round::answer("x"));
    let mut env = env_with_skills(provider.clone());
    env.skills = None;
    assert!(matches!(
        spawn_worker(
            Arc::new(env),
            brief("w1", "x", dir.path(), 3),
            SpawnOptions::default()
        )
        .await,
        Err(WorkerError::SkillsMismatch(_))
    ));
    let mut env = env_with(provider, vec![], CallMode::Complete);
    env.skills = Some(kgraf_session::skills::SkillSession::new(no_writes_catalog()));
    assert!(matches!(
        spawn_worker(
            Arc::new(env),
            brief("w1", "x", dir.path(), 3),
            SpawnOptions::default()
        )
        .await,
        Err(WorkerError::SkillsMismatch(_))
    ));
}

/// The first segment of a worker that activated `no-writes`, for a resume.
async fn events_of_a_restricted_segment(worktree: &Path) -> Vec<WorkerEvent> {
    let provider = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![("t0", "skill", json!({"name": "no-writes"}))]),
        _ => Round::tool(vec![("t1", "skill", json!({"name": "no-writes"}))]),
    });
    let handle = spawn_worker(
        Arc::new(env_with_skills(provider)),
        brief("w1", "look", worktree, 2),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, first) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(first.outcome, WorkerOutcome::BudgetExhausted);
    events
}

/// A resume re-activates a skill only at the version the transcript
/// recorded: an edited skill, a removed one, or one the model may no longer
/// invoke refuses the resume, naming the skill and what to do.
#[tokio::test]
async fn a_resume_after_the_skill_changed_is_refused_with_the_fix() {
    let worktree = tempfile::tempdir().expect("worktree");
    let events = events_of_a_restricted_segment(worktree.path()).await;
    let answer = || Fake::new(false, |_| Round::answer("done"));
    let resume = |env: WorkerEnv| {
        let events = events.clone();
        let root = worktree.path().to_path_buf();
        async move {
            spawn_worker(
                Arc::new(env),
                brief("w1", "look", &root, 6),
                SpawnOptions {
                    resume_from: Some(events),
                    ..SpawnOptions::default()
                },
            )
            .await
        }
    };
    let edited = no_writes_catalog_at("9999aaaa");
    let refusal = match resume(env_with_catalog(answer(), edited)).await {
        Err(e @ WorkerError::SkillReplay { .. }) => e.to_string(),
        other => panic!("an edited skill must refuse the resume: {other:?}"),
    };
    assert!(refusal.contains("`no-writes`"), "{refusal}");
    assert!(refusal.contains(NO_WRITES_HASH), "{refusal}");
    assert!(refusal.contains("9999aaaa"), "{refusal}");
    assert!(
        refusal.contains("Restore the skill") && refusal.contains("new run"),
        "{refusal}"
    );

    let other = catalog_of(json!([{
        "name": "other",
        "description": "d",
        "frontmatter": {},
        "body": "b",
        "files": [],
        "allowed_tools": null,
        "disable_model_invocation": false,
        "user_invocable": true,
        "hash": "abcd",
        "sources": [],
    }]));
    assert!(matches!(
        resume(env_with_catalog(answer(), other)).await,
        Err(WorkerError::SkillReplay { .. })
    ));

    // A run with no skills at all: the removed inventory is named, not
    // reported as a different prefix.
    match resume(env_with(
        answer(),
        vec![Arc::new(WriteFile)],
        CallMode::Complete,
    ))
    .await
    {
        Err(e @ WorkerError::SkillReplay { .. }) => {
            assert!(e.to_string().contains("`no-writes`"), "{e}");
        }
        other => panic!("a run without skills must refuse the resume: {other:?}"),
    }

    // The same version resumes.
    let handle = resume(env_with_skills(answer())).await.expect("resume");
    let (_, resumed) = with_timeout(drain(handle)).await.expect("in time");
    assert_eq!(resumed.outcome, WorkerOutcome::Completed);
}

/// The version a resume is checked against survives the scrub a stored
/// transcript passes through.
#[tokio::test]
async fn the_recorded_skill_version_survives_the_transcript_scrub() {
    let worktree = tempfile::tempdir().expect("worktree");
    let events = events_of_a_restricted_segment(worktree.path()).await;
    let (output, is_error) = finished_output(&events, "t0");
    assert!(!is_error, "{output}");
    let stored = kgraf_secrets::scrub_persist(&output);
    assert_eq!(
        kgraf_session::skills::recorded_hash(&stored),
        Some(NO_WRITES_HASH)
    );
}

/// A call the skill's restriction refuses as it starts (the activation ran
/// earlier in the same round) is recorded as the approval chain records a
/// denial: with its prefix, and counted.
#[tokio::test]
async fn a_call_refused_as_it_starts_is_a_counted_denial() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![
            ("t0", "skill", json!({"name": "no-writes"})),
            ("t1", "write_file", json!({"path": "d", "content": "d"})),
        ]),
        _ => Round::answer("done"),
    });
    let handle = spawn_worker(
        Arc::new(env_with_skills(provider)),
        brief("w1", "look", worktree.path(), 4),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = with_timeout(drain(handle)).await.expect("in time");
    let (refused, is_error) = finished_output(&events, "t1");
    assert!(is_error, "{refused}");
    assert!(
        refused.starts_with("ERROR: denied by skills:allowed-tools: "),
        "{refused}"
    );
    assert_eq!(result.denied, 1);
    assert!(!worktree.path().join("d").exists(), "the refused write ran");
}

/// A subagent's inherited restriction applies even where the environment has
/// no skills of its own, and the handle exposes what its own children inherit.
#[tokio::test]
async fn inherited_restrictions_apply_in_an_environment_without_skills() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(false, |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t0",
            "write_file",
            json!({"path": "e", "content": "e"}),
        )]),
        _ => Round::answer("done"),
    });
    let inherited = vec![kgraf_session::skills::SkillRestriction {
        skill: "no-writes".into(),
        hash: NO_WRITES_HASH.into(),
        allowed_tools: vec!["skill".into()],
    }];
    let handle = spawn_worker(
        Arc::new(env_with(
            provider,
            vec![Arc::new(WriteFile)],
            CallMode::Complete,
        )),
        brief("w1.s1", "write", worktree.path(), 4),
        SpawnOptions {
            inherited_skills: inherited.clone(),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("spawn");
    let policy = handle.skills().expect("a policy");
    assert_eq!(policy.restrictions(), inherited);
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    let (refused, is_error) = finished_output(&events, "t0");
    assert!(is_error, "{refused}");
    assert!(
        refused.contains("not permitted by skill `no-writes`"),
        "{refused}"
    );
    assert!(!worktree.path().join("e").exists(), "the refused write ran");
}

/// Two rejected `apply_patch` calls in one attempt: from the second on, the
/// rejection ends with the rule it broke and a minimal valid patch for the
/// file the call targeted. The first rejection carries none, and a policy
/// refusal (a write outside the scope) is not a rejected patch and carries
/// none.
#[tokio::test]
async fn a_second_rejected_patch_carries_a_worked_example_for_its_file() {
    let worktree = tempfile::tempdir().expect("worktree");
    let provider = Fake::new(true, move |request| match round_of(request) {
        0 => Round::tool(vec![(
            "t1",
            "apply_patch",
            json!({"patch": "--- a/notes.md\n+++ b/notes.md"}),
        )]),
        1 => Round::tool(vec![(
            "t2",
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Add File: notes.md\n+x\ny\n*** End Patch"}),
        )]),
        2 => Round::tool(vec![(
            "t3",
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Add File: elsewhere/x.rs\n+x\n*** End Patch"}),
        )]),
        _ => Round::answer("done"),
    });
    let env = env_with(
        provider.clone(),
        vec![Arc::new(kgraf_tools::ApplyPatchTool)],
        CallMode::Stream,
    );
    let mut b = brief("w1", "edit it", worktree.path(), 8);
    b.write_scope = Some(vec![std::path::PathBuf::from("notes.md")]);
    let handle = spawn_worker(Arc::new(env), b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    let (first, _) = finished_output(&events, "t1");
    assert!(!first.contains("A minimal valid patch"), "{first}");
    let (second, is_error) = finished_output(&events, "t2");
    assert!(is_error);
    assert!(second.contains("Each file section starts with"), "{second}");
    assert!(
        second.contains("A minimal valid patch for this file:"),
        "{second}"
    );
    assert!(
        second.contains("*** Add File: notes.md\n+first line"),
        "{second}"
    );
    let (third, _) = finished_output(&events, "t3");
    assert!(third.starts_with("ERROR: refused: "), "{third}");
    assert!(!third.contains("A minimal valid patch"), "{third}");
}

/// Three rejected `apply_patch` calls in a row on one file: the third
/// rejection, and no earlier one, names the whole-file route. A rejection for a
/// different file keeps its own count, and a successful patch on a file clears
/// that file's count.
#[tokio::test]
async fn a_third_rejected_patch_on_one_file_names_the_whole_file_route() {
    let worktree = tempfile::tempdir().expect("worktree");
    std::fs::write(worktree.path().join("a.rs"), "one\n").expect("seed");
    std::fs::write(worktree.path().join("b.rs"), "one\n").expect("seed");
    let bad = |file: &str, n: u32| {
        json!({"patch": format!(
            "*** Begin Patch\n*** Update File: {file}\n@@\n-missing {n}\n+x\n*** End Patch"
        )})
    };
    let good =
        json!({"patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-one\n+two\n*** End Patch"});
    let provider = Fake::new(true, move |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "apply_patch", bad("a.rs", 1))]),
        1 => Round::tool(vec![("t2", "apply_patch", bad("a.rs", 2))]),
        2 => Round::tool(vec![("t3", "apply_patch", bad("b.rs", 3))]),
        3 => Round::tool(vec![("t4", "apply_patch", bad("a.rs", 4))]),
        4 => Round::tool(vec![("t5", "apply_patch", bad("b.rs", 5))]),
        5 => Round::tool(vec![("t6", "apply_patch", good.clone())]),
        6 => Round::tool(vec![("t7", "apply_patch", bad("a.rs", 7))]),
        _ => Round::answer("done"),
    });
    let env = env_with(
        provider.clone(),
        vec![Arc::new(kgraf_tools::ApplyPatchTool)],
        CallMode::Stream,
    );
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "edit it", worktree.path(), 12),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    let hint = "replace the whole file in one patch";
    for id in ["t1", "t2", "t3", "t5"] {
        let (out, is_error) = finished_output(&events, id);
        assert!(is_error, "{id}: {out}");
        assert!(!out.contains(hint), "{id}: {out}");
    }
    // a.rs: t1, t2, t4 fail in a row (the b.rs failure between does not reset it).
    let (t4, _) = finished_output(&events, "t4");
    assert!(t4.contains(hint), "{t4}");
    assert!(
        t4.contains("*** Delete File: a.rs\n*** Add File: a.rs\n+full new content"),
        "{t4}"
    );
    // b.rs failed twice only (t3, t5). The good patch on a.rs clears its
    // count, so t7 is a first failure again.
    let (t6, is_error) = finished_output(&events, "t6");
    assert!(!is_error, "{t6}");
    let (t7, is_error) = finished_output(&events, "t7");
    assert!(is_error, "{t7}");
    assert!(!t7.contains(hint), "{t7}");
}

/// `replace_in_file` goes through the worker loop's write-scope check as
/// `apply_patch` does: an edit inside the scope is applied, one outside is
/// refused and writes nothing. The whole-file hint after three failed patches
/// names the tool when the worker has it, and does not when it lacks it.
#[tokio::test]
async fn replace_in_file_is_held_to_the_write_scope_and_named_by_the_hint() {
    let worktree = tempfile::tempdir().expect("worktree");
    std::fs::write(worktree.path().join("a.rs"), "one\n").expect("seed");
    std::fs::write(worktree.path().join("b.rs"), "one\n").expect("seed");
    let bad = |n: u32| {
        json!({"patch": format!(
            "*** Begin Patch\n*** Update File: a.rs\n@@\n-missing {n}\n+x\n*** End Patch"
        )})
    };
    let provider = Fake::new(true, move |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "apply_patch", bad(1))]),
        1 => Round::tool(vec![("t2", "apply_patch", bad(2))]),
        2 => Round::tool(vec![("t3", "apply_patch", bad(3))]),
        3 => Round::tool(vec![(
            "t4",
            "replace_in_file",
            json!({"path": "b.rs", "old": "one", "new": "two"}),
        )]),
        4 => Round::tool(vec![(
            "t5",
            "replace_in_file",
            json!({"path": "a.rs", "old": "one", "new": "two"}),
        )]),
        _ => Round::answer("done"),
    });
    let env = env_with(
        provider.clone(),
        vec![
            Arc::new(kgraf_tools::ApplyPatchTool),
            Arc::new(kgraf_tools::ReplaceInFileTool::default()),
        ],
        CallMode::Stream,
    );
    let mut b = brief("w1", "edit it", worktree.path(), 12);
    b.write_scope = Some(vec![std::path::PathBuf::from("a.rs")]);
    let handle = spawn_worker(Arc::new(env), b, SpawnOptions::default())
        .await
        .expect("spawn");
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    let (t3, is_error) = finished_output(&events, "t3");
    assert!(is_error, "{t3}");
    assert!(t3.contains("call replace_in_file"), "{t3}");
    let (t4, is_error) = finished_output(&events, "t4");
    assert!(is_error, "{t4}");
    assert!(t4.contains("outside the declared write scope"), "{t4}");
    assert_eq!(
        std::fs::read_to_string(worktree.path().join("b.rs")).expect("read"),
        "one\n"
    );
    let (t5, is_error) = finished_output(&events, "t5");
    assert!(!is_error, "{t5}");
    assert_eq!(
        std::fs::read_to_string(worktree.path().join("a.rs")).expect("read"),
        "two\n"
    );

    // A worker without the tool is not pointed at it.
    let worktree = tempfile::tempdir().expect("worktree");
    std::fs::write(worktree.path().join("a.rs"), "one\n").expect("seed");
    let provider = Fake::new(true, move |request| match round_of(request) {
        0 => Round::tool(vec![("t1", "apply_patch", bad(1))]),
        1 => Round::tool(vec![("t2", "apply_patch", bad(2))]),
        2 => Round::tool(vec![("t3", "apply_patch", bad(3))]),
        _ => Round::answer("done"),
    });
    let env = env_with(
        provider.clone(),
        vec![Arc::new(kgraf_tools::ApplyPatchTool)],
        CallMode::Stream,
    );
    let handle = spawn_worker(
        Arc::new(env),
        brief("w1", "edit it", worktree.path(), 12),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _) = with_timeout(drain(handle)).await.expect("in time");
    let (t3, _) = finished_output(&events, "t3");
    assert!(t3.contains("replace the whole file in one patch"), "{t3}");
    assert!(!t3.contains("replace_in_file"), "{t3}");
}

// ------------------------------------------- reasoning-only turn: reasoning off

/// A turn that holds reasoning and nothing else.
fn reasoning_only() -> Round {
    Round {
        reasoning: "plan the edit step by step".repeat(40),
        ..Round::answer("")
    }
}

fn medium_env(provider: Arc<Fake>) -> WorkerEnv {
    let mut env = env_with(provider, vec![], CallMode::Complete);
    env.settings.reasoning = kgraf_provider::ReasoningLevels::new().with(
        kgraf_provider::ReasoningScope::All,
        kgraf_provider::ReasoningEffort::Medium.into(),
    );
    env
}

async fn run_one(env: &Arc<WorkerEnv>, dir: &Path) -> (Vec<WorkerEvent>, WorkerResult) {
    let handle = spawn_worker(
        env.clone(),
        brief("w1", "task", dir, 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    with_timeout(drain(handle)).await.expect("in time")
}

fn reasoning_off_notices(events: &[WorkerEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                WorkerEvent::Notice {
                    kind: NoticeKind::ReasoningOff,
                    ..
                }
            )
        })
        .count()
}

fn efforts_of(provider: &Fake) -> Vec<Option<kgraf_provider::ReasoningEffort>> {
    provider
        .requests()
        .iter()
        .map(|r| r.reasoning_effort)
        .collect()
}

/// A reasoning-only empty turn on a route that can turn reasoning off: the
/// continue notice stays, the next request carries reasoning off, and the
/// attempt goes on to a tool call.
#[tokio::test]
async fn a_reasoning_only_turn_is_followed_by_a_request_with_reasoning_off() {
    let dir = tempfile::tempdir().expect("dir");
    let off_rounds = AtomicUsize::new(0);
    let provider = Fake::turning_off(move |request| {
        if request.reasoning_effort != Some(kgraf_provider::ReasoningEffort::Off) {
            return reasoning_only();
        }
        match off_rounds.fetch_add(1, Ordering::SeqCst) {
            0 => Round::tool(vec![("t1", "list_directory", json!({"path": "."}))]),
            _ => Round::answer("finished"),
        }
    });
    let env = Arc::new(medium_env(provider.clone()));
    let (events, result) = run_one(&env, dir.path()).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    use kgraf_provider::ReasoningEffort::{Medium, Off};
    assert_eq!(
        efforts_of(&provider),
        vec![Some(Medium), Some(Off), Some(Off)]
    );
    assert_eq!(reasoning_off_notices(&events), 1, "once an attempt");
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::Notice {
            kind: NoticeKind::EmptyTurn,
            ..
        }
    )));
}

/// A route with no off level keeps today's behavior: the same level on the
/// retry, and a second empty turn fails the attempt.
#[tokio::test]
async fn a_route_without_an_off_level_keeps_the_empty_turn_behavior() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(false, |_| reasoning_only());
    let env = Arc::new(medium_env(provider.clone()));
    let (events, result) = run_one(&env, dir.path()).await;
    match &result.outcome {
        WorkerOutcome::Failed { error } => {
            assert!(error.contains("reasoning only: true"), "{error}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    use kgraf_provider::ReasoningEffort::Medium;
    assert_eq!(efforts_of(&provider), vec![Some(Medium), Some(Medium)]);
    assert_eq!(reasoning_off_notices(&events), 0);
}

/// Reasoning stays off for one attempt only: the next attempt (a new worker
/// on the same environment) starts at the configured level.
#[tokio::test]
async fn the_next_attempt_starts_at_the_configured_reasoning_level() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::turning_off(|request| {
        if request.reasoning_effort == Some(kgraf_provider::ReasoningEffort::Off) {
            Round::answer("finished")
        } else {
            reasoning_only()
        }
    });
    let env = Arc::new(medium_env(provider.clone()));
    let (_, first) = run_one(&env, dir.path()).await;
    assert_eq!(first.outcome, WorkerOutcome::Completed, "{first:?}");
    let (_, second) = run_one(&env, dir.path()).await;
    assert_eq!(second.outcome, WorkerOutcome::Completed, "{second:?}");
    use kgraf_provider::ReasoningEffort::{Medium, Off};
    assert_eq!(
        efforts_of(&provider),
        vec![Some(Medium), Some(Off), Some(Medium), Some(Off)]
    );
}

fn notices_of(events: &[WorkerEvent], wanted: NoticeKind) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, WorkerEvent::Notice { kind, .. } if *kind == wanted))
        .count()
}

/// The stub shape: every answer is reasoning only, and the server ignores the
/// reasoning-off request. The fallback was the worker's own, so the attempt
/// ends with the empty-turn reason, with one "ignored the fallback" notice,
/// and not with `ReasoningNotOff`. The route is then remembered: a later
/// attempt on the same environment does not try the fallback again.
#[tokio::test]
async fn a_route_that_ignores_the_reasoning_off_fallback_ends_as_an_empty_turn() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::turning_off_but_ignoring(|_| reasoning_only());
    let env = Arc::new(medium_env(provider.clone()));
    let (events, result) = run_one(&env, dir.path()).await;
    match &result.outcome {
        WorkerOutcome::Failed { error } => {
            assert!(error.contains("no tool call and no answer"), "{error}");
            assert!(
                !error.contains("ignored the reasoning-off setting"),
                "{error}"
            );
            assert!(!error.contains("Remove the setting"), "{error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    use kgraf_provider::ReasoningEffort::{Medium, Off};
    assert_eq!(efforts_of(&provider), vec![Some(Medium), Some(Off)]);
    assert_eq!(notices_of(&events, NoticeKind::EmptyTurn), 1);
    assert_eq!(notices_of(&events, NoticeKind::ReasoningOff), 1);
    assert_eq!(notices_of(&events, NoticeKind::ReasoningOffIgnored), 1);
    // The ignored answer was billed and recorded: one ledger call per request.
    assert_eq!(result.calls.len(), 2, "{:?}", result.calls);

    // A later attempt on the same route does not try the fallback again.
    let (events, result) = run_one(&env, dir.path()).await;
    assert!(
        matches!(&result.outcome, WorkerOutcome::Failed { error } if error.contains("no tool call and no answer")),
        "{result:?}"
    );
    assert_eq!(
        efforts_of(&provider),
        vec![Some(Medium), Some(Off), Some(Medium), Some(Medium)]
    );
    assert_eq!(reasoning_off_notices(&events), 0);
    assert_eq!(notices_of(&events, NoticeKind::ReasoningOffIgnored), 0);
}

/// A configured `off` that the server ignores is the user's setting, so the
/// call still fails with `ReasoningNotOff`, and the fallback is not involved.
#[tokio::test]
async fn a_configured_off_that_is_ignored_still_fails_with_reasoning_not_off() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::turning_off_but_ignoring(|_| reasoning_only());
    let mut env = env_with(provider.clone(), vec![], CallMode::Complete);
    env.settings.reasoning = kgraf_provider::ReasoningLevels::new().with(
        kgraf_provider::ReasoningScope::All,
        kgraf_provider::ReasoningEffort::Off.into(),
    );
    let env = Arc::new(env);
    let (events, result) = run_one(&env, dir.path()).await;
    match &result.outcome {
        WorkerOutcome::Failed { error } => {
            assert!(
                error.contains("ignored the reasoning-off setting"),
                "{error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(notices_of(&events, NoticeKind::ReasoningOffIgnored), 0);
    assert!(
        !env.reasoning_off_ignored
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}
