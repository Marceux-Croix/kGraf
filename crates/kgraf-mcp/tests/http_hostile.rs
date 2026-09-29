//! Streamable HTTP against a hostile or careless server (`common::fake`, a raw
//! HTTP/1.1 listener): replies `rmcp`'s own server never sends. Each test here
//! failed before the fix it guards.

mod common;

use common::{Options, Reply, call, config, fake, fixed_env, registered};
use kgraf_mcp::{
    HeaderFromEnv, InMemoryAttributionSink, MAX_GET_STREAM_REOPENS, MIN_GET_STREAM_INTERVAL,
    McpCallKind, McpCallOutcome, McpConnectError, McpServerConfig, McpTransport, REDACTED,
    RmcpCaller,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn bearer(var: &str) -> HeaderFromEnv {
    HeaderFromEnv::new(var).with_prefix("Bearer ")
}

async fn connect(
    url: &str,
    headers: &[(&str, HeaderFromEnv)],
    value: &'static str,
) -> (RmcpCaller, kgraf_mcp::McpServerConfig) {
    let config = config(url, headers);
    let caller = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &fixed_env(value),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await
    .expect("connect");
    (caller, config)
}

/// A JSON-escaped credential (`\/` for `/`) in SSE data decodes to the plain
/// credential. Scrubbing the raw text missed it; scrubbing decoded strings does not.
#[tokio::test]
async fn a_json_escaped_credential_in_an_sse_reply_is_scrubbed() {
    let server = fake(Options::reply(Reply::Sse(r"tok+secret\/xyz"))).await;
    let (caller, config) = connect(
        &server.url,
        &[("Authorization", bearer("T"))],
        "tok+secret/xyz",
    )
    .await;
    let registry = registered(caller, &config).await;
    let out = call(&registry, "mcp__remote__leak", json!({}))
        .await
        .expect("call")
        .render();
    assert!(!out.contains("tok+secret/xyz"), "{out}");
    assert!(out.contains(REDACTED), "{out}");
}

/// A credential holding `"` or `\` is escaped in JSON, so it never matched
/// the serialized text. It must match the decoded string.
#[tokio::test]
async fn a_credential_with_a_quote_and_a_backslash_is_scrubbed_from_a_json_reply() {
    let server = fake(Options::reply(Reply::Json(r#"pa\"ss\\word-99"#))).await;
    let (caller, config) = connect(
        &server.url,
        &[("Authorization", bearer("T"))],
        r#"pa"ss\word-99"#,
    )
    .await;
    let registry = registered(caller, &config).await;
    let out = call(&registry, "mcp__remote__leak", json!({}))
        .await
        .expect("call")
        .render();
    assert!(!out.contains(r#"pa"ss\word-99"#), "{out}");
    assert!(out.contains(REDACTED), "{out}");
}

/// Far above the 16 MiB cap, so the part the client never reads is larger than
/// any loopback socket buffer could absorb.
const OVER_CAP: usize = 64 * 1024 * 1024;

/// A JSON reply larger than the body cap is refused, with `Content-Length`
/// (refused before reading) and without it (refused while reading).
#[tokio::test]
async fn a_json_reply_over_the_body_cap_is_refused() {
    for reply in [Reply::BigJson(OVER_CAP), Reply::BigJsonUnsized(OVER_CAP)] {
        let server = fake(Options::reply(reply)).await;
        let (caller, config) = connect(&server.url, &[], "unused").await;
        let registry = registered(caller, &config).await;
        let error = call(&registry, "mcp__remote__leak", json!({}))
            .await
            .expect_err("an oversized reply must be refused");
        assert!(error.to_string().contains("byte limit"), "{error}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let sent = server.big_body_bytes_sent.load(Ordering::SeqCst);
        assert!(
            sent < OVER_CAP,
            "the client read the whole body ({sent} bytes)"
        );
    }
}

/// An error body is read only up to a small cap; the client hangs up on the
/// rest instead of buffering it.
#[tokio::test]
async fn an_error_body_is_read_only_up_to_its_cap() {
    let server = fake(Options::reply(Reply::BigError(OVER_CAP))).await;
    let (caller, config) = connect(&server.url, &[], "unused").await;
    let registry = registered(caller, &config).await;
    let error = call(&registry, "mcp__remote__leak", json!({}))
        .await
        .expect_err("a 500 is an error");
    assert!(error.to_string().contains("500"), "{error}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent = server.big_body_bytes_sent.load(Ordering::SeqCst);
    assert!(
        sent < OVER_CAP,
        "the client read the whole error body: {sent}"
    );
}

/// A server whose GET stream says `retry: 0` and closes at once must not get
/// a reconnect storm.
#[tokio::test]
async fn a_zero_retry_get_stream_does_not_cause_a_reconnect_storm() {
    let server = fake(Options {
        reply: Reply::Json("hi"),
        hammer_get: true,
        unauthorized: None,
        huge_get_event: false,
    })
    .await;
    let (caller, _config) = connect(&server.url, &[], "unused").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let gets = server.count("GET");
    caller.close().await;
    assert!(gets <= 4, "{gets} GET requests in 3 s");
}

/// A GET stream whose event is over the cap is dropped and reopened at
/// the paced rate (the reconnect budget bounds the total); it no longer disables
/// the session's GET stream, which the previous fix did.
#[tokio::test]
async fn an_oversized_get_stream_event_is_dropped_and_the_stream_reopened_paced() {
    let server = fake(Options {
        huge_get_event: true,
        ..Options::reply(Reply::Json("hi"))
    })
    .await;
    let (caller, _config) = connect(&server.url, &[], "unused").await;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let gets = server.count("GET");
    caller.close().await;
    assert!((2..=4).contains(&gets), "{gets} GET requests in 3.5 s");
}

/// A reply event over the cap, after an event with an
/// id, fails the call at once with the cap in the message. Before, `rmcp`
/// resumed the stream and the call waited out its timeout.
#[tokio::test]
async fn an_oversized_reply_event_fails_the_call_at_once() {
    let server = fake(Options::reply(Reply::OversizedSse(18 * 1024 * 1024))).await;
    let (caller, config) = connect(&server.url, &[], "unused").await;
    let registry = registered(caller, &config).await;
    let started = std::time::Instant::now();
    let error = call(&registry, "mcp__remote__leak", json!({}))
        .await
        .expect_err("an oversized reply must fail the call");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        error.to_string().contains("exceeded the 16 MiB limit"),
        "{error}"
    );
}

/// A slow tool answered by SEP-1699 polling resumes
/// its reply with `Last-Event-ID` more times than the reopen budget allows; the
/// resumes are not charged to it, and the call gets its result. Before, the
/// eleventh resume was refused and the call failed with "Transport closed".
#[tokio::test]
async fn a_tool_answered_by_polling_resumes_past_the_reopen_budget() {
    let resumes = MAX_GET_STREAM_REOPENS + 5;
    let (url, served) = common::polling_server(resumes).await;
    let config = config(&url, &[]).with_timeout_ms(60_000).expect("timeout");
    let caller = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &fixed_env("unused"),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await
    .expect("connect");
    let registry = registered(caller, &config).await;
    let out = call(&registry, "mcp__remote__slow", json!({}))
        .await
        .expect("the call completes by polling")
        .render();
    assert!(
        out.contains(&format!("finished after {resumes} resumes")),
        "{out}"
    );
    assert_eq!(served.load(Ordering::SeqCst), resumes);
}

/// A server that closes its GET stream cleanly and
/// at once gets at most one open plus the reopen budget, and then no more; every
/// open, and the refusal, is attributed.
#[tokio::test]
async fn the_get_stream_reopen_budget_ends_a_clean_reconnect_loop() {
    let server = fake(Options {
        hammer_get: true,
        ..Options::reply(Reply::Json("hi"))
    })
    .await;
    let config = config(&server.url, &[]);
    let sink = Arc::new(InMemoryAttributionSink::new());
    let caller =
        RmcpCaller::connect_streamable_http_with_env(&config, &fixed_env("unused"), sink.clone())
            .await
            .expect("connect");
    let budget = MAX_GET_STREAM_REOPENS + 1;
    // Paced at one per second, the budget runs out after about `budget` seconds.
    tokio::time::sleep(MIN_GET_STREAM_INTERVAL * (budget as u32 + 2)).await;
    let at_limit = server.count("GET");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = server.count("GET");
    caller.close().await;
    assert_eq!(at_limit, budget, "opens allowed in the window");
    assert_eq!(later, at_limit, "no GET after the budget ran out");
    let opens: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|e| e.kind == McpCallKind::OpenStream)
        .collect();
    assert_eq!(opens.len(), budget + 1, "every open and the refusal");
    assert_eq!(
        opens.last().map(|e| e.outcome.clone()),
        Some(McpCallOutcome::ProtocolError)
    );
}

/// A public header with a short value (`2.0`) is sent and never scrubbed, so
/// `"jsonrpc": "2.0"` and the tool's text survive; before the `secret` flag it
/// corrupted every reply and the connect failed.
#[tokio::test]
async fn a_short_public_header_does_not_corrupt_replies() {
    let server = fake(Options::reply(Reply::Json("release 2.0 of acme"))).await;
    let (caller, config) = connect(
        &server.url,
        &[("X-Api-Version", HeaderFromEnv::new("V").with_secret(false))],
        "2.0",
    )
    .await;
    let registry = registered(caller, &config).await;
    let out = call(&registry, "mcp__remote__leak", json!({}))
        .await
        .expect("call")
        .render();
    assert!(out.contains("release 2.0 of acme"), "{out}");
    let requests = server.requests.lock().expect("lock").clone();
    assert!(
        requests.iter().any(|r| r.contains("x-api-version: 2.0")),
        "the header was sent"
    );
}

/// A secret too short to scrub safely is refused before any request, with a
/// message that says how to declare a public header.
#[tokio::test]
async fn a_short_secret_is_refused_before_any_request() {
    let server = fake(Options::reply(Reply::Json("the team is acme"))).await;
    let config = config(&server.url, &[("X-Team-Token", HeaderFromEnv::new("V"))]);
    let Err(error) = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &fixed_env("acme"),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await
    else {
        panic!("a four-character secret must be refused");
    };
    assert!(error.to_string().contains("\"secret\": false"), "{error}");
    assert!(server.requests.lock().expect("lock").is_empty());
}

/// A server that refuses the configured credential with 401 raises
/// `AuthorizationRequired` after one request, with the challenge scrubbed and
/// capped; nothing else is tried.
#[tokio::test]
async fn a_refused_credential_stops_at_one_request_with_a_scrubbed_challenge() {
    let server = fake(Options {
        reply: Reply::Json("unused"),
        hammer_get: false,
        unauthorized: Some(r#"Bearer realm="mcp", error="invalid_token", echo="sk-LIVE-4242""#),
        huge_get_event: false,
    })
    .await;
    let config = config(&server.url, &[("Authorization", bearer("T"))]);
    let Err(error) = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &fixed_env("sk-LIVE-4242"),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await
    else {
        panic!("a refused credential must fail the connect");
    };
    let McpConnectError::AuthorizationRequired { challenge, .. } = &error else {
        panic!("AuthorizationRequired, got {error:?}");
    };
    assert!(challenge.contains(r#"realm="mcp""#), "{challenge}");
    assert!(!challenge.contains("sk-LIVE-4242"), "{challenge}");
    assert!(!format!("{error} {error:?}").contains("sk-LIVE-4242"));
    assert_eq!(server.count("POST"), 1, "no retry and no OAuth attempt");
}

/// A server configured for OAuth is refused by the plain constructor, so a
/// caller cannot connect it without its token by mistake.
#[tokio::test]
async fn an_oauth_server_is_refused_by_the_plain_constructor() {
    let server = fake(Options::reply(Reply::Json("unused"))).await;
    let config = McpServerConfig::new(
        "remote",
        McpTransport::StreamableHttp {
            url: server.url.clone(),
            headers: Default::default(),
            proxy: None,
            oauth: true,
        },
    )
    .expect("valid");
    let result = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &fixed_env("unused"),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await;
    assert!(matches!(result, Err(McpConnectError::Setup { .. })));
    assert!(server.requests.lock().expect("lock").is_empty());
}

/// A proxy is used only when the configuration names one, and then it is.
/// The fake proxy receives the tunnel request and never the credential.
#[tokio::test]
async fn a_configured_proxy_receives_the_tunnel_request_and_not_the_credential() {
    let proxy = fake(Options::reply(Reply::Json("unused"))).await;
    let proxy_address = proxy.url.trim_end_matches("/mcp").to_owned();
    let config = McpServerConfig::new(
        "remote",
        McpTransport::StreamableHttp {
            url: "https://mcp.example.invalid/mcp".to_owned(),
            headers: [("Authorization".to_owned(), bearer("T"))].into(),
            proxy: Some(proxy_address),
            oauth: false,
        },
    )
    .expect("valid")
    .with_timeout_ms(5_000)
    .expect("timeout");
    let result = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &fixed_env("tunnel-secret-1"),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await;
    assert!(result.is_err(), "the fake proxy cannot complete TLS");
    let seen = proxy.requests.lock().expect("lock").clone();
    assert!(
        seen.iter()
            .any(|r| r.starts_with("CONNECT mcp.example.invalid:443")),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|r| r.contains("tunnel-secret-1")),
        "the credential travels inside the tunnel, not to the proxy"
    );
}
