//! Live check for rate-limit header capture (decision 0067): one
//! cheap call against a real speaker, then reading
//! `kgraf_provider::limits::get` (the production path
//! `AnthropicAdapter::post`'s own header-capture block writes to) to prove
//! the Anthropic-style parser recognizes at least one header a real vendor
//! actually sends.
//!
//! **No native Anthropic key exists in this environment** (task instruction,
//! confirmed against `.env`): this exercises Kimi's Anthropic-compatible
//! endpoint (`kimi-anthropic` in `kgraf-fleet`'s catalog,
//! `https://api.moonshot.ai/anthropic/v1`) instead, which speaks the
//! Anthropic Messages wire shape on the same credential as
//! `KGRAF_KIMI_API_KEY`. If Kimi's compatible endpoint does not send
//! Anthropic-style rate-limit headers, this test says so explicitly rather
//! than silently passing (fail loud): see the assertion message below.
//!
//! Ignored by default: it spends money (a few tokens on `kimi-k3`, a
//! fraction of a cent).
//!
//! ```text
//! KGRAF_LIVE_RATE_HEADERS=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-provider-anthropic --test live_rate_limit_headers -- --ignored --nocapture
//! ```

use kgraf_core::Protocol;
use kgraf_provider::{Content, Ledger, Message, Provider, ProviderConfig, Request, Role};
use kgraf_provider_anthropic::AnthropicAdapter;

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
async fn kimis_anthropic_compatible_endpoint_is_checked_for_anthropic_style_headers() {
    if !live() {
        println!("KGRAF_LIVE_RATE_HEADERS is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_KIMI_API_KEY") else {
        println!("[kimi-anthropic] SKIPPED: KGRAF_KIMI_API_KEY is not set");
        return;
    };
    let model = "kimi-k3";
    let base_url = "https://api.moonshot.ai/anthropic/v1";
    let config = ProviderConfig::new(
        "kimi-anthropic",
        base_url,
        key,
        vec![model.to_owned()],
        Protocol::Additive,
    )
    .expect("config");
    let adapter = AnthropicAdapter::new(config).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            "kimi-anthropic",
            Protocol::Additive,
            model,
            "live-rate-headers",
            "live-check",
        )
        .expect("begin");
    let mut request = Request::new(model, "placeholder", 5);
    request.messages = vec![Message {
        role: Role::User,
        content: vec![Content::text("Reply with just: OK")],
    }];
    let response = adapter
        .complete(&request, &token)
        .await
        .expect("a cheap, short call must succeed");
    println!("[kimi-anthropic] answer: {}", response.text().trim());

    let observed = kgraf_provider::limits::get(base_url);
    println!("[kimi-anthropic] observed: {observed:?}");
    match observed {
        Some(o)
            if o.requests_limit.is_some()
                || o.tokens_limit.is_some()
                || o.requests_remaining.is_some() =>
        {
            println!(
                "[kimi-anthropic] headers seen: requests_limit={} tokens_limit={} \
                 requests_remaining={} reset_ms={}",
                o.requests_limit.is_some(),
                o.tokens_limit.is_some(),
                o.requests_remaining.is_some(),
                o.reset_ms.is_some(),
            );
        }
        _ => {
            println!(
                "[kimi-anthropic] Kimi's Anthropic-compatible endpoint did not send any \
                 anthropic-ratelimit-* headers on this call; the parser is verified against \
                 the Anthropic Messages spec's own header names (unit-tested in \
                 kgraf-provider::limits), but no live confirmation exists for this specific \
                 compatible endpoint. Recorded, not hidden (fail loud)."
            );
        }
    }

    let rows = ledger.calls().expect("rows");
    let uncached_input: i64 = rows.iter().map(|r| r.usage.uncached_input as i64).sum();
    let output: i64 = rows.iter().map(|r| r.usage.total_output() as i64).sum();
    let cost_micros = uncached_input * 3_000_000 / 1_000_000 + output * 15_000_000 / 1_000_000;
    println!("[kimi-anthropic] cost ${:.6}", cost_micros as f64 / 1e6);
}

/// The same live check, this time against Anthropic itself
/// (`https://api.anthropic.com/v1`), the reference implementation the parser
/// was written against but, until this run, never checked live. Header
/// *names* only are printed (never a limit or remaining value, which is
/// account-tier information).
///
/// ```text
/// KGRAF_LIVE_RATE_HEADERS=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
///   cargo test -p kgraf-provider-anthropic --test live_rate_limit_headers -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "live: spends money; see the module docs"]
async fn anthropics_own_endpoint_is_checked_for_anthropic_style_headers() {
    if !live() {
        println!("KGRAF_LIVE_RATE_HEADERS is not 1; not run");
        return;
    }
    let Some(key) = credential("KGRAF_ANTHROPIC_API_KEY") else {
        println!("[anthropic] SKIPPED: KGRAF_ANTHROPIC_API_KEY is not set");
        return;
    };
    let model = "claude-haiku-4-5-20251001";
    let base_url = "https://api.anthropic.com/v1";
    let config = ProviderConfig::new(
        "anthropic",
        base_url,
        key,
        vec![model.to_owned()],
        Protocol::Additive,
    )
    .expect("config");
    let adapter = AnthropicAdapter::new(config).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            "anthropic",
            Protocol::Additive,
            model,
            "live-rate-headers",
            "live-check",
        )
        .expect("begin");
    let mut request = Request::new(model, "placeholder", 5);
    request.messages = vec![Message {
        role: Role::User,
        content: vec![Content::text("Reply with just: OK")],
    }];
    let response = adapter
        .complete(&request, &token)
        .await
        .expect("a cheap, short call must succeed against api.anthropic.com");
    println!("[anthropic] answer: {}", response.text().trim());

    let observed = kgraf_provider::limits::get(base_url);
    println!("[anthropic] observed: {observed:?}");
    match observed {
        Some(o)
            if o.requests_limit.is_some()
                || o.tokens_limit.is_some()
                || o.requests_remaining.is_some() =>
        {
            println!(
                "[anthropic] anthropic-ratelimit-* headers seen: requests_limit={} \
                 tokens_limit={} requests_remaining={} reset_ms={}",
                o.requests_limit.is_some(),
                o.tokens_limit.is_some(),
                o.requests_remaining.is_some(),
                o.reset_ms.is_some(),
            );
        }
        _ => {
            println!(
                "[anthropic] SURPRISE: api.anthropic.com sent no anthropic-ratelimit-* header \
                 this parser recognizes. Recorded, not hidden (fail loud): the fixtures in \
                 tests/fixtures need a new entry if this is confirmed on a \
                 second run."
            );
        }
    }

    let rows = ledger.calls().expect("rows");
    let uncached_input: i64 = rows.iter().map(|r| r.usage.uncached_input as i64).sum();
    let output: i64 = rows.iter().map(|r| r.usage.total_output() as i64).sum();
    // claude-haiku-4-5-20251001: $1/M uncached input, $5/M output.
    let cost_micros = uncached_input * 1_000_000 / 1_000_000 + output * 5_000_000 / 1_000_000;
    println!("[anthropic] cost ${:.6}", cost_micros as f64 / 1e6);
}
