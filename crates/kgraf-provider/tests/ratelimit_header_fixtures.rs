//! Rate-limit header parsing, checked against the recorded live headers
//! (decision 0067): the fixtures under `tests/fixtures` are the recorded response headers a real
//! call to each vendor actually returned on 2026-09-28, with account-identifying
//! headers removed. This test is offline and always runs: it is not the live
//! check itself (that is each adapter crate's own `#[ignore]`d
//! `live_rate_limit_headers.rs`), only a permanent regression test that the
//! parser still reads what a real vendor was once seen to send.

use kgraf_provider::{ObservedLimits, parse_anthropic_style, parse_openai_style};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    serde_json::from_str(&text).expect("valid json")
}

#[test]
fn the_openai_responses_fixture_parses_exactly_as_recorded() {
    let v = fixture("2026-09-28-openai-responses-ratelimit-headers.json");
    let headers = v["headers"].as_object().expect("headers object").clone();
    let observed = parse_openai_style(&move |name: &str| {
        headers.get(name).and_then(Value::as_str).map(str::to_owned)
    });
    let expected = &v["parsed_by_kgraf_provider_limits_parse_openai_style"];
    assert_eq!(
        observed.requests_limit,
        expected["requests_limit"].as_u64().map(|n| n as u32)
    );
    assert_eq!(observed.tokens_limit, expected["tokens_limit"].as_u64());
    assert_eq!(
        observed.requests_remaining,
        expected["requests_remaining"].as_u64().map(|n| n as u32)
    );
    assert_eq!(observed.reset_ms, expected["reset_ms"].as_i64());
}

#[test]
fn kimis_chat_completions_fixture_confirms_no_openai_style_headers_are_sent() {
    let v = fixture("2026-09-28-kimi-chat-headers-no-ratelimit.json");
    assert_eq!(
        v["openai_style_ratelimit_headers_present"],
        Value::Bool(false)
    );
    // The recorded header *names* (not the ones this fixture's `note` already
    // excluded) contain none of the four this parser looks for.
    let seen: Vec<String> = v["headers_seen"]
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h.as_str().expect("string").to_owned())
        .collect();
    for name in [
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-reset-requests",
    ] {
        assert!(
            !seen.contains(&name.to_owned()),
            "{name} unexpectedly present"
        );
    }
    let observed = parse_openai_style(&|_: &str| None::<String>);
    assert_eq!(observed, ObservedLimits::default());
}

#[test]
fn the_kimi_anthropic_fixture_confirms_no_anthropic_style_headers_are_sent() {
    let v = fixture("2026-09-28-kimi-anthropic-headers-no-ratelimit.json");
    assert_eq!(
        v["anthropic_style_ratelimit_headers_present"],
        Value::Bool(false)
    );
    let seen: Vec<String> = v["headers_seen"]
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h.as_str().expect("string").to_owned())
        .collect();
    for name in [
        "anthropic-ratelimit-requests-limit",
        "anthropic-ratelimit-tokens-limit",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-requests-reset",
    ] {
        assert!(
            !seen.contains(&name.to_owned()),
            "{name} unexpectedly present"
        );
    }
    let observed = parse_anthropic_style(&|_: &str| None::<String>);
    assert_eq!(observed, ObservedLimits::default());
}

#[test]
fn absent_headers_always_parse_to_the_all_none_default() {
    assert_eq!(
        parse_openai_style(&|_: &str| None::<String>),
        ObservedLimits::default()
    );
    assert_eq!(
        parse_anthropic_style(&|_: &str| None::<String>),
        ObservedLimits::default()
    );
}
