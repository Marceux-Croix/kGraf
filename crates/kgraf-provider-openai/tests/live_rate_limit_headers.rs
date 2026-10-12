//! Live check for rate-limit header capture (decision 0067): one
//! cheap call against a real speaker, then reading
//! `kgraf_provider::limits::get` (the production path
//! `responses.rs`'s own `observe_rate_limit_headers` writes to) to prove the
//! OpenAI-style parser recognizes the headers a real vendor actually sends.
//!
//! **Kimi/Moonshot's own endpoint does not send OpenAI-style
//! `x-ratelimit-*` headers at all** (checked live 2026-09-28 with a bare
//! `curl`; the recorded headers are in
//! `kgraf-provider`'s `tests/fixtures`): it sends its own `msh-*` vendor headers instead, none of them a request or
//! token limit). This test therefore checks the real OpenAI endpoint, the
//! vendor the OpenAI-style parser is actually named for, using
//! `KGRAF_OPENAI_API_KEY`.
//!
//! Ignored by default: it spends money (a handful of tokens on
//! `gpt-6-luna`, a small fraction of a cent). Keys come from the
//! environment, or from a `KEY=VALUE` file named by `KGRAF_LIVE_ENV_FILE`
//! (same pattern as `live_two_text_blocks.rs` in this crate). Nothing from
//! that file is ever printed, and it is read only inside this process.
//!
//! ```text
//! KGRAF_LIVE_RATE_HEADERS=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-provider-openai --test live_rate_limit_headers -- --ignored --nocapture
//! ```
//!
//! A route whose key is absent is skipped and says so; nothing is faked.
//! Only header **names** are ever printed (never values, which could
//! identify the account by its remaining-quota numbers).

use kgraf_core::Protocol;
use kgraf_provider::{Content, Ledger, Message, Provider, ProviderConfig, Request, Role};
use kgraf_provider_openai::OpenAiAdapter;

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

fn credential(name: &str) -> Option<String> {
    let file = env_file();
    kgraf_core::legacy::resolve_env(name, |n| {
        std::env::var(n).ok().or_else(|| file.get(n).cloned())
    })
    .value
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_RATE_HEADERS").is_ok_and(|v| v == "1")
}

#[tokio::test]
#[ignore = "live: spends money; see the module docs"]
async fn openai_responses_sends_openai_style_rate_limit_headers() {
    if !live() {
        println!("KGRAF_LIVE_RATE_HEADERS is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_OPENAI_API_KEY") else {
        println!("[openai-responses] SKIPPED: KGRAF_OPENAI_API_KEY is not set");
        return;
    };
    let model = "gpt-6-luna";
    let base_url = "https://api.openai.com/v1";
    let config = ProviderConfig::new(
        "openai-responses",
        base_url,
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
            "live-rate-headers",
            "live-check",
        )
        .expect("begin");
    let mut request = Request::new(model, "placeholder", 16);
    request.messages = vec![Message {
        role: Role::User,
        content: vec![Content::text("Reply with just: OK")],
    }];
    let response = adapter
        .complete(&request, &token)
        .await
        .expect("a cheap, short call must succeed");
    println!("[openai-responses] answer: {}", response.text().trim());

    let observed = kgraf_provider::limits::get(base_url)
        .expect("the adapter's own header capture recorded something");
    println!(
        "[openai-responses] headers seen: requests_limit={} tokens_limit={} \
         requests_remaining={} reset_ms={}",
        observed.requests_limit.is_some(),
        observed.tokens_limit.is_some(),
        observed.requests_remaining.is_some(),
        observed.reset_ms.is_some(),
    );
    assert!(
        observed.requests_limit.is_some()
            && observed.tokens_limit.is_some()
            && observed.requests_remaining.is_some(),
        "expected the full set of OpenAI-style rate-limit headers from a real Responses \
         call, saw: {observed:?}"
    );

    let rows = ledger.calls().expect("rows");
    let uncached_input: i64 = rows.iter().map(|r| r.usage.uncached_input as i64).sum();
    let output: i64 = rows.iter().map(|r| r.usage.total_output() as i64).sum();
    // gpt-6-luna: 100_000 uncached-input, 500_000 output micro-dollars per
    // million tokens (same prices `live_two_text_blocks.rs` uses).
    let cost_micros = uncached_input * 100_000 / 1_000_000 + output * 500_000 / 1_000_000;
    println!("[openai-responses] cost ${:.6}", cost_micros as f64 / 1e6);
}
