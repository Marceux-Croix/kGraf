//! Harness budgets. Each benchmark's median is asserted against a
//! budget at the end of the run, so a regression fails `cargo bench`, not only a
//! reader's eye.
//!
//! - `turn_overhead/*`: one model round plus one tool call, outside the model call,
//!   with a provider that answers at once. Includes the ledger row (begin plus
//!   settle), masking, request build and prefix check, the event channel and
//!   transcript builder, approval (an empty chain, recorded) and dispatch of one
//!   no-op tool. A worker runs `ROUNDS` tool rounds and an answer round; the time
//!   reported is per round, with spawn amortized over them.
//! - `spawn_teardown/*`: spawn to result. `spawn_then_cancel` cancels at once a
//!   worker whose provider never answers (in-memory ledger); `one_round` runs one
//!   answer round (on-disk ledger).
//! - `cancel_to_release/*`: from `cancel()` to the result in hand, the event channel
//!   closed and the ledger row settled. `http_in_flight` cancels a real HTTP request
//!   through the Chat Completions adapter against a local server that accepts the
//!   request and never answers; `stream_*` cancel a streaming fake mid-response.
//!
//! Budgets are the medians measured on 2026-09-26 (Apple silicon, release profile)
//! with headroom of about 2.5x, so machine noise passes and a real regression does
//! not. The on-disk ledger figures are dominated by SQLite's synced writes and
//! vary run to run by about 2x; the in-memory figures are the loop's own cost. Run: `cargo bench -p kgraf-runtime --bench worker_loop`.

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::{Capabilities, StreamEvent, Streaming};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderConfig, ProviderError, Request, Response, Role,
    Settlement, StopReason,
};
use kgraf_provider_openai::{ChatCompletionsAdapter, UsageDialect};
use kgraf_runtime::{
    CallMode, ContextConfig, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot, SharedPrefix,
    SpawnOptions, WorkerBrief, WorkerEnv, WorkerEvent, WorkerOutcome, spawn_worker,
};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// The silent server and the request-or-end wait, shared with tests/http_cancel.rs.
#[path = "../tests/support/silent_http.rs"]
mod silent_http;
use silent_http::{Arrival, request_or_end, silent_server};

const ROUNDS: usize = 20;
const MODEL: &str = "bench-model";

/// Budgets: benchmark id to the median it must not exceed.
const BUDGETS: &[(&str, Duration)] = &[
    (
        "turn_overhead/stream_disk_ledger",
        Duration::from_micros(1_600),
    ),
    (
        "turn_overhead/complete_disk_ledger",
        Duration::from_micros(1_600),
    ),
    (
        "turn_overhead/stream_memory_ledger",
        Duration::from_micros(100),
    ),
    (
        "spawn_teardown/spawn_then_cancel",
        Duration::from_micros(40),
    ),
    ("spawn_teardown/one_round", Duration::from_micros(1_600)),
    (
        "cancel_to_release/http_in_flight",
        Duration::from_micros(1_500),
    ),
    (
        "cancel_to_release/stream_disk_ledger",
        Duration::from_micros(1_000),
    ),
    (
        "cancel_to_release/stream_memory_ledger",
        Duration::from_micros(60),
    ),
];

/// Per-iteration times each benchmark measured, for the budget check.
#[derive(Default)]
struct Samples(Mutex<BTreeMap<&'static str, Vec<Duration>>>);

impl Samples {
    fn record(&self, id: &'static str, per_iter: Duration) {
        self.0
            .lock()
            .expect("samples")
            .entry(id)
            .or_default()
            .push(per_iter);
    }

    fn assert_budgets(&self) {
        let samples = self.0.lock().expect("samples");
        let mut failures = Vec::new();
        for (id, budget) in BUDGETS {
            let Some(times) = samples.get(id) else {
                failures.push(format!("{id}: not measured"));
                continue;
            };
            let mut sorted = times.clone();
            sorted.sort();
            let median = sorted[sorted.len() / 2];
            println!("budget {id}: median {median:?} of {budget:?}");
            if median > *budget {
                failures.push(format!("{id}: median {median:?} exceeds budget {budget:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "budgets missed:\n{}",
            failures.join("\n")
        );
    }
}

#[derive(Clone, Copy)]
enum Behavior {
    /// `ROUNDS` tool rounds, then an answer.
    ToolRounds,
    /// Answer at once.
    Answer,
    /// Stream one delta, then never finish.
    Hang,
}

struct Instant0 {
    behavior: Behavior,
}

fn usage() -> TokenUsage {
    TokenUsage {
        uncached_input: 1_200,
        cache_read: 38_000,
        text_output: 40,
        ..TokenUsage::default()
    }
}

impl Instant0 {
    fn response(&self, request: &Request) -> Response {
        let round = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let tool_round = matches!(self.behavior, Behavior::ToolRounds) && round < ROUNDS;
        let mut content = vec![Content::text("ok")];
        if tool_round {
            content.push(Content::ToolUse {
                id: format!("t{round}"),
                name: "noop".into(),
                input: serde_json::json!({ "round": round }),
            });
        }
        Response {
            model: request.model.clone(),
            content,
            stop_reason: if tool_round {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage: usage(),
            request_id: None,
        }
    }
}

#[async_trait::async_trait]
impl Provider for Instant0 {
    fn adapter(&self) -> &'static str {
        "bench"
    }
    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let response = self.response(request);
        token.settle(&response.usage, None)?;
        Ok(response)
    }
    fn capabilities(&self) -> Capabilities<'_> {
        Capabilities::none().with_streaming(self)
    }
}

#[async_trait::async_trait]
impl Streaming for Instant0 {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        let response = self.response(request);
        sink(StreamEvent::Usage {
            usage: response.usage.clone(),
        });
        sink(StreamEvent::Delta {
            content: Content::text("ok"),
        });
        if matches!(self.behavior, Behavior::Hang) {
            std::future::pending::<()>().await;
        }
        sink(StreamEvent::Finished {
            stop_reason: response.stop_reason,
        });
        token.settle(&response.usage, None)?;
        Ok(response)
    }
}

struct Noop;

#[async_trait::async_trait]
impl Tool for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "noop".into(),
            description: "Does nothing".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("ok"))
    }
}

fn env(
    provider: Arc<dyn Provider>,
    mode: CallMode,
    ledger: Arc<Ledger>,
    vendor: &str,
) -> Arc<WorkerEnv> {
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(Noop), Exposure::Prefix)
        .expect("register");
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: vendor.into(),
            model: MODEL.into(),
            // A realistic system prompt size, so the per-turn clone and check of the
            // prefix are measured at a size that matters.
            system: vec![Content::text("You are a kGraf agent. ".repeat(400))],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("prefix");
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = mode;
    Arc::new(env)
}

fn instant(behavior: Behavior) -> Arc<dyn Provider> {
    Arc::new(Instant0 { behavior })
}

fn brief(budget: u32) -> WorkerBrief {
    WorkerBrief {
        task: "bench-task-1".into(),
        worker: "bench-worker-1".into(),
        instruction: "Do the thing.".into(),
        workspace_root: std::env::temp_dir(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: budget,
        context: ContextConfig::for_model(1_000_000).expect("config"),
        write_scope: None,
    }
}

async fn run_to_end(env: Arc<WorkerEnv>, budget: u32) -> WorkerOutcome {
    let mut handle = spawn_worker(env, brief(budget), SpawnOptions::default())
        .await
        .expect("spawn");
    while handle.next_event().await.is_some() {}
    handle.result().await.expect("result").outcome
}

fn bench(c: &mut Criterion) {
    let samples = Samples::default();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("dir");
    let disk = Arc::new(Ledger::open(&dir.path().join("ledger.sqlite")).expect("disk ledger"));
    let memory = Arc::new(Ledger::open_in_memory().expect("memory ledger"));

    let mut turns = c.benchmark_group("turn_overhead");
    for (name, id, mode, ledger) in [
        (
            "stream_disk_ledger",
            "turn_overhead/stream_disk_ledger",
            CallMode::Stream,
            &disk,
        ),
        (
            "complete_disk_ledger",
            "turn_overhead/complete_disk_ledger",
            CallMode::Complete,
            &disk,
        ),
        (
            "stream_memory_ledger",
            "turn_overhead/stream_memory_ledger",
            CallMode::Stream,
            &memory,
        ),
    ] {
        let env = env(instant(Behavior::ToolRounds), mode, ledger.clone(), "bench");
        turns.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let outcome = rt.block_on(run_to_end(env.clone(), ROUNDS as u32 + 5));
                    total += start.elapsed();
                    assert_eq!(outcome, WorkerOutcome::Completed);
                }
                // Per round: ROUNDS tool rounds plus the answer round.
                let per_round = total / (ROUNDS as u32 + 1);
                samples.record(id, per_round / iters.max(1) as u32);
                per_round
            });
        });
    }
    turns.finish();

    let mut spawn = c.benchmark_group("spawn_teardown");
    let answer = env(
        instant(Behavior::Answer),
        CallMode::Stream,
        disk.clone(),
        "bench",
    );
    // A provider that never answers, so the cancel always lands before the run can
    // finish on its own (between rounds or in the stream).
    let never = env(
        instant(Behavior::Hang),
        CallMode::Stream,
        memory.clone(),
        "bench",
    );
    spawn.bench_function("spawn_then_cancel", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                rt.block_on(async {
                    let mut handle = spawn_worker(never.clone(), brief(4), SpawnOptions::default())
                        .await
                        .expect("spawn");
                    handle.cancel();
                    while handle.next_event().await.is_some() {}
                    let result = handle.result().await.expect("result");
                    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
                });
            }
            let total = start.elapsed();
            samples.record(
                "spawn_teardown/spawn_then_cancel",
                total / iters.max(1) as u32,
            );
            total
        });
    });
    spawn.bench_function("one_round", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let outcome = rt.block_on(run_to_end(answer.clone(), 4));
                assert_eq!(outcome, WorkerOutcome::Completed);
            }
            let total = start.elapsed();
            samples.record("spawn_teardown/one_round", total / iters.max(1) as u32);
            total
        });
    });
    spawn.finish();

    let mut cancel = c.benchmark_group("cancel_to_release");

    // A real HTTP request in flight: the Chat Completions adapter against a local
    // server that has read the whole request and will never answer.
    let (url, announcements) = rt.block_on(silent_server());
    let announcements = Arc::new(tokio::sync::Mutex::new(announcements));
    let chat: Arc<dyn Provider> = Arc::new(
        ChatCompletionsAdapter::new(
            ProviderConfig::local(url, vec![MODEL.into()], Protocol::Inclusive),
            UsageDialect::Details,
        )
        .expect("adapter"),
    );
    let http_ledger = Arc::new(Ledger::open(&dir.path().join("http.sqlite")).expect("ledger"));
    let http = env(chat, CallMode::Complete, http_ledger.clone(), "bench-http");
    cancel.bench_function("http_in_flight", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += rt.block_on(async {
                    let mut handle = spawn_worker(http.clone(), brief(4), SpawnOptions::default())
                        .await
                        .expect("spawn");
                    // Fail loud if the worker ends before its request lands (for
                    // example a refused connect); a bare wait on the announcement
                    // would park the bench forever.
                    let arrival =
                        request_or_end(&mut handle, &mut *announcements.lock().await).await;
                    if arrival == Arrival::WorkerEnded {
                        panic!(
                            "the worker ended before its request reached the server: {:?}",
                            handle.result().await
                        );
                    }
                    let start = Instant::now();
                    handle.cancel();
                    while handle.next_event().await.is_some() {}
                    let result = handle.result().await.expect("result");
                    let elapsed = start.elapsed();
                    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
                    elapsed
                });
            }
            samples.record(
                "cancel_to_release/http_in_flight",
                total / iters.max(1) as u32,
            );
            total
        });
    });
    // Every cancelled HTTP call reached the server and delivered no usage, so each
    // is left unreconciled rather than counted as free (every token is attributed).
    let rows = http_ledger.calls().expect("rows");
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|r| r.settlement == Settlement::Unreconciled)
    );

    for (name, id, ledger) in [
        (
            "stream_disk_ledger",
            "cancel_to_release/stream_disk_ledger",
            &disk,
        ),
        (
            "stream_memory_ledger",
            "cancel_to_release/stream_memory_ledger",
            &memory,
        ),
    ] {
        let hang = env(
            instant(Behavior::Hang),
            CallMode::Stream,
            ledger.clone(),
            "bench",
        );
        cancel.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    total += rt.block_on(async {
                        let mut handle =
                            spawn_worker(hang.clone(), brief(4), SpawnOptions::default())
                                .await
                                .expect("spawn");
                        while let Some(event) = handle.next_event().await {
                            if matches!(event, WorkerEvent::TextDelta { .. }) {
                                break;
                            }
                        }
                        let start = Instant::now();
                        handle.cancel();
                        while handle.next_event().await.is_some() {}
                        let result = handle.result().await.expect("result");
                        let elapsed = start.elapsed();
                        assert_eq!(result.outcome, WorkerOutcome::Cancelled);
                        elapsed
                    });
                }
                samples.record(id, total / iters.max(1) as u32);
                total
            });
        });
    }
    cancel.finish();

    samples.assert_budgets();
}

criterion_group!(benches, bench);
criterion_main!(benches);
