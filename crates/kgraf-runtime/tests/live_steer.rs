//! Live steering exchange (decision 0067). An adapter is
//! verified against a running speaker. One real worker per protocol makes a
//! tool call; a note is injected while the tool runs; the next request carries
//! `[tool results..., note]` in one user turn. The speaker must accept that
//! request, and its answer must follow the note, which only it can have read.
//! One more exchange (OpenAI Responses, the cheapest route) sends the note in
//! interrupt mode: the running tool call is cancelled, `RoundAbandoned` is
//! appended, and the speaker must accept the cancelled result plus the note.
//!
//! Ignored by default: it spends money. Keys come **only** from a
//! `KEY=VALUE` file named by `KGRAF_LIVE_ENV_FILE` (the project keeps them in
//! the repository root's `.env`; `HAUSTORIA_*` names are still read). The
//! process environment is not read for keys, so a run spends only against the
//! file the caller named:
//!
//! ```text
//! KGRAF_LIVE_STEER=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-runtime --test live_steer -- --ignored --nocapture --test-threads=1
//! ```
//!
//! A route whose key is absent is skipped and says so; nothing is faked. Each
//! worker has a spend ceiling, and the ledger's rows are priced and printed.

use kgraf_core::session::{InboxKind, InboxSource, InjectMode};
use kgraf_core::{ModelPrice, Money, PriceTable};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{Content, Ledger, Provider};
use kgraf_runtime::{
    CallMode, ContextConfig, Injection, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot,
    SharedPrefix, SpawnOptions, WorkerBrief, WorkerEnv, WorkerEvent, WorkerOutcome, spawn_worker,
};
use kgraf_tools::{CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Prices as recorded for the live checks (micro-dollars per million tokens).
fn price(model: &str) -> ModelPrice {
    match model {
        "kimi-k3" => ModelPrice {
            uncached_input: 3_000_000,
            cache_read: 300_000,
            cache_write: 3_000_000,
            output: 15_000_000,
            reasoning: 15_000_000,
        },
        "gpt-6-luna" => ModelPrice {
            uncached_input: 100_000,
            cache_read: 10_000,
            cache_write: 0,
            output: 500_000,
            reasoning: 500_000,
        },
        // From `third_party/litellm/model_prices_and_context_window.json`
        // (checked 2026-09-28): the cheapest current Anthropic model.
        "claude-haiku-4-5-20251001" => ModelPrice {
            uncached_input: 1_000_000,
            cache_read: 100_000,
            cache_write: 1_250_000,
            output: 5_000_000,
            reasoning: 5_000_000,
        },
        other => panic!("no recorded price for {other}"),
    }
}

/// Answers a lookup, after the test lets it. Returns at once if its token is
/// cancelled (a steer interrupt cancels read-only calls).
struct Lookup(Arc<Semaphore>);

#[async_trait::async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    /// Read-only. The trait's default is exclusive (may write anywhere), and
    /// a steer does not cancel a call that may write.
    fn access(&self, _input: &Value) -> Result<kgraf_tools::Access, ToolError> {
        Ok(kgraf_tools::Access::none().read_node("symbols".to_owned()))
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "lookup".into(),
            description: "Look up the value of a named symbol.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"symbol": {"type": "string"}},
                "required": ["symbol"]
            }),
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let permit = tokio::select! {
            () = ctx.cancel.cancelled() => {
                return Err(ToolError::Cancelled { tool: "lookup".into() });
            }
            permit = self.0.acquire() => permit.map_err(|e| ToolError::Internal {
                tool: "lookup".into(),
                reason: e.to_string(),
            })?,
        };
        permit.forget();
        let symbol = input["symbol"].as_str().unwrap_or_default();
        Ok(ToolOutput::text(format!("The value of {symbol} is 42.")))
    }
}

const NOTE: &str = "Note from the user: end your reply with the word PINEAPPLE.";

const INTERRUPT_NOTE: &str = "Note from the user: stop the lookup and do not call any tool \
                              again. Reply with one short sentence that ends with the word \
                              PINEAPPLE.";

/// One live exchange. Returns the cost the ledger rows price to, or `None`
/// when the route's key is absent.
async fn exchange(route: &str, model: &str, mode: InjectMode) -> Option<Money> {
    let entry = kgraf_fleet::entry(route).expect("catalog entry");
    let file = env_file();
    let lookup = |name: &str| kgraf_core::legacy::resolve_env(name, |n| file.get(n).cloned()).value;
    let credential = match kgraf_fleet::credential_from(entry, lookup) {
        Ok(key) => key,
        Err(e) => {
            println!("[{route}] SKIPPED: {e}");
            return None;
        }
    };
    let provider: Arc<dyn Provider> =
        Arc::from(kgraf_fleet::build(route, &credential, vec![model.to_owned()]).expect("build"));
    let gate = Arc::new(Semaphore::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(Lookup(gate.clone())), Exposure::Prefix)
        .expect("register");
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: route.to_owned(),
            model: model.to_owned(),
            system: vec![Content::text(
                "You are a kGraf worker. Use the tools you are given. Follow any note from \
                 the user that arrives during your work.",
            )],
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
    env.settings.call_mode = CallMode::Stream;
    env.settings.max_output_tokens = 1_500;
    let env = Arc::new(env);
    let dir = tempfile::tempdir().expect("dir");
    let brief = WorkerBrief {
        task: format!("live-steer-{route}").as_str().into(),
        worker: format!("live-steer-{route}").as_str().into(),
        instruction: "Call the lookup tool exactly once, with symbol \"alpha\". After you have \
                      its result, reply with one short sentence that states the value."
            .into(),
        workspace_root: dir.path().to_path_buf(),
        model: model.to_owned(),
        slices: vec![],
        skills: vec![],
        step_budget: 4,
        context: ContextConfig::for_model(200_000).expect("config"),
        write_scope: None,
    };
    let mut handle = spawn_worker(env, brief, SpawnOptions::default())
        .await
        .expect("spawn");
    let mut events = Vec::new();
    let opened = tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = handle.next_event().await {
            let started = matches!(event, WorkerEvent::ToolCallStarted { .. });
            events.push(event);
            if started {
                return true;
            }
        }
        false
    })
    .await
    .expect("the first round in time");
    assert!(opened, "[{route}] the model did not call the tool");
    let interrupt = mode == InjectMode::Interrupt;
    if interrupt {
        // `ToolCallStarted` is emitted before approval and execution: wait
        // until the lookup is running, so the interrupt cancels a live call.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    handle
        .injector()
        .inject(Injection {
            id: format!("live-{route}"),
            source: InboxSource::User,
            kind: InboxKind::Note,
            mode,
            text: if interrupt { INTERRUPT_NOTE } else { NOTE }.into(),
        })
        .expect("queued");
    if !interrupt {
        gate.add_permits(1);
    }
    let rest = tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = handle.next_event().await {
            // After the abandon, let any later lookup answer at once, so a
            // model that ignores the note cannot hang the test.
            if matches!(event, WorkerEvent::RoundAbandoned { .. }) {
                gate.add_permits(16);
            }
            events.push(event);
        }
        handle.result().await.expect("result")
    })
    .await
    .expect("the answer in time");

    let mut table = PriceTable::new();
    table.set(model.to_owned(), price(model));
    let rows = ledger.calls().expect("rows");
    let mut total = 0i64;
    for row in &rows {
        let cost = table.cost(model, &row.usage).expect("priced");
        total += cost.micros();
        println!(
            "[{route}] call {} {:?}: in {} (read {}, write {}), out {} (reasoning {}), ${:.6}",
            row.id,
            row.settlement,
            row.usage.total_input(),
            row.usage.cache_read,
            row.usage.cache_write,
            row.usage.total_output(),
            row.usage.reasoning_output,
            cost.micros() as f64 / 1e6
        );
    }
    let reasoning_seen = events
        .iter()
        .any(|e| matches!(e, WorkerEvent::ReasoningDelta { .. }));
    println!("[{route}] outcome {:?}", rest.outcome);
    println!("[{route}] answer: {}", rest.answer.trim());
    println!("[{route}] reasoning text streamed: {reasoning_seen}");
    println!("[{route}] total ${:.6}", total as f64 / 1e6);

    assert_eq!(rest.outcome, WorkerOutcome::Completed, "[{route}]");
    if interrupt {
        for event in &events {
            if let WorkerEvent::ToolCallFinished { output, .. } = event {
                println!("[{route}] tool result: {output}");
            }
        }
        let cancelled = format!(
            "ERROR: {}",
            ToolError::Cancelled {
                tool: "lookup".into()
            }
        );
        // The first lookup's result: cancelled while running (the case this
        // exchange aims at), or not run if the interrupt was armed before
        // the call started. Both close the tool use; which one
        // happened is printed.
        let first = events
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ToolCallFinished { output, .. } => Some(output.clone()),
                _ => None,
            })
            .expect("a result for the lookup");
        let path = if first == cancelled {
            "cancelled while running"
        } else if first == kgraf_runtime::STEER_NOT_RUN {
            "not started"
        } else {
            panic!("[{route}] unexpected lookup result after the interrupt: {first}")
        };
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::RoundAbandoned { .. })),
            "[{route}] the tool round was abandoned"
        );
        println!("[{route}] interrupt: lookup {path}, round abandoned, note injected");
    } else {
        assert_eq!(rows.len(), 2, "[{route}] one tool round, then the answer");
    }
    let injected = events
        .iter()
        .filter(|e| matches!(e, WorkerEvent::Injected { .. }))
        .count();
    assert_eq!(injected, 1, "[{route}] the note was delivered once");
    assert!(
        rest.answer.to_uppercase().contains("PINEAPPLE"),
        "[{route}] the answer does not follow the note: {}",
        rest.answer
    );
    Some(Money::from_micros(total))
}

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names, if it names one.
/// Read into memory only: nothing is set in the process environment, and no
/// value is printed. There is no default path, so without it every route is
/// skipped.
fn env_file() -> std::collections::BTreeMap<String, String> {
    let Ok(path) = std::env::var("KGRAF_LIVE_ENV_FILE") else {
        return Default::default();
    };
    let text = std::fs::read_to_string(&path).expect("KGRAF_LIVE_ENV_FILE is readable");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            (k.trim().to_owned(), v.to_owned())
        })
        .collect()
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_STEER").is_ok_and(|v| v == "1")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn live_anthropic_messages_accepts_a_note_after_tool_results() {
    if !live() {
        println!("KGRAF_LIVE_STEER is not 1; not run");
        return;
    }
    exchange("kimi-anthropic", "kimi-k3", InjectMode::AtBoundary).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn live_openai_responses_accepts_a_note_after_tool_results() {
    if !live() {
        println!("KGRAF_LIVE_STEER is not 1; not run");
        return;
    }
    exchange("openai", "gpt-6-luna", InjectMode::AtBoundary).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn live_chat_completions_accepts_a_note_after_tool_results() {
    if !live() {
        println!("KGRAF_LIVE_STEER is not 1; not run");
        return;
    }
    exchange("kimi", "kimi-k3", InjectMode::AtBoundary).await;
}

/// Anthropic's own endpoint (`https://api.anthropic.com`), not the
/// `kimi-anthropic` compatible path the exchange above uses. This is the live run
/// of this route's steer-after-tool-results case.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn live_anthropic_native_accepts_a_note_after_tool_results() {
    if !live() {
        println!("KGRAF_LIVE_STEER is not 1; not run");
        return;
    }
    exchange(
        "anthropic",
        "claude-haiku-4-5-20251001",
        InjectMode::AtBoundary,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn live_openai_responses_accepts_an_interrupt_mid_tool() {
    if !live() {
        println!("KGRAF_LIVE_STEER is not 1; not run");
        return;
    }
    exchange("openai", "gpt-6-luna", InjectMode::Interrupt).await;
}
