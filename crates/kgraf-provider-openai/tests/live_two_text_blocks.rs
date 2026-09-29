//! Live check for the two changed protocols: a user message carrying two
//! separate `Content::Text` blocks must be accepted by a real speaker, after the fix
//! that stopped `chat.rs` and `responses.rs` from flattening adjacent text blocks
//! into one run-on string with no separator.
//!
//! Ignored by default: it spends money. Keys come from the environment, or from a
//! `KEY=VALUE` file named by `KGRAF_LIVE_ENV_FILE` (the project keeps them in the
//! repository root's `.env`). Same pattern as `kgraf-runtime`'s `live_steer.rs`.
//! Nothing from that file is ever printed, and it is read only inside this process.
//!
//! ```text
//! KGRAF_LIVE_TWO_BLOCKS=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-provider-openai --test live_two_text_blocks -- --ignored --nocapture
//! ```
//!
//! A route whose key is absent is skipped and says so; nothing is faked.

use kgraf_core::Protocol;
use kgraf_provider::{Content, Ledger, Message, Provider, ProviderConfig, Request, Role};
use kgraf_provider_openai::{ChatCompletionsAdapter, OpenAiAdapter, UsageDialect};

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names, if it names one. Read
/// into memory only: nothing is set in the process environment, and no value is ever
/// printed. There is no default path.
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

/// Resolves `name` (a `KGRAF_*` variable), falling back to its legacy `HAUSTORIA_*`
/// spelling, exactly as `kgraf-fleet` and `live_steer.rs` do.
fn credential(name: &str) -> Option<String> {
    let file = env_file();
    kgraf_core::legacy::resolve_env(name, |n| {
        std::env::var(n).ok().or_else(|| file.get(n).cloned())
    })
    .value
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_TWO_BLOCKS").is_ok_and(|v| v == "1")
}

/// A user turn with two distinct text blocks, so the assertion is about the wire
/// shape the adapter builds, not about what one already-joined string happens to say.
fn two_block_request(model: &str, max_output_tokens: u32) -> Request {
    let mut request = Request::new(model, "placeholder", max_output_tokens);
    request.messages = vec![Message {
        role: Role::User,
        content: vec![
            Content::text("Reply with exactly one word, in capital letters."),
            Content::text("The word must be ACKNOWLEDGED."),
        ],
    }];
    request
}

/// Prints the priced cost of a ledger's rows, in the same shape `live_steer.rs` uses,
/// without pulling in `kgraf-runtime`'s pricing dependency for one number: the two
/// prices here are recorded routes' rates (micro-dollars per million tokens).
fn cost_micros(ledger: &Ledger, uncached_input: i64, output: i64) -> i64 {
    let rows = ledger.calls().expect("rows");
    rows.iter()
        .map(|row| {
            let usage = &row.usage;
            usage.uncached_input as i64 * uncached_input / 1_000_000
                + usage.total_output() as i64 * output / 1_000_000
        })
        .sum()
}

#[tokio::test]
#[ignore = "live: spends money; see the module docs"]
async fn live_openai_responses_accepts_a_user_message_with_two_text_blocks() {
    if !live() {
        println!("KGRAF_LIVE_TWO_BLOCKS is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_OPENAI_API_KEY") else {
        println!("[openai-responses] SKIPPED: KGRAF_OPENAI_API_KEY is not set");
        return;
    };
    let model = "gpt-6-luna";
    let config = ProviderConfig::new(
        "openai-responses",
        "https://api.openai.com/v1",
        key,
        vec![model.to_owned()],
        Protocol::Inclusive,
    )
    .expect("config");
    let adapter = OpenAiAdapter::new(config).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            "openai-responses",
            Protocol::Inclusive,
            model,
            "live-two-blocks",
            "sr-011",
        )
        .expect("begin");
    let response = adapter
        .complete(&two_block_request(model, 20), &token)
        .await
        .expect("the provider must accept two text blocks in one message");
    println!("[openai-responses] answer: {}", response.text().trim());
    // gpt-6-luna: 100_000 uncached-input, 500_000 output micro-dollars per million.
    let cost = cost_micros(&ledger, 100_000, 500_000);
    println!("[openai-responses] cost ${:.6}", cost as f64 / 1e6);
    assert!(!response.text().trim().is_empty());
}

#[tokio::test]
#[ignore = "live: spends money; see the module docs"]
async fn live_chat_completions_accepts_a_user_message_with_two_text_blocks() {
    if !live() {
        println!("KGRAF_LIVE_TWO_BLOCKS is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_KIMI_API_KEY") else {
        println!("[kimi-chat] SKIPPED: KGRAF_KIMI_API_KEY is not set");
        return;
    };
    let model = "kimi-k3";
    let config = ProviderConfig::new(
        "openai-chat-completions",
        "https://api.moonshot.ai/v1",
        key,
        vec![model.to_owned()],
        Protocol::Inclusive,
    )
    .expect("config");
    let adapter = ChatCompletionsAdapter::new(config, UsageDialect::Details).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            "openai-chat-completions",
            Protocol::Inclusive,
            model,
            "live-two-blocks",
            "sr-011",
        )
        .expect("begin");
    let response = adapter
        .complete(&two_block_request(model, 300), &token)
        .await
        .expect("the provider must accept two text blocks in one message");
    println!("[kimi-chat] answer: {}", response.text().trim());
    // kimi-k3: 3_000_000 uncached-input, 15_000_000 output micro-dollars per million.
    let cost = cost_micros(&ledger, 3_000_000, 15_000_000);
    println!("[kimi-chat] cost ${:.6}", cost as f64 / 1e6);
    assert!(!response.text().trim().is_empty());
}
