//! The steer path (decision 0067), tested on the mechanism: a scripted
//! provider that implements the real `Provider` and `Streaming` traits records
//! every request the loop sends, and each test asserts on those requests, the
//! ledger rows and the event stream, never on a setting.
//!
//! 1. The cache property: request k's messages are a byte prefix of request
//!    k+1's across an injection, and the shared prefix is unchanged.
//! 2. Placement: the note follows the tool results, and a notice, in one turn.
//! 3. Interrupt mid-stream: the call settles cancelled with its partial usage,
//!    `RoundAbandoned` is appended, and the next request is clean.
//! 4. Interrupt mid-tool: a call done within the grace keeps its result; the
//!    others get a synthetic result.
//! 5. Crash and resume: the resumed request is identical, and the note is in
//!    it once, whichever side of the append the crash fell on.
//! 6. A finished worker refuses at once; a leftover is reported.
//! 7. The session masker never changes injected text.

use kgraf_core::session::{InboxKind, InboxSource, InjectMode};
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::{Capabilities, StreamEvent, Streaming};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Message, Provider, ProviderError, Request, Response, Role,
    Settlement, StopReason,
};
use kgraf_runtime::{
    CallMode, CancelPhase, ContextConfig, InjectError, Injection, NeutralPrefixEncoder, NoMasking,
    PrefixRegistry, PrefixSnapshot, STEER_INTERRUPTED, STEER_NOT_RUN, SessionMasker, SharedPrefix,
    SpawnOptions, WorkerBrief, WorkerEnv, WorkerEvent, WorkerHandle, WorkerOutcome, WorkerResult,
    spawn_worker,
};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

// ---------------------------------------------------------------- scripted provider

#[derive(Clone, Debug, Default)]
struct Round {
    text: String,
    calls: Vec<(String, String, Value)>,
    stop: Option<StopReason>,
    usage: TokenUsage,
    deltas: usize,
    /// Stream: stop delivering and wait forever after this many deltas.
    hang_after: Option<usize>,
    /// Stream: after the first delta, wait for a permit from this semaphore.
    hold: Option<Arc<Semaphore>>,
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        uncached_input: input,
        text_output: output,
        ..TokenUsage::default()
    }
}

fn answer(text: &str) -> Round {
    Round {
        text: text.into(),
        stop: Some(StopReason::EndTurn),
        usage: usage(100, 10),
        deltas: 1,
        ..Round::default()
    }
}

fn tools(calls: Vec<(&str, &str, Value)>) -> Round {
    Round {
        text: "working".into(),
        calls: calls
            .into_iter()
            .map(|(i, n, v)| (i.to_owned(), n.to_owned(), v))
            .collect(),
        stop: Some(StopReason::ToolUse),
        usage: usage(100, 10),
        deltas: 1,
        ..Round::default()
    }
}

/// The script sees the request and the index of the call (0-based), so a
/// round abandoned mid-stream does not repeat itself.
type Script = dyn Fn(&Request, usize) -> Round + Send + Sync;

struct Scripted {
    script: Box<Script>,
    requests: Mutex<Vec<Request>>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(script: impl Fn(&Request, usize) -> Round + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }

    fn round(&self, request: &Request) -> Round {
        self.requests.lock().expect("lock").push(request.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        (self.script)(request, n)
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
        finish_reason: None,
        model: request.model.clone(),
        content,
        stop_reason: round.stop.unwrap_or(StopReason::EndTurn),
        usage: round.usage.clone(),
        request_id: None,
    }
}

#[async_trait::async_trait]
impl Provider for Scripted {
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
        let round = self.round(request);
        if round.hang_after.is_some() {
            std::future::pending::<()>().await;
        }
        token.settle(&round.usage, None)?;
        Ok(response(request, &round))
    }

    fn capabilities(&self) -> Capabilities<'_> {
        Capabilities::none().with_streaming(self)
    }
}

#[async_trait::async_trait]
impl Streaming for Scripted {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        let round = self.round(request);
        sink(StreamEvent::Started {
            model: request.model.clone(),
            request_id: Some("req-scripted".into()),
        });
        // The input side first, as Anthropic's message_start does.
        sink(StreamEvent::Usage {
            usage: TokenUsage {
                text_output: 0,
                ..round.usage.clone()
            },
        });
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
            if i == 0
                && let Some(hold) = &round.hold
            {
                hold.acquire().await.map(|p| p.forget()).ok();
            }
            tokio::task::yield_now().await;
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
        token.settle(&round.usage, None)?;
        Ok(response(request, &round))
    }
}

// ---------------------------------------------------------------- tools

/// Holds its call until the test releases a permit: the test injects while a
/// tool round is known to be open.
struct Gate(Arc<Semaphore>);

#[async_trait::async_trait]
impl Tool for Gate {
    fn name(&self) -> &str {
        "gate"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "gate".into(),
            description: "Wait for the test".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none().read_node("gate".to_owned()))
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let permit = self.0.acquire().await.map_err(|e| ToolError::Internal {
            tool: "gate".into(),
            reason: e.to_string(),
        })?;
        permit.forget();
        // Longer than the masking placeholder, so the masker can mask it.
        Ok(ToolOutput::text(vec!["gate opened"; 20].join("\n")))
    }
}

/// Sleeps `ms` and ignores cancellation, then returns `slept <label>` and
/// `lines` lines of output. Reads node `label`, or writes it when `write`.
struct Sleeper;

#[async_trait::async_trait]
impl Tool for Sleeper {
    fn name(&self) -> &str {
        "sleep"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "sleep".into(),
            description: "Sleep".into(),
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
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(Duration::from_millis(input["ms"].as_u64().unwrap_or(0))).await;
        let label = input["label"].as_str().unwrap_or_default();
        let lines = input["lines"].as_u64().unwrap_or(1) as usize;
        Ok(ToolOutput::text(
            vec![format!("slept {label}"); lines].join("\n"),
        ))
    }
}

// ---------------------------------------------------------------- harness

const MODEL: &str = "scripted-model";

fn env_with(provider: Arc<Scripted>, tools: Vec<Arc<dyn Tool>>, mode: CallMode) -> WorkerEnv {
    let mut registry = ToolRegistry::new();
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
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.masker = Arc::new(NoMasking);
    env.settings.call_mode = mode;
    env
}

fn brief(worker: &str, root: &Path, budget: u32) -> WorkerBrief {
    WorkerBrief {
        task: format!("task-{worker}").as_str().into(),
        worker: worker.into(),
        instruction: "do the task".into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: budget,
        context: ContextConfig::for_model(200_000).expect("config"),
        write_scope: None,
    }
}

fn note(id: &str, text: &str, mode: InjectMode) -> Injection {
    Injection {
        id: id.into(),
        source: InboxSource::User,
        kind: InboxKind::Note,
        mode,
        text: text.into(),
    }
}

fn with_timeout<F: std::future::Future>(f: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout(Duration::from_secs(10), f)
}

/// Read events until `stop` matches one (included).
async fn read_until(
    handle: &mut WorkerHandle,
    events: &mut Vec<WorkerEvent>,
    stop: impl Fn(&WorkerEvent) -> bool,
) {
    loop {
        let event = with_timeout(handle.next_event())
            .await
            .expect("in time")
            .expect("an event before the end");
        let hit = stop(&event);
        events.push(event);
        if hit {
            return;
        }
    }
}

/// Read events until the provider holds `n` requests. A request leaves only
/// once the reader has asked past every event before it
/// (`WorkerEndpoint::stored`), so the reader keeps reading while it waits; a
/// `ModelCall` event alone does not mean the request has left.
async fn read_until_requests(
    handle: &mut WorkerHandle,
    events: &mut Vec<WorkerEvent>,
    provider: &Scripted,
    n: usize,
) {
    with_timeout(async {
        while provider.requests().len() < n {
            tokio::select! {
                event = handle.next_event() => {
                    events.push(event.expect("an event before the end"));
                }
                () = tokio::time::sleep(Duration::from_millis(2)) => {}
            }
        }
    })
    .await
    .expect("in time");
}

async fn finish(
    mut handle: WorkerHandle,
    mut events: Vec<WorkerEvent>,
) -> (Vec<WorkerEvent>, WorkerResult) {
    while let Some(event) = with_timeout(handle.next_event()).await.expect("in time") {
        events.push(event);
    }
    let result = handle.result().await.expect("result");
    (events, result)
}

/// The request's messages as the bytes a byte-prefix cache compares.
fn messages_bytes(request: &Request) -> String {
    serde_json::to_string(&request.messages).expect("serialize")
}

/// Every text block of every message, in order.
fn texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Every tool use in `messages` has exactly one result in the next message.
fn assert_paired(messages: &[Message]) {
    for (i, m) in messages.iter().enumerate() {
        let uses: Vec<&str> = m
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        if uses.is_empty() {
            continue;
        }
        let next = messages.get(i + 1).expect("a turn after the tool uses");
        assert_eq!(next.role, Role::User);
        let results: Vec<&str> = next
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, uses, "every tool use is paired, in order");
    }
}

fn result_of<'a>(message: &'a Message, id: &str) -> (&'a str, bool) {
    message
        .content
        .iter()
        .find_map(|c| match c {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == id => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .expect("a result for the call")
}

// ---------------------------------------------------------------- 1. cache property

#[tokio::test]
async fn each_request_is_a_byte_prefix_of_the_next_across_an_injection() {
    let dir = tempfile::tempdir().expect("dir");
    let gate = Arc::new(Semaphore::new(0));
    let provider = Scripted::new(|_, n| match n {
        0 => tools(vec![("g1", "gate", json!({}))]),
        1 => tools(vec![("s1", "sleep", json!({"label": "x", "node": "x"}))]),
        _ => answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Gate(gate.clone())), Arc::new(Sleeper)],
        CallMode::Stream,
    ));
    let mut handle = spawn_worker(
        env.clone(),
        brief("w1", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut events = Vec::new();
    read_until(
        &mut handle,
        &mut events,
        |e| matches!(e, WorkerEvent::ToolCallStarted { tool, .. } if tool == "gate"),
    )
    .await;
    // The tool round is open: the note must wait for the boundary.
    handle
        .injector()
        .inject(note(
            "m1",
            "Also update the changelog.",
            InjectMode::AtBoundary,
        ))
        .expect("queued");
    gate.add_permits(1);
    let (events, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        let (before, after) = (messages_bytes(&pair[0]), messages_bytes(&pair[1]));
        // The earlier array without its closing bracket.
        let open = &before[..before.len() - 1];
        assert!(
            after.starts_with(open),
            "request k+1 must extend request k byte for byte\nk:   {before}\nk+1: {after}"
        );
        assert_eq!(
            pair[0].system, pair[1].system,
            "the shared prefix is unchanged"
        );
        assert_eq!(
            pair[0].tools, pair[1].tools,
            "the tool definitions are unchanged"
        );
    }
    for request in &requests {
        env.prefix.check(request).expect("the prefix check passes");
    }
    assert_eq!(
        result.prefix_digest,
        env.prefix.digest().as_str(),
        "the prefix digest is the snapshot's"
    );
    // The note reached the second request, once, and the event carries its id.
    let seen = texts(&requests[1].messages);
    assert_eq!(
        seen.iter()
            .filter(|t| **t == "Also update the changelog.")
            .count(),
        1
    );
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::Injected { id, kind: InboxKind::Note, .. } if id == "m1"
    )));
}

// ---------------------------------------------------------------- 2. placement

#[tokio::test]
async fn the_note_follows_the_tool_results_and_the_notice_in_one_user_turn() {
    let dir = tempfile::tempdir().expect("dir");
    let gate = Arc::new(Semaphore::new(0));
    let provider = Scripted::new(|_, n| match n {
        0 => tools(vec![("g1", "gate", json!({}))]),
        _ => answer("done"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Gate(gate.clone()))],
        CallMode::Stream,
    ));
    // A budget of 4: the second round has three left, so a notice joins it.
    let mut handle = spawn_worker(env, brief("w1", dir.path(), 4), SpawnOptions::default())
        .await
        .expect("spawn");
    let mut events = Vec::new();
    read_until(&mut handle, &mut events, |e| {
        matches!(e, WorkerEvent::ToolCallStarted { .. })
    })
    .await;
    handle
        .injector()
        .inject(note(
            "m1",
            "Prefer the smaller change.",
            InjectMode::AtBoundary,
        ))
        .expect("queued");
    gate.add_permits(1);
    let (events, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    let requests = provider.requests();
    let last = requests[1].messages.last().expect("a turn");
    assert_eq!(last.role, Role::User);
    assert!(
        matches!(&last.content[0], Content::ToolResult { tool_use_id, .. } if tool_use_id == "g1")
    );
    let notice = events
        .iter()
        .find_map(|e| match e {
            WorkerEvent::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("a notice on this round");
    assert_eq!(last.content[1], Content::text(notice));
    assert_eq!(last.content[2], Content::text("Prefer the smaller change."));
    assert_eq!(last.content.len(), 3);
}

// ---------------------------------------------------------------- 3. interrupt mid-stream

#[tokio::test]
async fn an_interrupt_mid_stream_settles_the_partial_call_and_the_next_request_is_clean() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Scripted::new(|_, n| match n {
        0 => tools(vec![("s1", "sleep", json!({"label": "a", "node": "a"}))]),
        1 => Round {
            text: "a partial answer that is abandoned".into(),
            stop: Some(StopReason::EndTurn),
            usage: usage(1_234, 50),
            deltas: 6,
            hang_after: Some(2),
            ..Round::default()
        },
        _ => answer("done after the note"),
    });
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Sleeper)],
        CallMode::Stream,
    ));
    let mut handle = spawn_worker(
        env.clone(),
        brief("w1", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut events = Vec::new();
    // The second model call's first delta: the stream is under way.
    read_until(
        &mut handle,
        &mut events,
        |e| matches!(e, WorkerEvent::TextDelta { text } if text.starts_with("a ")),
    )
    .await;
    handle
        .injector()
        .inject(note("m1", "Stop. Do Y instead.", InjectMode::Interrupt))
        .expect("queued");
    let (events, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(result.answer, "done after the note");

    // The abandoned call is settled cancelled with the partial usage.
    let rows = env.ledger.calls().expect("rows");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].settlement, Settlement::Cancelled);
    assert_eq!(rows[1].usage.uncached_input, 1_234);
    assert_eq!(rows[1].usage.text_output, 0);
    assert!(env.ledger.unreconciled().expect("query").is_empty());

    // RoundAbandoned, then its partial usage, then the injection.
    let abandoned = events
        .iter()
        .position(|e| {
            matches!(
                e,
                WorkerEvent::RoundAbandoned {
                    phase: CancelPhase::ModelStream,
                    ..
                }
            )
        })
        .expect("round abandoned");
    assert!(matches!(
        &events[abandoned + 1],
        WorkerEvent::Usage { call, usage } if *call == rows[1].id && usage.uncached_input == 1_234
    ));
    let injected = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::Injected { .. }))
        .expect("injected");
    assert!(injected > abandoned);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Cancelled { .. })),
        "an interrupt does not cancel the worker"
    );

    // The next request carries no partial text and no unpaired tool use.
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let next = &requests[2].messages;
    assert!(
        !texts(next).iter().any(|t| t.contains("partial")),
        "the abandoned text is dropped"
    );
    assert_paired(next);
    // The tool-result turn was already sent in request 2, so the note is a
    // turn of its own and request 2's messages are still a prefix.
    assert_eq!(next.last(), Some(&Message::user("Stop. Do Y instead.")));
    let before = messages_bytes(&requests[1]);
    assert!(messages_bytes(&requests[2]).starts_with(&before[..before.len() - 1]));
}

#[tokio::test]
async fn an_interrupt_during_a_complete_call_abandons_it_and_leaves_the_spend_unreconciled() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Scripted::new(|_, n| match n {
        0 => Round {
            hang_after: Some(0),
            ..answer("never")
        },
        _ => answer("done"),
    });
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
    let mut handle = spawn_worker(
        env.clone(),
        brief("w1", dir.path(), 8),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut events = Vec::new();
    // The call is in flight at the provider, which never answers it.
    read_until_requests(&mut handle, &mut events, &provider, 1).await;
    handle
        .injector()
        .inject(note("m1", "Change of plan.", InjectMode::Interrupt))
        .expect("queued");
    let (_, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    // No usage arrived, so the spend is unknown, not zero (every token is attributed).
    let unreconciled = env.ledger.unreconciled().expect("query");
    assert_eq!(unreconciled.len(), 1);
    // The first call was sent with the opening message alone, so the note is
    // a turn of its own after it: the opening instruction is not extended.
    let requests = provider.requests();
    assert_eq!(
        requests[1].messages,
        vec![
            requests[0].messages[0].clone(),
            Message::user("Change of plan.")
        ]
    );
}

// ---------------------------------------------------------------- 4. interrupt mid-tool

#[tokio::test]
async fn an_interrupt_mid_tool_keeps_results_done_within_the_grace_and_closes_the_rest() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Scripted::new(|_, n| match n {
        0 => tools(vec![
            (
                "quick",
                "sleep",
                json!({"label": "quick", "node": "q", "ms": 150}),
            ),
            (
                "slow",
                "sleep",
                json!({"label": "slow", "node": "s", "ms": 30_000, "write": true}),
            ),
            // Writes the node `slow` writes, so it waits and never starts.
            (
                "after",
                "sleep",
                json!({"label": "after", "node": "s", "write": true}),
            ),
        ]),
        _ => answer("done"),
    });
    let mut env = env_with(provider.clone(), vec![Arc::new(Sleeper)], CallMode::Stream);
    env.settings.tool_cancel_grace = Duration::from_millis(600);
    let env = Arc::new(env);
    let mut handle = spawn_worker(env, brief("w1", dir.path(), 8), SpawnOptions::default())
        .await
        .expect("spawn");
    let mut events = Vec::new();
    read_until(
        &mut handle,
        &mut events,
        |e| matches!(e, WorkerEvent::ToolCallStarted { call_id, .. } if call_id == "after"),
    )
    .await;
    // Let `quick` and `slow` start running.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let interrupted_at = Instant::now();
    handle
        .injector()
        .inject(note(
            "m1",
            "Leave the slow path alone.",
            InjectMode::Interrupt,
        ))
        .expect("queued");
    let (events, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(5),
        "the slow call was not waited for"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let turn = requests[1].messages.last().expect("a turn");
    assert_eq!(result_of(turn, "quick"), ("slept quick", false));
    // `slow` writes node `s`: not cancelled by the steer, it
    // outlives the grace and is reported as possibly having written it.
    assert_eq!(
        result_of(turn, "slow"),
        (
            format!("{STEER_INTERRUPTED} It may have written graph nodes: s.").as_str(),
            true
        )
    );
    assert_eq!(result_of(turn, "after"), (STEER_NOT_RUN, true));
    assert_eq!(
        turn.content.last(),
        Some(&Content::text("Leave the slow path alone.")),
        "the note follows the results in the same, unsent turn"
    );
    assert_paired(&requests[1].messages);
    let abandoned = events
        .iter()
        .position(|e| {
            matches!(
                e,
                WorkerEvent::RoundAbandoned {
                    phase: CancelPhase::ToolCall,
                    ..
                }
            )
        })
        .expect("round abandoned");
    let last_result = events
        .iter()
        .rposition(|e| matches!(e, WorkerEvent::ToolCallFinished { .. }))
        .expect("results");
    assert!(last_result < abandoned, "every call has its result first");
}

// ---------------------------------------------------------------- 5. crash and resume

#[tokio::test]
async fn a_crash_around_the_injection_resumes_to_the_same_request_with_the_note_once() {
    let dir = tempfile::tempdir().expect("dir");
    let gate = Arc::new(Semaphore::new(0));
    let script = |_: &Request, n: usize| match n {
        0 => tools(vec![("g1", "gate", json!({}))]),
        _ => answer("done"),
    };
    let provider = Scripted::new(script);
    let env = Arc::new(env_with(
        provider.clone(),
        vec![Arc::new(Gate(gate.clone()))],
        CallMode::Stream,
    ));
    let mut handle = spawn_worker(env, brief("w1", dir.path(), 8), SpawnOptions::default())
        .await
        .expect("spawn");
    let mut events = Vec::new();
    read_until(&mut handle, &mut events, |e| {
        matches!(e, WorkerEvent::ToolCallStarted { .. })
    })
    .await;
    let m1 = note("m1", "Keep the public API.", InjectMode::AtBoundary);
    handle.injector().inject(m1.clone()).expect("queued");
    gate.add_permits(1);
    let (events, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    let original = provider.requests()[1].clone();

    let injected_at = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::Injected { .. }))
        .expect("injected");
    // After the append, before the caller marked the row delivered: the
    // transcript has the event, and the inbox still offers the row.
    let after_append = events[..=injected_at].to_vec();
    // Before the append: the event never reached the transcript.
    let before_append = events[..injected_at].to_vec();

    for (label, transcript, duplicates) in [
        ("after the append", after_append, vec!["m1".to_owned()]),
        ("before the append", before_append, vec![]),
    ] {
        let gate = Arc::new(Semaphore::new(0));
        let provider = Scripted::new(|_, _| answer("done"));
        let env = Arc::new(env_with(
            provider.clone(),
            vec![Arc::new(Gate(gate))],
            CallMode::Stream,
        ));
        let handle = spawn_worker(
            env,
            brief("w1", dir.path(), 8),
            SpawnOptions {
                resume_from: Some(transcript),
                injections: vec![m1.clone()],
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("resume");
        assert_eq!(handle.already_delivered(), duplicates.as_slice(), "{label}");
        assert!(handle.listed_twice().is_empty(), "{label}");
        let injector = handle.injector();
        assert_eq!(
            injector.inject(m1.clone()),
            Err(InjectError::Duplicate { id: "m1".into() }),
            "{label}: the id is never injected twice"
        );
        let (resumed_events, result) = finish(handle, Vec::new()).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed, "{label}");
        let resumed = provider.requests();
        assert_eq!(
            messages_bytes(&resumed[0]),
            messages_bytes(&original),
            "{label}: the resumed request is the one the crashed worker sent"
        );
        assert_eq!(
            texts(&resumed[0].messages)
                .iter()
                .filter(|t| **t == "Keep the public API.")
                .count(),
            1,
            "{label}"
        );
        let delivered_again = resumed_events
            .iter()
            .filter(|e| matches!(e, WorkerEvent::Injected { .. }))
            .count();
        assert_eq!(delivered_again, duplicates.is_empty() as usize, "{label}");
    }
}

#[tokio::test]
async fn a_spawn_injection_is_delivered_once_at_the_first_boundary() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Scripted::new(|_, _| answer("done"));
    let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Stream));
    let handle = spawn_worker(
        env,
        brief("w1", dir.path(), 8),
        SpawnOptions {
            injections: vec![
                note("m1", "first", InjectMode::Interrupt),
                note("m1", "first", InjectMode::AtBoundary),
                note("m2", "second", InjectMode::AtBoundary),
            ],
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("spawn");
    assert_eq!(
        handle.listed_twice(),
        ["m1"],
        "the extra copy, reported apart"
    );
    assert!(
        handle.already_delivered().is_empty(),
        "no transcript: nothing was delivered before"
    );
    let (events, result) = finish(handle, Vec::new()).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    let ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Injected { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["m1", "m2"], "in order, once each");
    // Before the first call, the notes join the opening message.
    assert_eq!(
        texts(&provider.requests()[0].messages),
        ["do the task", "first", "second"]
    );
}

// ---------------------------------------------------------------- 6. finished worker

#[tokio::test]
async fn an_injection_into_a_finished_worker_is_refused_at_once_and_a_leftover_is_reported() {
    let dir = tempfile::tempdir().expect("dir");
    let hold = Arc::new(Semaphore::new(0));
    let held = hold.clone();
    let provider = Scripted::new(move |_, _| Round {
        deltas: 4,
        hold: Some(held.clone()),
        ..answer("the answer, streamed")
    });
    let env = Arc::new(env_with(provider, vec![], CallMode::Stream));
    let mut handle = spawn_worker(env, brief("w1", dir.path(), 8), SpawnOptions::default())
        .await
        .expect("spawn");
    let injector = handle.injector();
    let mut events = Vec::new();
    read_until(&mut handle, &mut events, |e| {
        matches!(e, WorkerEvent::TextDelta { .. })
    })
    .await;
    // Queued while the final answer streams: the worker never reaches another
    // boundary, so the item is a leftover, not a silent drop.
    injector
        .inject(note("late", "one more thing", InjectMode::AtBoundary))
        .expect("queued while running");
    hold.add_permits(1);
    let (events, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Injected { .. }))
    );
    assert!(injector.is_finished());
    assert_eq!(injector.undelivered(), ["late"]);

    let started = Instant::now();
    let refused = injector.inject(note("after", "hello?", InjectMode::Interrupt));
    assert_eq!(
        refused,
        Err(InjectError::Finished {
            worker: "w1".into()
        })
    );
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "it does not block"
    );
    assert_eq!(injector.undelivered(), ["late"], "a refusal queues nothing");
}

// ---------------------------------------------------------------- 7. masking

#[tokio::test]
async fn the_session_masker_leaves_injected_text_byte_identical_after_many_rounds() {
    let dir = tempfile::tempdir().expect("dir");
    let gate = Arc::new(Semaphore::new(0));
    const ROUNDS: usize = 12;
    let provider = Scripted::new(|_, n| match n {
        0 => tools(vec![("g", "gate", json!({}))]),
        n if n < ROUNDS => {
            // A distinct id and input per round, so no stall is detected.
            let mut round = tools(vec![]);
            round.calls = vec![(
                format!("t{n}"),
                "sleep".to_owned(),
                json!({"label": format!("round {n}"), "node": format!("n{n}"), "lines": 40}),
            )];
            round
        }
        _ => answer("done"),
    });
    let mut env = env_with(
        provider.clone(),
        vec![Arc::new(Gate(gate.clone())), Arc::new(Sleeper)],
        CallMode::Stream,
    );
    env.masker = Arc::new(SessionMasker::default());
    let env = Arc::new(env);
    let mut handle = spawn_worker(env, brief("w1", dir.path(), 20), SpawnOptions::default())
        .await
        .expect("spawn");
    let mut events = Vec::new();
    read_until(&mut handle, &mut events, |e| {
        matches!(e, WorkerEvent::ToolCallStarted { .. })
    })
    .await;
    // Long, and in a turn whose tool result will be masked many rounds later.
    let text = "Keep the migration reversible. ".repeat(40);
    handle
        .injector()
        .inject(note("m1", &text, InjectMode::AtBoundary))
        .expect("queued");
    gate.add_permits(1);
    let (_, result) = finish(handle, events).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);

    let requests = provider.requests();
    assert_eq!(requests.len(), ROUNDS + 1);
    let placeholder = SessionMasker::default().placeholder().to_owned();
    for (k, request) in requests.iter().enumerate().skip(1) {
        // The turn that carries the note is message 2 in every later request.
        let turn = &request.messages[2];
        assert_eq!(
            turn.content.last(),
            Some(&Content::text(text.clone())),
            "request {k}: the note is byte-identical"
        );
        if k == ROUNDS {
            assert_eq!(
                result_of(turn, "g").0,
                placeholder,
                "the masker did run over that same turn"
            );
        }
    }
}

// ---------------------------------------------------------------- regressions

/// Regression tests. Each one fails with its defect put back.
mod review {
    use super::*;
    use kgraf_runtime::{AskResolver, Injector, ObservationMasker, STEER_ABANDON_REASON};
    use kgraf_tools::approval::{
        ApprovalChain, ApprovalHook, HookDecision, PausedCall, PendingCall, UserAnswer,
    };
    use std::sync::OnceLock;
    use tokio::sync::Notify;

    /// Records the label of every call it starts, then sleeps `ms`. Reads
    /// node `node`, or writes it when `write`. Ignores cancellation.
    struct Recorder(Arc<Mutex<Vec<String>>>);

    #[async_trait::async_trait]
    impl Tool for Recorder {
        fn name(&self) -> &str {
            "rec"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "rec".into(),
                description: "Record".into(),
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
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            let label = input["label"].as_str().unwrap_or_default().to_owned();
            self.0.lock().expect("lock").push(label.clone());
            tokio::time::sleep(Duration::from_millis(input["ms"].as_u64().unwrap_or(0))).await;
            Ok(ToolOutput::text(format!("ran {label}")))
        }
    }

    /// Writes `path` after `ms`, unless cancelled first. Declares the write.
    struct SlowWrite;

    #[async_trait::async_trait]
    impl Tool for SlowWrite {
        fn name(&self) -> &str {
            "slow_write"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "slow_write".into(),
                description: "Write a file slowly".into(),
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
            tokio::select! {
                () = ctx.cancel.cancelled() => {
                    return Err(ToolError::Cancelled { tool: "slow_write".into() });
                }
                () = tokio::time::sleep(Duration::from_millis(input["ms"].as_u64().unwrap_or(0))) => {}
            }
            let path = ctx
                .workspace_root
                .join(input["path"].as_str().unwrap_or_default());
            std::fs::write(&path, "written").map_err(|e| ToolError::Internal {
                tool: "slow_write".into(),
                reason: e.to_string(),
            })?;
            Ok(ToolOutput::text("written"))
        }
    }

    /// A read-only call that returns as soon as its token is cancelled.
    struct WaitsForCancel;

    #[async_trait::async_trait]
    impl Tool for WaitsForCancel {
        fn name(&self) -> &str {
            "wait"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "wait".into(),
                description: "Wait".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        fn access(&self, _input: &Value) -> Result<Access, ToolError> {
            Ok(Access::none().read_node("w".to_owned()))
        }
        async fn call(
            &self,
            _input: Value,
            ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            tokio::select! {
                () = ctx.cancel.cancelled() => Err(ToolError::Cancelled { tool: "wait".into() }),
                () = tokio::time::sleep(Duration::from_secs(30)) => Ok(ToolOutput::text("waited")),
            }
        }
    }

    fn cancelled_text(tool: &str) -> String {
        format!("ERROR: {}", ToolError::Cancelled { tool: tool.into() })
    }

    fn assert_each_extends_the_last(requests: &[Request]) {
        for (k, pair) in requests.windows(2).enumerate() {
            let (before, after) = (messages_bytes(&pair[0]), messages_bytes(&pair[1]));
            assert!(
                after.starts_with(&before[..before.len() - 1]),
                "request {} must extend request {k}\n{k}: {before}\nnext: {after}",
                k + 1
            );
        }
    }

    #[tokio::test]
    async fn a_notice_after_an_abandoned_round_does_not_rewrite_the_sent_turn() {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![("s1", "sleep", json!({"label": "a", "node": "a"}))]),
            1 => Round {
                text: "a partial answer".into(),
                stop: Some(StopReason::EndTurn),
                usage: usage(10, 5),
                deltas: 4,
                hang_after: Some(1),
                ..Round::default()
            },
            _ => answer("done"),
        });
        let env = Arc::new(env_with(
            provider.clone(),
            vec![Arc::new(Sleeper)],
            CallMode::Stream,
        ));
        // Budget 5: round 3 has three left, so a notice comes with the note.
        let mut handle = spawn_worker(env, brief("w1", dir.path(), 5), SpawnOptions::default())
            .await
            .expect("spawn");
        let mut events = Vec::new();
        read_until(
            &mut handle,
            &mut events,
            |e| matches!(e, WorkerEvent::TextDelta { text } if text.starts_with("a ")),
        )
        .await;
        handle
            .injector()
            .inject(note("m1", "Change course.", InjectMode::Interrupt))
            .expect("queued");
        let (events, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Notice { .. }))
        );

        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert_each_extends_the_last(&requests);
        let last = requests[2].messages.last().expect("a turn");
        assert_eq!(last.role, Role::User);
        assert!(
            !last
                .content
                .iter()
                .any(|c| matches!(c, Content::ToolResult { .. })),
            "the notice and the note are a turn of their own"
        );
        assert_eq!(last.content.last(), Some(&Content::text("Change course.")));
    }

    #[tokio::test]
    async fn after_an_abandon_in_round_one_the_opening_instruction_is_not_extended() {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Scripted::new(|_, n| match n {
            0 => Round {
                text: "a partial answer".into(),
                stop: Some(StopReason::EndTurn),
                usage: usage(10, 5),
                deltas: 4,
                hang_after: Some(1),
                ..Round::default()
            },
            _ => answer("done"),
        });
        let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Stream));
        // Budget 4: round 2 has three left, so it carries a notice.
        let mut handle = spawn_worker(env, brief("w1", dir.path(), 4), SpawnOptions::default())
            .await
            .expect("spawn");
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::TextDelta { .. })
        })
        .await;
        handle
            .injector()
            .inject(note("m1", "Change course.", InjectMode::Interrupt))
            .expect("queued");
        let (_, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        let requests = provider.requests();
        assert_each_extends_the_last(&requests);
        assert_eq!(
            requests[1].messages[0], requests[0].messages[0],
            "the opening instruction is as it was first sent"
        );
    }

    #[tokio::test]
    async fn no_call_starts_after_the_interrupt_while_a_result_emit_is_blocked() {
        let dir = tempfile::tempdir().expect("dir");
        let ran = Arc::new(Mutex::new(Vec::new()));
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![
                ("a", "rec", json!({"label": "a", "node": "r", "ms": 10})),
                (
                    "b",
                    "rec",
                    json!({"label": "b", "node": "x", "write": true, "ms": 60}),
                ),
                (
                    "c",
                    "rec",
                    json!({"label": "c", "node": "x", "write": true, "ms": 10}),
                ),
            ]),
            _ => answer("done"),
        });
        let mut env = env_with(
            provider.clone(),
            vec![Arc::new(Recorder(ran.clone()))],
            CallMode::Stream,
        );
        env.settings.bulk_capacity = 1;
        let mut handle = spawn_worker(
            Arc::new(env),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut events = Vec::new();
        read_until(
            &mut handle,
            &mut events,
            |e| matches!(e, WorkerEvent::ToolCallStarted { call_id, .. } if call_id == "c"),
        )
        .await;
        // Not reading: `a`'s result takes the one bulk slot, and `b`'s result
        // emit then blocks. The interrupt arrives while it is blocked.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(*ran.lock().expect("lock"), ["a", "b"]);
        handle
            .injector()
            .inject(note("m1", "Stop there.", InjectMode::Interrupt))
            .expect("queued");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (_, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert_eq!(
            *ran.lock().expect("lock"),
            ["a", "b"],
            "c must not start after the interrupt"
        );
        let turn = provider.requests()[1]
            .messages
            .last()
            .expect("turn")
            .clone();
        assert_eq!(result_of(&turn, "b"), ("ran b", false));
        assert_eq!(result_of(&turn, "c"), (STEER_NOT_RUN, true));
    }

    struct AskFirst;

    #[async_trait::async_trait]
    impl ApprovalHook for AskFirst {
        fn name(&self) -> &str {
            "ask-first"
        }
        async fn review(&self, _call: &PendingCall) -> HookDecision {
            HookDecision::Ask {
                question: "run it?".into(),
            }
        }
    }

    struct HeldAsk {
        asked: Arc<Notify>,
        release: Arc<Semaphore>,
    }

    #[async_trait::async_trait]
    impl AskResolver for HeldAsk {
        async fn answer(&self, _paused: &PausedCall) -> UserAnswer {
            self.asked.notify_one();
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
            UserAnswer::Allow
        }
    }

    #[tokio::test]
    async fn an_interrupt_during_an_approval_ask_waits_for_it_then_starts_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let ran = Arc::new(Mutex::new(Vec::new()));
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![("a", "rec", json!({"label": "a", "node": "r"}))]),
            _ => answer("done"),
        });
        let mut env = env_with(
            provider.clone(),
            vec![Arc::new(Recorder(ran.clone()))],
            CallMode::Stream,
        );
        let asked = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        env.approval = ApprovalChain::new(vec![Arc::new(AskFirst)]);
        env.asks = Some(Arc::new(HeldAsk {
            asked: asked.clone(),
            release: release.clone(),
        }));
        let handle = spawn_worker(
            Arc::new(env),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let injector = handle.injector();
        let run = tokio::spawn(finish(handle, Vec::new()));
        with_timeout(asked.notified())
            .await
            .expect("the ask is pending");
        injector
            .inject(note("m1", "Never mind.", InjectMode::Interrupt))
            .expect("queued");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!run.is_finished(), "the ask is waited out, not interrupted");
        release.add_permits(1);
        let (events, result) = with_timeout(run).await.expect("in time").expect("join");
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert!(
            ran.lock().expect("lock").is_empty(),
            "the approved call never started"
        );
        let turn = provider.requests()[1]
            .messages
            .last()
            .expect("turn")
            .clone();
        assert_eq!(result_of(&turn, "a"), (STEER_NOT_RUN, true));
        assert_eq!(turn.content.last(), Some(&Content::text("Never mind.")));
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::RoundAbandoned { reason, .. } if reason == STEER_ABANDON_REASON
        )));
    }

    #[tokio::test]
    async fn a_steer_does_not_cancel_a_writing_call_that_finishes_within_the_grace() {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![(
                "w",
                "slow_write",
                json!({"path": "out.txt", "ms": 300}),
            )]),
            _ => answer("done"),
        });
        let mut env = env_with(
            provider.clone(),
            vec![Arc::new(SlowWrite)],
            CallMode::Stream,
        );
        env.settings.tool_cancel_grace = Duration::from_secs(3);
        let mut handle = spawn_worker(
            Arc::new(env),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::ToolCallStarted { .. })
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle
            .injector()
            .inject(note("m1", "Redirect.", InjectMode::Interrupt))
            .expect("queued");
        let (_, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        let turn = provider.requests()[1]
            .messages
            .last()
            .expect("turn")
            .clone();
        assert_eq!(result_of(&turn, "w"), ("written", false), "the real result");
        assert!(dir.path().join("out.txt").exists());
        assert_eq!(
            result.changed_paths,
            vec![std::path::PathBuf::from("out.txt")]
        );
    }

    #[tokio::test]
    async fn a_writing_call_that_outlives_the_grace_is_reported_as_may_have_written() {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![(
                "w",
                "slow_write",
                json!({"path": "late.txt", "ms": 30_000}),
            )]),
            _ => answer("done"),
        });
        let mut env = env_with(
            provider.clone(),
            vec![Arc::new(SlowWrite)],
            CallMode::Stream,
        );
        env.settings.tool_cancel_grace = Duration::from_millis(300);
        let mut handle = spawn_worker(
            Arc::new(env),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::ToolCallStarted { .. })
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle
            .injector()
            .inject(note("m1", "Redirect.", InjectMode::Interrupt))
            .expect("queued");
        let (_, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        let turn = provider.requests()[1]
            .messages
            .last()
            .expect("turn")
            .clone();
        assert_eq!(
            result_of(&turn, "w"),
            (
                // After the grace the call is stopped through its
                // own token, and what it said as it stopped is kept.
                format!(
                    "{STEER_INTERRUPTED} It may have written: late.txt. Its own result when \
                     stopped: {}",
                    cancelled_text("slow_write")
                )
                .as_str(),
                true
            )
        );
        assert_eq!(
            result.changed_paths,
            vec![std::path::PathBuf::from("late.txt")],
            "a possible write counts as changed"
        );
    }

    /// Decision 0080 removed the worker spend ceiling: a call abandoned with
    /// unknown usage no longer counts toward anything that stops the worker.
    /// It still stays unknown in the ledger (every token is attributed: nothing here silently
    /// invents a cost), and the worker runs until its step budget, not a
    /// money ceiling, stops it.
    #[tokio::test]
    async fn calls_abandoned_with_unknown_usage_stay_unreconciled_in_the_ledger() {
        let dir = tempfile::tempdir().expect("dir");
        // Every call hangs and reports nothing: only an interrupt ends it.
        let provider = Scripted::new(|_, _| Round {
            hang_after: Some(0),
            ..answer("never")
        });
        let env = Arc::new(env_with(provider.clone(), vec![], CallMode::Complete));
        let mut brief = brief("w1", dir.path(), 5);
        brief.instruction = "Refactor the parser carefully. ".repeat(100);
        let mut handle = spawn_worker(env.clone(), brief, SpawnOptions::default())
            .await
            .expect("spawn");
        let injector = handle.injector();
        // Interrupt three working rounds (never the final one, which the loop
        // always lets finish); the provider hangs forever, so a
        // fourth interrupt would never end the round to observe. Then cancel
        // outright, as any caller giving up on a hung worker would.
        let mut n = 0;
        let mut events = Vec::new();
        while n < 3 {
            // The call has reached the provider; interrupt it.
            read_until_requests(&mut handle, &mut events, &provider, n + 1).await;
            n += 1;
            injector
                .inject(note(&format!("m{n}"), "again", InjectMode::Interrupt))
                .expect("queued");
        }
        handle.cancel();
        let (_events, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Cancelled);
        let unreconciled = env.ledger.unreconciled().expect("query");
        assert_eq!(
            unreconciled.len(),
            n,
            "each abandoned call stays unknown in the ledger"
        );
    }

    #[tokio::test]
    async fn a_queued_note_that_would_pass_the_stop_point_hands_back_undelivered() {
        let dir = tempfile::tempdir().expect("dir");
        let gate = Arc::new(Semaphore::new(0));
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![("g1", "gate", json!({}))]),
            _ => answer("done"),
        });
        let env = Arc::new(env_with(
            provider.clone(),
            vec![Arc::new(Gate(gate.clone()))],
            CallMode::Stream,
        ));
        let mut handle = spawn_worker(env, brief("w1", dir.path(), 8), SpawnOptions::default())
            .await
            .expect("spawn");
        let injector = handle.injector();
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::ToolCallStarted { .. })
        })
        .await;
        // Far past a 200,000-token window's stop point on its own.
        let huge = "x".repeat(900_000);
        injector
            .inject(note("big", &huge, InjectMode::AtBoundary))
            .expect("queued");
        gate.add_permits(1);
        let (events, result) = finish(handle, events).await;
        assert!(
            matches!(result.outcome, WorkerOutcome::HandedBack { .. }),
            "{:?}",
            result.outcome
        );
        assert_eq!(
            provider.requests().len(),
            1,
            "no oversized request was sent"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Injected { .. }))
        );
        assert_eq!(injector.undelivered(), ["big"]);
    }

    #[tokio::test]
    async fn an_interrupt_in_the_final_round_lets_the_answer_finish_and_reports_the_note() {
        let dir = tempfile::tempdir().expect("dir");
        let hold = Arc::new(Semaphore::new(0));
        let held = hold.clone();
        let provider = Scripted::new(move |_, n| match n {
            0 => tools(vec![("s1", "sleep", json!({"label": "a", "node": "a"}))]),
            _ => Round {
                deltas: 4,
                hold: Some(held.clone()),
                ..answer("the final answer")
            },
        });
        let env = Arc::new(env_with(
            provider.clone(),
            vec![Arc::new(Sleeper)],
            CallMode::Stream,
        ));
        // Budget 2: the second round is the final one.
        let mut handle = spawn_worker(env, brief("w1", dir.path(), 2), SpawnOptions::default())
            .await
            .expect("spawn");
        let injector = handle.injector();
        let mut events = Vec::new();
        read_until(
            &mut handle,
            &mut events,
            |e| matches!(e, WorkerEvent::TextDelta { text } if text.starts_with("the")),
        )
        .await;
        injector
            .inject(note("late", "One more thing.", InjectMode::Interrupt))
            .expect("queued");
        tokio::time::sleep(Duration::from_millis(20)).await;
        hold.add_permits(1);
        let (events, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert_eq!(result.answer, "the final answer");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkerEvent::RoundAbandoned { .. }))
        );
        assert_eq!(injector.undelivered(), ["late"], "reported to the caller");
    }

    #[tokio::test]
    async fn a_steer_cancels_the_token_of_a_read_only_call() {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![("r", "wait", json!({}))]),
            _ => answer("done"),
        });
        let mut env = env_with(
            provider.clone(),
            vec![Arc::new(WaitsForCancel)],
            CallMode::Stream,
        );
        env.settings.tool_cancel_grace = Duration::from_secs(3);
        let mut handle = spawn_worker(
            Arc::new(env),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::ToolCallStarted { .. })
        })
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let started = Instant::now();
        handle
            .injector()
            .inject(note("m1", "Redirect.", InjectMode::Interrupt))
            .expect("queued");
        let (_, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the call saw the cancel and did not use the grace"
        );
        let turn = provider.requests()[1]
            .messages
            .last()
            .expect("turn")
            .clone();
        assert_eq!(
            result_of(&turn, "r"),
            (cancelled_text("wait").as_str(), true),
            "the call's own result: it saw its token cancelled"
        );
    }

    /// Injects an interrupt from inside the loop's window between the drain
    /// and the model call: the masker runs there, on the second request.
    struct InjectOnSecondMask {
        injector: Arc<OnceLock<Injector>>,
        calls: AtomicUsize,
    }

    impl ObservationMasker for InjectOnSecondMask {
        fn mask(&self, messages: &[Message]) -> Vec<Message> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1
                && let Some(injector) = self.injector.get()
            {
                injector
                    .inject(note("m1", "Wait.", InjectMode::Interrupt))
                    .expect("queued");
            }
            messages.to_vec()
        }
    }

    #[tokio::test]
    async fn an_interrupt_before_the_request_leaves_settles_the_call_at_a_known_zero() {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![("s1", "sleep", json!({"label": "a", "node": "a"}))]),
            _ => answer("done"),
        });
        let slot = Arc::new(OnceLock::new());
        let mut env = env_with(provider.clone(), vec![Arc::new(Sleeper)], CallMode::Stream);
        env.masker = Arc::new(InjectOnSecondMask {
            injector: slot.clone(),
            calls: AtomicUsize::new(0),
        });
        let env = Arc::new(env);
        let handle = spawn_worker(
            env.clone(),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        slot.set(handle.injector()).expect("set once");
        let (events, result) = finish(handle, Vec::new()).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        let rows = env.ledger.calls().expect("rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].settlement, Settlement::Cancelled);
        assert_eq!(rows[1].usage, TokenUsage::default(), "a known zero");
        assert!(
            env.ledger.unreconciled().expect("query").is_empty(),
            "nothing left the process, so no spend is unknown"
        );
        assert_eq!(
            provider.requests().len(),
            2,
            "the abandoned request never left"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Injected { .. }))
        );
        // The known zero is in the transcript too, as the abandoned call's
        // usage: that is what keeps it out of the ceiling's unknown spend.
        assert!(
            events.iter().any(|e| matches!(
                e,
                WorkerEvent::Usage { call, usage }
                    if *call == rows[1].id && *usage == TokenUsage::default()
            )),
            "the zero usage of the abandoned call is recorded"
        );
    }

    /// Injects an interrupt from inside the loop's window on each of the first
    /// `first` mask calls, so each of the first attempted rounds is abandoned
    /// before the request leaves.
    struct InjectOnFirstMasks {
        first: usize,
        injector: Arc<OnceLock<Injector>>,
        calls: AtomicUsize,
    }

    impl ObservationMasker for InjectOnFirstMasks {
        fn mask(&self, messages: &[Message]) -> Vec<Message> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.first
                && let Some(injector) = self.injector.get()
            {
                injector
                    .inject(note(&format!("m{n}"), "Wait.", InjectMode::Interrupt))
                    .expect("queued");
            }
            messages.to_vec()
        }
    }

    /// A round abandoned before the request leaves must not spend a round of
    /// the step budget: the budget allows exactly the three real model turns
    /// this script needs (two tool rounds, then the answer), with nothing to
    /// spare for the wasted attempt. Without the refund, the tool round that
    /// the second wasted round pushes into becomes the forced final round,
    /// which cannot dispatch a tool call, and the run ends `BudgetExhausted`
    /// with the model never having answered.
    #[tokio::test]
    async fn an_abandon_before_the_request_leaves_does_not_spend_a_budget_round() {
        let dir = tempfile::tempdir().expect("dir");
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![("s1", "sleep", json!({"label": "a", "node": "a"}))]),
            1 => tools(vec![("s2", "sleep", json!({"label": "b", "node": "b"}))]),
            _ => answer("done"),
        });
        let slot = Arc::new(OnceLock::new());
        let mut env = env_with(provider.clone(), vec![Arc::new(Sleeper)], CallMode::Stream);
        env.masker = Arc::new(InjectOnFirstMasks {
            first: 3,
            injector: slot.clone(),
            calls: AtomicUsize::new(0),
        });
        let env = Arc::new(env);
        let handle = spawn_worker(
            env.clone(),
            brief("w1", dir.path(), 3),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        slot.set(handle.injector()).expect("set once");
        let (events, result) = finish(handle, Vec::new()).await;
        assert_eq!(
            result.outcome,
            WorkerOutcome::Completed,
            "the wasted round must not cost the run its answer"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, WorkerEvent::RoundAbandoned { .. }))
                .count(),
            3,
            "each steered attempt was abandoned before it reached the provider"
        );
        assert_eq!(
            provider.requests().len(),
            3,
            "only the three real turns left the process"
        );
        // A rebuild from the stored events counts the three real rounds, not
        // the three refunded attempts.
        let mut rebuilt = kgraf_runtime::TranscriptBuilder::new(kgraf_runtime::opening_messages(
            "instruction",
            &[],
        ));
        for event in &events {
            rebuilt.apply(event);
        }
        assert_eq!(rebuilt.rounds(), result.steps);
        assert_eq!(result.steps, 3);
        // The refunded attempts record a zero usage each; it must not reset
        // the context estimate to nothing. Round two's real usage anchors it.
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Usage { usage, .. } if usage.total_input() + usage.total_output() == 0
        )));
        assert!(
            rebuilt.context_tokens() >= 110,
            "{}",
            rebuilt.context_tokens()
        );
        // The round-one notice is in the transcript once, not once per attempt.
        let steps_left = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    WorkerEvent::Notice {
                        kind: kgraf_runtime::NoticeKind::StepsLeft,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(steps_left, 1);
    }

    /// A bridge overflow that a steer's drain meets is reported on the result,
    /// not dropped: the first delta is larger than the bridge limit, the
    /// provider then holds, and the steer abandons the call.
    #[tokio::test]
    async fn a_bridge_overflow_met_while_draining_after_a_steer_is_reported() {
        let dir = tempfile::tempdir().expect("dir");
        let hold = Arc::new(Semaphore::new(0));
        let held = hold.clone();
        let provider = Scripted::new(move |_, n| match n {
            0 => Round {
                text: "0123456789".into(),
                usage: usage(10, 1),
                deltas: 1,
                hold: Some(held.clone()),
                ..Round::default()
            },
            _ => answer("done"),
        });
        let mut env = env_with(provider, vec![Arc::new(Sleeper)], CallMode::Stream);
        env.settings.bridge_limit_bytes = 5;
        let env = Arc::new(env);
        let mut handle = spawn_worker(
            env.clone(),
            brief("w1", dir.path(), 4),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut events = Vec::new();
        // The input-side usage precedes the oversized delta in the bridge.
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::Usage { .. })
        })
        .await;
        handle
            .injector()
            .inject(note("m1", "Wait.", InjectMode::Interrupt))
            .expect("queued");
        let (_, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert!(
            result
                .trace_failures
                .iter()
                .any(|f| f.contains("overflowed")),
            "the dropped overflow is reported: {:?}",
            result.trace_failures
        );
    }

    /// A shell command that backgrounds a grandchild outlives a steer's
    /// grace. The worker must stop it through `ShellTool`'s own cancel path,
    /// which kills the whole process group, not only drop its future.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_grandchild_is_killed_when_a_steers_grace_expires() {
        use kgraf_tools::ShellTool;
        use kgraf_tools::execpolicy::{Decision, ExecPolicy};
        let dir = tempfile::tempdir().expect("dir");
        let marker = dir.path().join("still-running");
        std::fs::write(&marker, "").expect("seed marker");
        let script = format!("(sleep 1.2 && rm -f {}) & sleep 30", marker.display());
        let provider = Scripted::new(move |_, n| match n {
            0 => tools(vec![(
                "sh",
                "shell",
                json!({"command": ["/bin/sh", "-c", script.clone()]}),
            )]),
            _ => answer("done"),
        });
        let (shell, _hook) = ShellTool::new(ExecPolicy::new(Decision::Allow));
        let mut env = env_with(provider.clone(), vec![Arc::new(shell)], CallMode::Stream);
        env.settings.tool_cancel_grace = Duration::from_millis(300);
        let mut handle = spawn_worker(
            Arc::new(env),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::ToolCallStarted { .. })
        })
        .await;
        // Let the command and its grandchild start.
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle
            .injector()
            .inject(note("m1", "Stop the build.", InjectMode::Interrupt))
            .expect("queued");
        let (_, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        let turn = provider.requests()[1]
            .messages
            .last()
            .expect("turn")
            .clone();
        let (output, is_error) = result_of(&turn, "sh");
        assert!(is_error);
        assert!(
            output.starts_with(STEER_INTERRUPTED) && output.contains("was cancelled"),
            "the synthetic result, then the shell's own cancel: {output}"
        );
        // Past the grandchild's 1.2 s: had it survived, the marker is gone.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            marker.exists(),
            "the backgrounded grandchild was killed with the command's group"
        );
    }

    /// A note of dense text (CJK: three bytes a character, and a token or
    /// more a character in real tokenizers), sized so that the plain
    /// three-bytes-a-token estimate stays under the stop point and the
    /// conservative margin does not. The worker hands back instead of
    /// sending it.
    #[tokio::test]
    async fn a_dense_cjk_note_is_counted_with_the_conservative_margin() {
        let dir = tempfile::tempdir().expect("dir");
        let gate = Arc::new(Semaphore::new(0));
        let provider = Scripted::new(|_, n| match n {
            0 => tools(vec![("g1", "gate", json!({}))]),
            _ => answer("done"),
        });
        let env = Arc::new(env_with(
            provider.clone(),
            vec![Arc::new(Gate(gate.clone()))],
            CallMode::Stream,
        ));
        let brief = brief("w1", dir.path(), 8);
        let stop = brief.context.worker_stop_tokens();
        let mut handle = spawn_worker(env, brief, SpawnOptions::default())
            .await
            .expect("spawn");
        let injector = handle.injector();
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::ToolCallStarted { .. })
        })
        .await;
        // About 80% of the stop point by the plain estimate (one token per
        // CJK character), about 120% with the margin.
        let chars = usize::try_from(stop * 8 / 10).expect("fits");
        let text: String = "漢".repeat(chars);
        assert!(
            kgraf_core::budget::estimate_tokens(&text) < stop * 9 / 10,
            "the plain estimate alone would let this note through"
        );
        injector
            .inject(note("dense", &text, InjectMode::AtBoundary))
            .expect("queued");
        gate.add_permits(1);
        let (_, result) = finish(handle, events).await;
        assert!(
            matches!(result.outcome, WorkerOutcome::HandedBack { .. }),
            "{:?}",
            result.outcome
        );
        assert_eq!(provider.requests().len(), 1, "the dense note was not sent");
        assert_eq!(injector.undelivered(), ["dense"]);
    }

    #[tokio::test]
    async fn a_cancel_during_a_steers_grace_still_records_the_calls_result() {
        let dir = tempfile::tempdir().expect("dir");
        let ran = Arc::new(Mutex::new(Vec::new()));
        let provider = Scripted::new(|_, n| match n {
            // A writing call, so the steer lets it run through the grace.
            0 => tools(vec![(
                "w",
                "rec",
                json!({"label": "w", "node": "x", "write": true, "ms": 300}),
            )]),
            _ => answer("done"),
        });
        let mut env = env_with(
            provider,
            vec![Arc::new(Recorder(ran.clone()))],
            CallMode::Stream,
        );
        env.settings.tool_cancel_grace = Duration::from_secs(3);
        let mut handle = spawn_worker(
            Arc::new(env),
            brief("w1", dir.path(), 8),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut events = Vec::new();
        read_until(&mut handle, &mut events, |e| {
            matches!(e, WorkerEvent::ToolCallStarted { .. })
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle
            .injector()
            .inject(note("m1", "Redirect.", InjectMode::Interrupt))
            .expect("queued");
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The worker is cancelled while the call runs in the steer's grace.
        handle.cancel();
        let (events, result) = finish(handle, events).await;
        assert_eq!(result.outcome, WorkerOutcome::Cancelled);
        assert!(
            events.iter().any(|e| matches!(
                e,
                WorkerEvent::ToolCallFinished { call_id, output, .. }
                    if call_id == "w" && output == "ran w"
            )),
            "the call's real result reaches the transcript after the cancel"
        );
    }
}
