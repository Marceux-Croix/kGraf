//! Decision 0043, asserted on the bytes the adapters actually send.
//!
//! Three workers with different briefs run through the real Anthropic Messages and
//! Chat Completions adapters against a local HTTP server. The server records each
//! request body exactly as received and plays prompt cache: it keys on the body's
//! prefix region (Anthropic: `tools` then `system`; Chat Completions: `tools` then
//! the leading `system` messages), cut from the received bytes. The runtime's own
//! prefix encoder plays no part in deciding a hit, so the test cannot pass by
//! agreeing with itself.

use kgraf_core::Protocol;
use kgraf_core::session::{InboxKind, InboxSource, InjectMode};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{Content, Ledger, Provider, ProviderConfig};
use kgraf_provider_anthropic::AnthropicAdapter;
use kgraf_provider_openai::{ChatCompletionsAdapter, OpenAiAdapter, UsageDialect};
use kgraf_runtime::{
    CallMode, ContextConfig, Injection, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot,
    SharedPrefix, SpawnOptions, WorkerBrief, WorkerEnv, WorkerEvent, WorkerOutcome, spawn_worker,
};
use kgraf_tools::{CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MODEL: &str = "wire-model";

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    Anthropic,
    Chat,
}

/// The prefix region of a received body, as bytes cut from that body.
fn prefix_region(wire: Wire, raw: &[u8]) -> Vec<u8> {
    let body: Value = serde_json::from_slice(raw).expect("json body");
    // The cut is faithful only if re-encoding the parsed body gives back the bytes
    // received (key order is preserved in this workspace). Asserted, not assumed.
    assert_eq!(
        serde_json::to_vec(&body).expect("encode"),
        raw,
        "the body does not round-trip; cutting regions from it would not be the wire bytes"
    );
    let mut out = serde_json::to_vec(&body["tools"]).expect("tools");
    match wire {
        Wire::Anthropic => out.extend(serde_json::to_vec(&body["system"]).expect("system")),
        Wire::Chat => {
            for message in body["messages"].as_array().expect("messages") {
                if message["role"] != "system" {
                    break;
                }
                out.extend(serde_json::to_vec(message).expect("system message"));
            }
        }
    }
    out
}

/// A provider-side prompt cache over the received bytes.
#[derive(Clone, Default)]
struct CachingServer {
    wire: Option<Wire>,
    cached: Arc<Mutex<HashSet<Vec<u8>>>>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Respond for CachingServer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let wire = self.wire.expect("wire");
        self.bodies.lock().expect("lock").push(request.body.clone());
        let prefix = prefix_region(wire, &request.body);
        let tokens = prefix.len() as u64 / 4;
        let hit = !self.cached.lock().expect("lock").insert(prefix);
        let model: Value =
            serde_json::from_slice::<Value>(&request.body).expect("json")["model"].clone();
        let body = match wire {
            Wire::Anthropic => json!({
                "id": "msg_wire",
                "model": model,
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {
                    "input_tokens": 50,
                    "cache_read_input_tokens": if hit { tokens } else { 0 },
                    "cache_creation_input_tokens": if hit { 0 } else { tokens },
                    "output_tokens": 5
                }
            }),
            // Chat Completions reports cached reads only; it has no write field.
            Wire::Chat => json!({
                "id": "chatcmpl-wire",
                "model": model,
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "usage": {
                    "prompt_tokens": 50 + tokens,
                    "prompt_tokens_details": {"cached_tokens": if hit { tokens } else { 0 }},
                    "completion_tokens": 5
                }
            }),
        };
        ResponseTemplate::new(200).set_body_json(body)
    }
}

struct Lookup;

#[async_trait::async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "lookup".into(),
            description: "Look a symbol up in the code graph".into(),
            input_schema: json!({"type": "object", "properties": {"symbol": {"type": "string"}}}),
        }
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("found"))
    }
}

async fn run_three(wire: Wire) -> (Vec<Vec<u8>>, Arc<Ledger>) {
    let server = MockServer::start().await;
    let responder = CachingServer {
        wire: Some(wire),
        ..CachingServer::default()
    };
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;
    let provider: Arc<dyn Provider> = match wire {
        Wire::Anthropic => {
            let config = ProviderConfig::new(
                "anthropic",
                server.uri(),
                "test-key",
                vec![MODEL.into()],
                Protocol::Additive,
            )
            .expect("config")
            .honoring_cache_control();
            Arc::new(AnthropicAdapter::new(config).expect("adapter"))
        }
        Wire::Chat => {
            let config = ProviderConfig::new(
                "openai-chat",
                server.uri(),
                "test-key",
                vec![MODEL.into()],
                Protocol::Inclusive,
            )
            .expect("config");
            Arc::new(ChatCompletionsAdapter::new(config, UsageDialect::Details).expect("adapter"))
        }
    };
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(Lookup), Exposure::Prefix)
        .expect("register");
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "wire-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a kGraf worker. Keep changes small.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("prefix");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger.clone(), provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    let env = Arc::new(env);
    let dir = tempfile::tempdir().expect("dir");
    for (worker, instruction) in [
        ("w1", "rename foo to bar"),
        ("w2", "add a test for baz"),
        ("w3", "fix the typo in qux"),
    ] {
        let brief = WorkerBrief {
            task: format!("task-{worker}").as_str().into(),
            worker: worker.into(),
            instruction: instruction.into(),
            workspace_root: dir.path().to_path_buf(),
            model: MODEL.into(),
            slices: vec![],
            skills: vec![],
            step_budget: 4,
            context: ContextConfig::for_model(200_000).expect("config"),
            write_scope: None,
        };
        let handle = spawn_worker(env.clone(), brief, SpawnOptions::default())
            .await
            .expect("spawn");
        let result = handle.result().await.expect("result");
        assert_eq!(result.outcome, WorkerOutcome::Completed, "{worker}");
    }
    let bodies = responder.bodies.lock().expect("lock").clone();
    (bodies, ledger)
}

fn assert_shared(wire: Wire, bodies: &[Vec<u8>]) {
    assert_eq!(bodies.len(), 3);
    let prefixes: Vec<Vec<u8>> = bodies.iter().map(|b| prefix_region(wire, b)).collect();
    assert!(
        prefixes.windows(2).all(|w| w[0] == w[1]),
        "the adapter sent different bytes before the breakpoint"
    );
    assert!(prefixes[0].len() > 100, "the prefix region is not empty");
    // The bodies differ, and only after the prefix: in each worker's own brief.
    let distinct: HashSet<&Vec<u8>> = bodies.iter().collect();
    assert_eq!(distinct.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_wire_bytes_share_the_prefix_and_the_ledger_shows_one_write_and_two_reads() {
    let (bodies, ledger) = run_three(Wire::Anthropic).await;
    assert_shared(Wire::Anthropic, &bodies);
    // The breakpoint the adapter emitted sits at the end of the shared region.
    let body: Value = serde_json::from_slice(&bodies[0]).expect("json");
    let system = body["system"].as_array().expect("system");
    assert_eq!(
        system.last().expect("block")["cache_control"]["type"],
        "ephemeral"
    );
    let rows = ledger.calls().expect("rows");
    let writes = rows.iter().filter(|r| r.usage.cache_write > 0).count();
    let reads = rows.iter().filter(|r| r.usage.cache_read > 0).count();
    assert_eq!((writes, reads), (1, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_completions_wire_bytes_share_the_prefix_and_two_of_three_calls_read_the_cache() {
    let (bodies, ledger) = run_three(Wire::Chat).await;
    assert_shared(Wire::Chat, &bodies);
    let rows = ledger.calls().expect("rows");
    let reads = rows.iter().filter(|r| r.usage.cache_read > 0).count();
    assert_eq!(reads, 2);
}

// ------------------------------------------------ decision 0067: steering on the wire

/// The three protocols a steering note must survive, each through its real
/// adapter.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SteerWire {
    Anthropic,
    Responses,
    Chat,
}

impl SteerWire {
    /// The body field that carries the conversation.
    fn conversation(self) -> &'static str {
        match self {
            SteerWire::Responses => "input",
            SteerWire::Anthropic | SteerWire::Chat => "messages",
        }
    }
}

/// First request: a text reply with reasoning and one call of `gate`. Second:
/// a final answer.
#[derive(Clone)]
struct ToolThenAnswer {
    wire: SteerWire,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Respond for ToolThenAnswer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let n = {
            let mut bodies = self.bodies.lock().expect("lock");
            bodies.push(request.body.clone());
            bodies.len()
        };
        let model: Value =
            serde_json::from_slice::<Value>(&request.body).expect("json")["model"].clone();
        let first = n == 1;
        let body = match self.wire {
            SteerWire::Anthropic => json!({
                "id": format!("msg_{n}"),
                "model": model,
                "stop_reason": if first { "tool_use" } else { "end_turn" },
                "content": if first {
                    json!([
                        {"type": "thinking", "thinking": "private chain of thought", "signature": "sig"},
                        {"type": "text", "text": "opening the gate"},
                        {"type": "tool_use", "id": "toolu_1", "name": "gate", "input": {}}
                    ])
                } else {
                    json!([{"type": "text", "text": "done"}])
                },
                "usage": {"input_tokens": 50, "output_tokens": 5}
            }),
            SteerWire::Responses => json!({
                "id": format!("resp_{n}"),
                "model": model,
                "status": "completed",
                "output": if first {
                    json!([
                        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "private chain of thought"}]},
                        {"type": "message", "content": [{"type": "output_text", "text": "opening the gate"}]},
                        {"type": "function_call", "call_id": "call_1", "name": "gate", "arguments": "{}"}
                    ])
                } else {
                    json!([{"type": "message", "content": [{"type": "output_text", "text": "done"}]}])
                },
                "usage": {"input_tokens": 50, "output_tokens": 5}
            }),
            SteerWire::Chat => json!({
                "id": format!("chatcmpl-{n}"),
                "model": model,
                "choices": [{
                    "index": 0,
                    "finish_reason": if first { "tool_calls" } else { "stop" },
                    "message": if first {
                        json!({
                            "role": "assistant",
                            "reasoning_content": "private chain of thought",
                            "content": "opening the gate",
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "gate", "arguments": "{}"}}]
                        })
                    } else {
                        json!({"role": "assistant", "content": "done"})
                    }
                }],
                "usage": {"prompt_tokens": 50, "completion_tokens": 5}
            }),
        };
        ResponseTemplate::new(200).set_body_json(body)
    }
}

/// Holds its call until the test releases it, so the note is injected while
/// the tool round is known to be open.
struct Gate(Arc<tokio::sync::Semaphore>);

#[async_trait::async_trait]
impl Tool for Gate {
    fn name(&self) -> &str {
        "gate"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "gate".into(),
            description: "Wait for the test".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
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
        Ok(ToolOutput::text("the gate is open"))
    }
}

const NOTE: &str = "Note from the user: keep the public API.";

/// Run one worker through `wire`'s real adapter: a tool round, a note injected
/// while the tool runs, then an answer. Returns the two bodies received.
async fn steer_through(wire: SteerWire) -> Vec<Value> {
    let server = MockServer::start().await;
    let responder = ToolThenAnswer {
        wire,
        bodies: Arc::default(),
    };
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;
    let provider: Arc<dyn Provider> = match wire {
        SteerWire::Anthropic => {
            let config = ProviderConfig::new(
                "anthropic",
                server.uri(),
                "test-key",
                vec![MODEL.into()],
                Protocol::Additive,
            )
            .expect("config")
            .honoring_cache_control();
            Arc::new(AnthropicAdapter::new(config).expect("adapter"))
        }
        SteerWire::Responses => {
            let config = ProviderConfig::new(
                "openai",
                server.uri(),
                "test-key",
                vec![MODEL.into()],
                Protocol::Inclusive,
            )
            .expect("config");
            Arc::new(OpenAiAdapter::new(config).expect("adapter"))
        }
        SteerWire::Chat => {
            let config = ProviderConfig::new(
                "openai-chat",
                server.uri(),
                "test-key",
                vec![MODEL.into()],
                Protocol::Inclusive,
            )
            .expect("config");
            Arc::new(ChatCompletionsAdapter::new(config, UsageDialect::Details).expect("adapter"))
        }
    };
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(Gate(gate.clone())), Exposure::Prefix)
        .expect("register");
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "wire-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a kGraf worker. Keep changes small.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("prefix");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    let env = Arc::new(env);
    let dir = tempfile::tempdir().expect("dir");
    let brief = WorkerBrief {
        task: "task-steer".into(),
        worker: "w-steer".into(),
        instruction: "open the gate, then report".into(),
        workspace_root: dir.path().to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 8,
        context: ContextConfig::for_model(200_000).expect("config"),
        write_scope: None,
    };
    let mut handle = spawn_worker(env, brief, SpawnOptions::default())
        .await
        .expect("spawn");
    loop {
        let event = handle.next_event().await.expect("an event");
        if matches!(event, WorkerEvent::ToolCallStarted { .. }) {
            break;
        }
    }
    handle
        .injector()
        .inject(Injection {
            id: "inbox-1".into(),
            source: InboxSource::User,
            kind: InboxKind::Note,
            mode: InjectMode::AtBoundary,
            text: NOTE.into(),
        })
        .expect("queued");
    gate.add_permits(1);
    let result = handle.result().await.expect("result");
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{wire:?}");
    let bodies = responder.bodies.lock().expect("lock").clone();
    assert_eq!(bodies.len(), 2, "{wire:?}");
    bodies
        .iter()
        .map(|raw| {
            let body: Value = serde_json::from_slice(raw).expect("json");
            // Cutting arrays out of the parsed body is faithful only if it
            // re-encodes to the received bytes. Asserted, not assumed.
            assert_eq!(&serde_json::to_vec(&body).expect("encode"), raw);
            body
        })
        .collect()
}

/// The second request's conversation extends the first's byte for byte, and
/// everything around the conversation is unchanged.
fn assert_extends(wire: SteerWire, bodies: &[Value]) {
    let conversation = wire.conversation();
    let first = serde_json::to_string(&bodies[0][conversation]).expect("encode");
    let second = serde_json::to_string(&bodies[1][conversation]).expect("encode");
    assert!(
        second.starts_with(&first[..first.len() - 1]),
        "{wire:?}: request 2 must extend request 1\n1: {first}\n2: {second}"
    );
    for field in ["tools", "system", "instructions", "model"] {
        assert_eq!(bodies[0][field], bodies[1][field], "{wire:?}: {field}");
    }
    let raw = bodies[1].to_string();
    assert!(
        !raw.contains("private chain of thought"),
        "{wire:?}: reasoning is never resent: {raw}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_wire_puts_the_note_after_the_tool_result_blocks_and_extends_the_request() {
    let bodies = steer_through(SteerWire::Anthropic).await;
    assert_extends(SteerWire::Anthropic, &bodies);
    let turn = bodies[1]["messages"]
        .as_array()
        .and_then(|m| m.last())
        .expect("a turn")
        .clone();
    assert_eq!(turn["role"], "user");
    assert_eq!(turn["content"][0]["type"], "tool_result");
    assert_eq!(turn["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(turn["content"][1], json!({"type": "text", "text": NOTE}));
    assert_eq!(turn["content"].as_array().map(Vec::len), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn responses_wire_puts_the_note_after_the_function_call_output_and_extends_the_request() {
    let bodies = steer_through(SteerWire::Responses).await;
    assert_extends(SteerWire::Responses, &bodies);
    let input = bodies[1]["input"].as_array().expect("input");
    let n = input.len();
    assert_eq!(input[n - 2]["type"], "function_call_output");
    assert_eq!(input[n - 2]["call_id"], "call_1");
    assert_eq!(input[n - 1], json!({"role": "user", "content": NOTE}));
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_wire_puts_the_note_after_the_tool_message_and_extends_the_request() {
    let bodies = steer_through(SteerWire::Chat).await;
    assert_extends(SteerWire::Chat, &bodies);
    let messages = bodies[1]["messages"].as_array().expect("messages");
    let n = messages.len();
    assert_eq!(messages[n - 2]["role"], "tool");
    assert_eq!(messages[n - 2]["tool_call_id"], "call_1");
    assert_eq!(messages[n - 1], json!({"role": "user", "content": NOTE}));
}
