//! Live check against **Anthropic itself** (`https://api.anthropic.com`),
//! the first time this adapter has run against its own reference vendor rather
//! than Kimi's Anthropic-compatible endpoint (`kimi-anthropic`, exercised by
//! `live_rate_limit_headers.rs` and `kgraf-runtime/tests/live_steer.rs`).
//!
//! Covers, each against a real response:
//! - a plain call and a streaming call, with usage parsed (uncached input,
//!   output, cache read, cache write) under the additive dialect and priced
//!   (not "unpriced"),
//! - a tool round trip (`tool_use` answered with `tool_result`),
//! - prompt caching: two calls sharing one long cached system prefix, where
//!   the second reports a nonzero `cache_read`,
//! - that no `thinking` field is ever sent (decision 0067: extended
//!   thinking stays off) -- structurally guaranteed by `AnthropicAdapter::to_wire`,
//!   which has no code path that emits one; confirmed live by the response
//!   carrying no `Content::Reasoning` block.
//!
//! The steer-after-tool-results case (decision 0067) is covered
//! live by `kgraf-runtime/tests/live_steer.rs`'s `exchange("anthropic", ...)`,
//! not here: that harness owns the worker/injection machinery this crate does
//! not have.
//!
//! Model: `claude-haiku-4-5-20251001`, the cheapest current Anthropic model in
//! `third_party/litellm/model_prices_and_context_window.json` at $1/$5 per
//! million input/output tokens (checked 2026-09-28; Anthropic's opus and sonnet
//! lines cost 2x-5x more per input token and don't change what this file needs
//! to show).
//!
//! Ignored by default: it spends money (checked ledgered cost, a few cents at
//! most). Keys come **only** from a `KEY=VALUE` file named by
//! `KGRAF_LIVE_ENV_FILE` (the project keeps them in the repository root's
//! `.env`); the process environment is not read for keys.
//!
//! ```text
//! KGRAF_LIVE_ANTHROPIC_NATIVE=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-provider-anthropic --test live_anthropic_native -- --ignored --nocapture
//! ```

use kgraf_core::{ModelPrice, Protocol};
use kgraf_provider::{Content, Ledger, Message, Provider, ProviderConfig, Request, Role};
use kgraf_provider_anthropic::AnthropicAdapter;

const MODEL: &str = "claude-haiku-4-5-20251001";
const BASE_URL: &str = "https://api.anthropic.com/v1";

/// Micro-dollars per million tokens, from `third_party/litellm`'s
/// `claude-haiku-4-5-20251001` entry (checked 2026-09-28): $1 input, $0.10
/// cache read, $1.25 cache write (5-minute TTL), $5 output.
fn price() -> ModelPrice {
    ModelPrice {
        uncached_input: 1_000_000,
        cache_read: 100_000,
        cache_write: 1_250_000,
        output: 5_000_000,
        reasoning: 5_000_000,
    }
}

fn cost_micros(usage: &kgraf_core::TokenUsage) -> i64 {
    let p = price();
    usage.uncached_input as i64 * p.uncached_input / 1_000_000
        + usage.cache_read as i64 * p.cache_read / 1_000_000
        + usage.cache_write as i64 * p.cache_write / 1_000_000
        + usage.total_output() as i64 * p.output / 1_000_000
}

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names, if it names one.
/// Read into memory only: nothing is set in the process environment, and no
/// value is ever printed (mirrors `live_rate_limit_headers.rs` and
/// `kgraf-runtime/tests/live_steer.rs`).
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

/// Resolves `name` (a `KGRAF_*` variable) through the `.env` file only, falling
/// back to its legacy `HAUSTORIA_*` spelling, exactly as `kgraf-fleet` does.
fn credential(name: &str) -> Option<String> {
    let file = env_file();
    kgraf_core::legacy::resolve_env(name, |n| {
        std::env::var(n).ok().or_else(|| file.get(n).cloned())
    })
    .value
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_ANTHROPIC_NATIVE").is_ok_and(|v| v == "1")
}

fn adapter(key: &str, honoring_cache: bool) -> AnthropicAdapter {
    let mut config = ProviderConfig::new(
        "anthropic",
        BASE_URL,
        key,
        vec![MODEL.to_owned()],
        Protocol::Additive,
    )
    .expect("config");
    if honoring_cache {
        config = config.honoring_cache_control();
    }
    AnthropicAdapter::new(config).expect("adapter")
}

/// A long, stable block of filler text: long enough to clear Anthropic's
/// minimum cacheable-prefix length (1024 tokens for Haiku-line models), so a
/// warm second call actually has something to read back from the cache. Real
/// prose rather than a repeated token, since some providers refuse or dedupe
/// degenerate repetition oddly.
fn long_cacheable_prefix() -> String {
    let paragraph = "The kGraf harness treats every provider call as an accounted, priced \
        event: usage is normalized into six independent fields (uncached input, cache read, \
        cache write, text output, reasoning output, and the provider's raw usage blob) so a \
        pricing change never requires re-deriving cost from a lossy total. Prompt caching is \
        one adapter capability among several, opted into per endpoint rather than assumed from \
        a vendor name, because vendors disagree on both whether caching exists at all and on \
        how a cached prefix's cost is reported back to the caller. ";
    paragraph.repeat(40)
}

#[tokio::test]
#[ignore = "live: spends money; see the module docs"]
async fn live_anthropic_plain_and_streaming_calls_parse_and_price_usage() {
    if !live() {
        println!("KGRAF_LIVE_ANTHROPIC_NATIVE is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_ANTHROPIC_API_KEY") else {
        println!("[anthropic] SKIPPED: KGRAF_ANTHROPIC_API_KEY is not set");
        return;
    };
    let adapter = adapter(&key, false);

    // Plain call.
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            "anthropic",
            Protocol::Additive,
            MODEL,
            "live-plain",
            "sr-011",
        )
        .expect("begin");
    let mut request = Request::new(MODEL, "placeholder", 32);
    request.messages = vec![Message {
        role: Role::User,
        content: vec![Content::text("Reply with exactly one word: OK")],
    }];
    let response = adapter
        .complete(&request, &token)
        .await
        .expect("a cheap, short call must succeed against api.anthropic.com");
    println!("[anthropic plain] model echoed: {}", response.model);
    println!("[anthropic plain] answer: {}", response.text().trim());
    assert_eq!(
        response.model, MODEL,
        "the model must be echoed, not silently remapped"
    );
    assert!(!response.text().trim().is_empty());
    let rows = ledger.calls().expect("rows");
    assert_eq!(rows.len(), 1);
    let usage = &rows[0].usage;
    println!(
        "[anthropic plain] usage: uncached_input={} cache_read={} cache_write={} \
         text_output={} reasoning_output={}",
        usage.uncached_input,
        usage.cache_read,
        usage.cache_write,
        usage.text_output,
        usage.reasoning_output
    );
    assert!(
        usage.uncached_input > 0,
        "input tokens must parse as nonzero"
    );
    assert!(
        usage.total_output() > 0,
        "output tokens must parse as nonzero"
    );
    assert_eq!(
        usage.reasoning_output, 0,
        "extended thinking was never requested (decision 0067), so no \
         reasoning tokens should be billed"
    );
    assert!(
        !response
            .content
            .iter()
            .any(|c| matches!(c, Content::Reasoning { .. })),
        "no thinking block should be present: this adapter never sends `thinking`"
    );
    let plain_cost = cost_micros(usage);
    assert!(plain_cost > 0, "the call must be priced, not left unpriced");
    println!("[anthropic plain] cost ${:.6}", plain_cost as f64 / 1e6);

    // Streaming call.
    let token = ledger
        .begin(
            "anthropic",
            Protocol::Additive,
            MODEL,
            "live-stream",
            "sr-011",
        )
        .expect("begin");
    let mut request = Request::new(MODEL, "placeholder", 32);
    request.messages = vec![Message {
        role: Role::User,
        content: vec![Content::text(
            "Count from one to three, one number per line.",
        )],
    }];
    let caps = adapter.capabilities();
    let streaming = caps
        .streaming
        .expect("the anthropic adapter declares streaming");
    let mut deltas = 0usize;
    let mut sink = |event: kgraf_provider::StreamEvent| {
        if matches!(event, kgraf_provider::StreamEvent::Delta { .. }) {
            deltas += 1;
        }
    };
    let response = streaming
        .stream(&request, &token, &mut sink)
        .await
        .expect("a streamed call must succeed against api.anthropic.com");
    println!("[anthropic stream] deltas observed: {deltas}");
    println!("[anthropic stream] answer: {}", response.text().trim());
    assert!(
        deltas > 0,
        "a streamed answer must deliver at least one delta"
    );
    assert!(!response.text().trim().is_empty());
    let rows = ledger.calls().expect("rows");
    let stream_row = rows.iter().find(|r| r.id != rows[0].id).unwrap_or(&rows[0]);
    let usage = &stream_row.usage;
    println!(
        "[anthropic stream] usage: uncached_input={} cache_read={} cache_write={} \
         text_output={} reasoning_output={}",
        usage.uncached_input,
        usage.cache_read,
        usage.cache_write,
        usage.text_output,
        usage.reasoning_output
    );
    assert!(usage.uncached_input > 0);
    assert!(usage.total_output() > 0);
    let stream_cost = cost_micros(usage);
    assert!(stream_cost > 0, "the streamed call must be priced too");
    println!("[anthropic stream] cost ${:.6}", stream_cost as f64 / 1e6);
    println!(
        "[anthropic] total for both calls: ${:.6}",
        (plain_cost + stream_cost) as f64 / 1e6
    );
}

#[tokio::test]
#[ignore = "live: spends money; see the module docs"]
async fn live_anthropic_tool_round_trip_is_accepted() {
    if !live() {
        println!("KGRAF_LIVE_ANTHROPIC_NATIVE is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_ANTHROPIC_API_KEY") else {
        println!("[anthropic] SKIPPED: KGRAF_ANTHROPIC_API_KEY is not set");
        return;
    };
    let adapter = adapter(&key, false);
    let ledger = Ledger::open_in_memory().expect("ledger");

    // First turn: the model must call the tool to answer.
    let mut request = Request::new(MODEL, "placeholder", 200);
    request.tools = vec![kgraf_provider::message::ToolDefinition {
        name: "lookup".into(),
        description: "Look up the value of a named symbol.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"symbol": {"type": "string"}},
            "required": ["symbol"]
        }),
    }];
    request.messages = vec![Message {
        role: Role::User,
        content: vec![Content::text(
            "Call the lookup tool with symbol \"alpha\", then state its value in one short \
             sentence.",
        )],
    }];
    let token = ledger
        .begin(
            "anthropic",
            Protocol::Additive,
            MODEL,
            "live-tool-1",
            "sr-011",
        )
        .expect("begin");
    let first = adapter
        .complete(&request, &token)
        .await
        .expect("the first turn must succeed");
    println!("[anthropic tool] stop_reason: {:?}", first.stop_reason);
    let tool_uses: Vec<(String, String, serde_json::Value)> = first
        .tool_uses()
        .into_iter()
        .map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.clone()))
        .collect();
    assert!(
        !tool_uses.is_empty(),
        "the model must call the lookup tool: content was {:?}",
        first.content
    );
    let (tool_id, tool_name, tool_input) = &tool_uses[0];
    println!("[anthropic tool] called {tool_name}({tool_input}) id={tool_id}");

    // Second turn: answer the tool call, in the wire shape this project's own
    // unit test (`steer_user_turn.rs`) asserts Anthropic requires -- the
    // assistant's tool_use block echoed back, then a user turn carrying the
    // tool_result.
    request.messages.push(Message {
        role: Role::Assistant,
        content: first.content.clone(),
    });
    request.messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: tool_id.clone(),
            content: "The value of alpha is 42.".into(),
            is_error: false,
        }],
    });
    let token = ledger
        .begin(
            "anthropic",
            Protocol::Additive,
            MODEL,
            "live-tool-2",
            "sr-011",
        )
        .expect("begin");
    let second = adapter
        .complete(&request, &token)
        .await
        .expect("the second turn (tool result) must be accepted");
    println!("[anthropic tool] final answer: {}", second.text().trim());
    assert!(
        second.text().to_lowercase().contains("42"),
        "the answer should reflect the tool result: {}",
        second.text()
    );

    let rows = ledger.calls().expect("rows");
    let total: i64 = rows.iter().map(|r| cost_micros(&r.usage)).sum();
    println!("[anthropic tool] total ${:.6}", total as f64 / 1e6);
    assert!(total > 0);
}

#[tokio::test]
#[ignore = "live: spends money; see the module docs"]
async fn live_anthropic_prompt_cache_is_read_on_the_second_call() {
    if !live() {
        println!("KGRAF_LIVE_ANTHROPIC_NATIVE is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_ANTHROPIC_API_KEY") else {
        println!("[anthropic] SKIPPED: KGRAF_ANTHROPIC_API_KEY is not set");
        return;
    };
    let adapter = adapter(&key, true);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let prefix = long_cacheable_prefix();

    let make_request = |tail: &str| {
        let mut request = Request::new(MODEL, "placeholder", 16);
        request.system = vec![Content::cached_text(prefix.clone())];
        request.messages = vec![Message {
            role: Role::User,
            content: vec![Content::text(tail)],
        }];
        request
    };

    let token = ledger
        .begin(
            "anthropic",
            Protocol::Additive,
            MODEL,
            "live-cache-cold",
            "sr-011",
        )
        .expect("begin");
    let cold = adapter
        .complete(&make_request("Reply with exactly one word: FIRST"), &token)
        .await
        .expect("the cold (cache-write) call must succeed");
    println!("[anthropic cache cold] answer: {}", cold.text().trim());

    let rows = ledger.calls().expect("rows");
    let cold_usage = rows[0].usage.clone();
    println!(
        "[anthropic cache cold] uncached_input={} cache_read={} cache_write={}",
        cold_usage.uncached_input, cold_usage.cache_read, cold_usage.cache_write
    );
    assert!(
        cold_usage.cache_write > 0,
        "the first call over a fresh long prefix should write the cache \
         (uncached_input={}, cache_write={}); if this is 0, the prefix did not \
         clear Anthropic's minimum cacheable length",
        cold_usage.uncached_input,
        cold_usage.cache_write
    );

    let token = ledger
        .begin(
            "anthropic",
            Protocol::Additive,
            MODEL,
            "live-cache-warm",
            "sr-011",
        )
        .expect("begin");
    let warm = adapter
        .complete(&make_request("Reply with exactly one word: SECOND"), &token)
        .await
        .expect("the warm (cache-read) call must succeed");
    println!("[anthropic cache warm] answer: {}", warm.text().trim());

    let rows = ledger.calls().expect("rows");
    let warm_usage = rows[1].usage.clone();
    println!(
        "[anthropic cache warm] uncached_input={} cache_read={} cache_write={}",
        warm_usage.uncached_input, warm_usage.cache_read, warm_usage.cache_write
    );
    assert!(
        warm_usage.cache_read > 0,
        "the second call over the same prefix should read it back from cache \
         (cache_read={})",
        warm_usage.cache_read
    );

    let total: i64 = cost_micros(&cold_usage) + cost_micros(&warm_usage);
    println!("[anthropic cache] total ${:.6}", total as f64 / 1e6);
    assert!(total > 0);
}
