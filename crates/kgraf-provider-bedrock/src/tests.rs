//! Adapter tests against recorded-shape responses.
//!
//! Every response body is a fixture in `tests/fixtures`, each with a `_fixture`
//! header naming the AWS documentation page it was copied or constructed from. No
//! test reaches AWS, so none of them verifies the adapter against the real service.

use super::*;
use crate::eventstream::testing;
use kgraf_provider::{Ledger, Provider, Settlement};
use serde_json::json;
use sha2::{Digest, Sha256};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

const SECRET: &str = "kQ7vX2mZp9LrT4nB8sHdW1yUeC6aFjG3oIxV5tR0";
const TOKEN: &str = "FwoGZXIvYXdzEBYaDPq3lN8tK2mVh4dR7SLcQeXAMPLEsessiontoken0123456789";

fn auth() -> AwsAuth {
    AwsAuth {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_access_key: SECRET.into(),
        session_token: Some(TOKEN.into()),
        region: "us-east-1".into(),
        bearer_token: None,
    }
}

fn config(base_url: &str) -> ProviderConfig {
    ProviderConfig::new(
        ADAPTER,
        base_url,
        "unused",
        vec![
            "model-a".into(),
            "anthropic.claude-3-sonnet-20240229-v1:0".into(),
        ],
        Protocol::Additive,
    )
    .expect("config")
}

fn adapter(server: &MockServer) -> BedrockAdapter {
    BedrockAdapter::with_auth(config(&server.uri()), auth()).expect("adapter")
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut value: Value = serde_json::from_str(&text).expect("fixture json");
    let header = value
        .as_object_mut()
        .and_then(|o| o.remove("_fixture"))
        .unwrap_or_else(|| panic!("{name} has no `_fixture` header"));
    assert!(header.get("source").is_some(), "{name}: no source cited");
    assert!(header.get("provenance").is_some(), "{name}: no provenance");
    value
}

/// A fixture's events framed as an event stream.
fn stream_bytes(name: &str) -> Vec<u8> {
    let fixture = fixture(name);
    let mut out = Vec::new();
    for event in fixture["events"].as_array().expect("events") {
        out.extend(testing::event(
            event["event"].as_str().expect("name"),
            &event["payload"].to_string(),
        ));
    }
    out
}

fn eventstream_response(bytes: Vec<u8>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_bytes(bytes)
        .insert_header("content-type", "application/vnd.amazon.eventstream")
        .insert_header("x-amzn-requestid", "req-stream-1")
}

fn json_response(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(body)
        .insert_header("x-amzn-requestid", "req-1")
}

// ---- an independent SigV4 verifier -------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

fn encode(segment: &str) -> String {
    encode_path_segment(segment)
}

/// Recompute the signature the service would, from the request as it arrived.
/// The canonical path is the received path with each segment encoded a second
/// time, the rule for every service except S3.
fn verify(request: &MockRequest, secret: &str, region: &str) -> Result<(), String> {
    let header = |name: &str| {
        request
            .headers
            .get(name)
            .map(|v| v.to_str().expect("ascii").to_owned())
    };
    let authorization = header("authorization").ok_or("no authorization header")?;
    let signed = authorization
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .ok_or("no SignedHeaders")?;
    let claimed = authorization
        .split("Signature=")
        .nth(1)
        .ok_or("no Signature")?;
    let amz_date = header("x-amz-date").ok_or("no x-amz-date")?;
    let date = &amz_date[..8];

    let canonical_path: String = request
        .url
        .path()
        .split('/')
        .map(encode)
        .collect::<Vec<_>>()
        .join("/");
    let mut canonical_headers = String::new();
    for name in signed.split(';') {
        let value = header(name).ok_or(format!("signed header {name} not sent"))?;
        canonical_headers.push_str(&format!("{name}:{}\n", value.trim()));
    }
    let canonical = format!(
        "{}\n{canonical_path}\n\n{canonical_headers}\n{signed}\n{}",
        request.method,
        hex(&Sha256::digest(&request.body)),
    );
    let scope = format!("{date}/{region}/bedrock/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    for part in [region, "bedrock", "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let expected = hex(&hmac(&key, to_sign.as_bytes()));
    if expected == claimed {
        Ok(())
    } else {
        Err(format!(
            "signature mismatch\ncanonical request:\n{canonical}"
        ))
    }
}

/// Answers 200 with `then` only when the signature verifies, else 403.
struct SignedOnly {
    then: ResponseTemplate,
    secret: String,
}

impl Respond for SignedOnly {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        match verify(request, &self.secret, "us-east-1") {
            Ok(()) => self.then.clone(),
            Err(why) => ResponseTemplate::new(403).set_body_string(why),
        }
    }
}

async fn mount(server: &MockServer, then: ResponseTemplate) {
    Mock::given(method("POST"))
        .respond_with(SignedOnly {
            then,
            secret: SECRET.into(),
        })
        .mount(server)
        .await;
}

fn begin<'a>(ledger: &'a Ledger, model: &str) -> CallToken<'a> {
    ledger
        .begin(ADAPTER, Protocol::Additive, model, "agent", "task")
        .expect("begin")
}

async fn complete(
    server: &MockServer,
    request: &Request,
) -> (Result<Response, ProviderError>, Ledger, String) {
    let adapter = adapter(server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &request.model);
    let id = token.id().to_string();
    let result = adapter.complete(request, &token).await;
    drop(token);
    (result, ledger, id)
}

fn record(ledger: &Ledger, id: &str) -> kgraf_provider::CallRecord {
    ledger
        .calls()
        .expect("calls")
        .into_iter()
        .find(|c| c.id.to_string() == id)
        .expect("row")
}

// ---- Converse ------------------------------------------------------------------

#[tokio::test]
async fn a_documented_response_is_signed_completed_and_counted() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let response = result.expect("complete");

    assert!(response.text().starts_with("Here is a list of 3 pop songs"));
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.model, "model-a");
    assert_eq!(response.request_id.as_deref(), Some("req-1"));
    assert_eq!(response.usage.uncached_input, 125);
    assert_eq!(response.usage.text_output, 60);

    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Counted);
    assert_eq!(row.request_id.as_deref(), Some("req-1"));
    assert!(row.usage.raw.is_some(), "the raw usage blob is persisted");
}

#[tokio::test]
async fn a_model_id_with_a_colon_signs_and_routes_correctly() {
    // The id is sent as `%3A` and signed as `%253A`; the verifier recomputes the
    // signature from the received path, so a wrong encoding rule gets a 403 here.
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let model = "anthropic.claude-3-sonnet-20240229-v1:0";
    let (result, _ledger, _id) = complete(&server, &Request::new(model, "hi", 100)).await;
    result.expect("signed request accepted");
    let seen = server.received_requests().await.expect("requests");
    assert_eq!(
        seen[0].url.path(),
        "/model/anthropic.claude-3-sonnet-20240229-v1%3A0/converse"
    );
}

#[tokio::test]
async fn a_wrong_secret_fails_the_independent_verifier() {
    // Guards the verifier itself: it must be able to say no.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(SignedOnly {
            then: json_response(fixture("converse-text.json")),
            secret: "some-other-secret".into(),
        })
        .mount(&server)
        .await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    match result.expect_err("403") {
        ProviderError::Http { status, .. } => assert_eq!(status, 403),
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
}

#[tokio::test]
async fn usage_is_additive_with_cache_counts_kept_apart() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-cache.json"))).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let usage = result.expect("complete").usage;
    assert_eq!(usage.uncached_input, 50);
    assert_eq!(usage.cache_read, 900);
    assert_eq!(usage.cache_write, 100);
    assert_eq!(usage.total_input(), 1050);
    let row = record(&ledger, &id);
    assert_eq!(row.usage.cache_read, 900);
    assert_eq!(row.usage.cache_write, 100);
}

#[tokio::test]
async fn a_tool_call_response_carries_the_call_and_the_tool_use_stop() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-tool-use.json"))).await;
    let (result, _l, _i) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let response = result.expect("complete");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    let calls = response.tool_uses();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q");
    assert_eq!(calls[0].1, "top_song");
    assert_eq!(calls[0].2, &json!({"sign": "WZPZ"}));
}

#[tokio::test]
async fn a_stop_sequence_response_maps() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(fixture("converse-stop-sequence.json")),
    )
    .await;
    let (result, _l, _i) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    assert_eq!(result.expect("ok").stop_reason, StopReason::StopSequence);
}

#[tokio::test]
async fn an_unusable_stop_raises_with_the_reason_and_stays_counted() {
    for reason in [
        "guardrail_intervened",
        "content_filtered",
        "malformed_model_output",
        "malformed_tool_use",
        "model_context_window_exceeded",
    ] {
        let server = MockServer::start().await;
        let mut body = fixture("converse-text.json");
        body["stopReason"] = json!(reason);
        mount(&server, json_response(body)).await;
        let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
        match result.expect_err("must raise") {
            ProviderError::TurnNotUsable { stop_reason, .. } => assert_eq!(stop_reason, reason),
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(
            record(&ledger, &id).settlement,
            Settlement::Counted,
            "{reason}"
        );
    }
}

#[tokio::test]
async fn an_unknown_stop_reason_raises_after_the_billed_call_is_counted() {
    let server = MockServer::start().await;
    let mut body = fixture("converse-text.json");
    body["stopReason"] = json!("brand_new_reason");
    mount(&server, json_response(body)).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let err = result.expect_err("must raise");
    assert!(err.to_string().contains("brand_new_reason"), "{err}");
    assert_eq!(record(&ledger, &id).settlement, Settlement::Counted);
}

#[tokio::test]
async fn a_response_without_usage_is_unreconciled_not_zero() {
    let server = MockServer::start().await;
    let mut body = fixture("converse-text.json");
    body.as_object_mut().expect("object").remove("usage");
    mount(&server, json_response(body)).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::UsageMissing { .. }
    ));
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert_eq!(row.request_id.as_deref(), Some("req-1"));
}

#[tokio::test]
async fn an_inconsistent_total_keeps_the_billed_answer_and_notes_the_mismatch() {
    let server = MockServer::start().await;
    let mut body = fixture("converse-text.json");
    body["usage"]["totalTokens"] = json!(9999);
    mount(&server, json_response(body)).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let response = result.expect("the answer is kept");
    assert!(!response.text().is_empty());
    assert_eq!(
        response.usage.uncached_input, 125,
        "counts come from fields"
    );
    let note = response
        .usage
        .raw
        .as_ref()
        .and_then(|raw| raw.get("kgraf_note"))
        .and_then(Value::as_str)
        .expect("the mismatch is noted on the raw usage");
    assert!(note.contains("9999"), "{note}");
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Counted);
    assert_eq!(row.usage.uncached_input, 125);
    assert!(
        row.usage
            .raw
            .as_ref()
            .is_some_and(|raw| raw.get("kgraf_note").is_some()),
        "the row carries the note"
    );
}

#[tokio::test]
async fn a_consistent_total_carries_no_note() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let (result, _ledger, _id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let response = result.expect("complete");
    assert!(
        response
            .usage
            .raw
            .as_ref()
            .is_some_and(|raw| raw.get("kgraf_note").is_none())
    );
}

#[tokio::test]
async fn a_streamed_inconsistent_total_keeps_the_answer_too() {
    let server = MockServer::start().await;
    let fixture = fixture("converse-stream-text.events.json");
    let mut bytes = Vec::new();
    for event in fixture["events"].as_array().expect("events") {
        let mut payload = event["payload"].clone();
        if event["event"] == "metadata" {
            payload["usage"]["totalTokens"] = json!(1);
        }
        bytes.extend(testing::event(
            event["event"].as_str().expect("name"),
            &payload.to_string(),
        ));
    }
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    let response = result.expect("the answer is kept");
    assert_eq!(response.usage.uncached_input, 47);
    assert_eq!(record(&ledger, &id).settlement, Settlement::Counted);
}

#[tokio::test]
async fn blank_text_and_an_empty_tool_result_never_reach_the_wire_blank() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let mut request = Request::new("model-a", "hi", 100);
    request.messages = vec![
        kgraf_provider::Message {
            role: kgraf_provider::Role::Assistant,
            content: vec![
                Content::text("   "),
                Content::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: json!({}),
                    signature: None,
                },
            ],
        },
        kgraf_provider::Message {
            role: kgraf_provider::Role::User,
            content: vec![
                Content::ToolResult {
                    tool_use_id: "t1".into(),
                    content: String::new(),
                    is_error: false,
                },
                Content::text(""),
            ],
        },
    ];
    let (result, _ledger, _id) = complete(&server, &request).await;
    result.expect("complete");
    let seen = server.received_requests().await.expect("requests");
    let sent: Value = serde_json::from_slice(&seen[0].body).expect("json");
    let blocks = |i: usize| {
        sent["messages"][i]["content"]
            .as_array()
            .expect("blocks")
            .clone()
    };
    assert_eq!(blocks(0).len(), 1, "the blank text block is skipped");
    assert!(blocks(0)[0].get("toolUse").is_some());
    let user = blocks(1);
    assert_eq!(user.len(), 1, "the blank text block is skipped");
    assert_eq!(
        user[0]["toolResult"]["content"][0]["text"],
        json!(wire::EMPTY_TOOL_RESULT)
    );
}

#[tokio::test]
async fn a_424_model_error_leaves_the_row_open_not_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(424)
                .set_body_string(r#"{"message":"model error","originalStatusCode":500}"#),
        )
        .mount(&server)
        .await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::Http { status: 424, .. }
    ));
    assert_eq!(record(&ledger, &id).settlement, Settlement::Unreconciled);
}

#[tokio::test]
async fn a_streamed_424_leaves_the_row_open_not_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(424).set_body_string(r#"{"message":"model error"}"#))
        .mount(&server)
        .await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::Http { status: 424, .. }
    ));
    assert_eq!(record(&ledger, &id).settlement, Settlement::Unreconciled);
}

#[tokio::test]
async fn a_huge_error_body_is_bounded_and_the_call_still_fails_closed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("x".repeat(500_000)))
        .mount(&server)
        .await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    match result.expect_err("raises") {
        ProviderError::Http { status, body, .. } => {
            assert_eq!(status, 400);
            assert!(body.len() <= 4000, "{}", body.len());
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
}

#[tokio::test]
async fn an_unbounded_error_body_read_stops_at_the_size_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("y".repeat(300_000)))
        .mount(&server)
        .await;
    let http = reqwest::Client::new()
        .post(server.uri())
        .send()
        .await
        .expect("send");
    let text = read_error_body(http, std::time::Duration::from_secs(5)).await;
    assert_eq!(text.len(), MAX_ERROR_BODY_BYTES);
}

#[tokio::test]
async fn a_region_that_would_change_the_host_is_refused_before_sending() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let mut bad = auth();
    bad.region = "evil.example.com/x".into();
    let adapter = BedrockAdapter::with_auth(config(&server.uri()), bad).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, "model-a");
    let id = token.id().to_string();
    let err = adapter
        .complete(&Request::new("model-a", "hi", 100), &token)
        .await
        .expect_err("raises");
    drop(token);
    assert!(err.to_string().contains("region"), "{err}");
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
}

#[tokio::test]
async fn a_corrupt_frame_after_metadata_keeps_the_usage_on_the_open_row() {
    let server = MockServer::start().await;
    let mut bytes = stream_bytes("converse-stream-text.events.json");
    // A corrupt frame in the same chunk as the frames before it.
    let mut bad = testing::event("messageStart", r#"{"role":"assistant"}"#);
    let n = bad.len();
    bad[n - 5] ^= 0xff;
    bytes.extend(bad);
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::Decode { .. }
    ));
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert_eq!(
        row.usage.uncached_input, 47,
        "metadata before the fault counts"
    );
    assert_eq!(row.usage.text_output, 20);
}

#[tokio::test]
async fn an_api_key_is_sent_as_a_bearer_token_and_not_signed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(json_response(fixture("converse-text.json")))
        .mount(&server)
        .await;
    let key = "bedrock-api-key-value-0123456789abcdef";
    let adapter = BedrockAdapter::with_lookup(config(&server.uri()), move |name| match name {
        "AWS_BEARER_TOKEN_BEDROCK" => Some(key.into()),
        "AWS_REGION" => Some("us-east-1".into()),
        _ => None,
    })
    .expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, "model-a");
    adapter
        .complete(&Request::new("model-a", "hi", 100), &token)
        .await
        .expect("complete");
    let seen = server.received_requests().await.expect("requests");
    assert_eq!(
        seen[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().expect("ascii")),
        Some(format!("Bearer {key}").as_str())
    );
    assert!(seen[0].headers.get("x-amz-date").is_none());
    // The key is registered with the scrubber.
    assert!(!kgraf_secrets::scrub_persist(&format!("leak {key}")).contains(key));
}

#[test]
fn credentials_are_registered_with_the_scrubber_when_the_adapter_is_built() {
    let mut fixed = auth();
    // High-entropy values: the scrubber declines to register low-entropy strings.
    fixed.secret_access_key = "zP4qW8xN2vB6mK9cR3tY7uL1hG5jD0sAeF+/QoIn".into();
    fixed.session_token =
        Some("IQoJb3JpZ2luX2VjEPr//////////wEaCXVzLWVhc3QtMSJHMEUCIQD7xK2mVh4dR7SLcQeN".into());
    let secret = fixed.secret_access_key.clone();
    let session = fixed.session_token.clone().expect("token");
    let _adapter = BedrockAdapter::with_auth(config("http://127.0.0.1:1"), fixed).expect("adapter");
    // No call has been made.
    let text = format!("a {secret} b {session}");
    let scrubbed = kgraf_secrets::scrub_persist(&text);
    assert!(!scrubbed.contains(&secret), "{scrubbed}");
    assert!(!scrubbed.contains(&session), "{scrubbed}");
}

#[test]
fn a_bedrock_model_id_the_price_table_lacks_is_unpriced_never_zero() {
    use kgraf_core::cost::{ModelPrice, PriceTable};
    use kgraf_provider::accounting::GroupBy;
    // The table holds the bare model id; the route is configured with the
    // cross-region inference profile id, which is a different key.
    let mut prices = PriceTable::new();
    prices.set(
        "anthropic.claude-3-5-sonnet-20241022-v2:0",
        ModelPrice {
            uncached_input: 3_000_000,
            cache_read: 300_000,
            cache_write: 3_750_000,
            output: 15_000_000,
            reasoning: 15_000_000,
        },
    );
    let profile = "us.anthropic.claude-3-5-sonnet-20241022-v2:0";
    let usage =
        wire::normalize_usage(&json!({"inputTokens": 10, "outputTokens": 5})).expect("usage");
    assert!(matches!(
        prices.cost_on(Some(ADAPTER), profile, &usage),
        Err(kgraf_core::cost::CostError::UnpricedModel { .. })
    ));

    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(ADAPTER, Protocol::Additive, profile, "agent", "task")
        .expect("begin");
    token.settle(&usage, Some("req")).expect("settle");
    let groups = ledger.spend_by(GroupBy::Agent, &prices).expect("spend");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].unpriced, vec![profile.to_owned()]);
    assert_eq!(groups[0].cost.micros(), 0, "no cost is invented for it");
}

#[tokio::test]
async fn a_model_outside_the_configured_set_is_refused_before_sending() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let (result, ledger, id) = complete(&server, &Request::new("typo-model", "hi", 100)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::ModelUnsupported { .. }
    ));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
}

#[tokio::test]
async fn a_reasoning_effort_is_refused_rather_than_dropped() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let mut request = Request::new("model-a", "hi", 100);
    request.reasoning_effort = Some(kgraf_provider::message::ReasoningEffort::High);
    let (result, _l, _i) = complete(&server, &request).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::CapabilityNotHonored { .. }
    ));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
}

#[tokio::test]
async fn an_http_error_is_a_failed_row_and_its_body_is_scrubbed() {
    let server = MockServer::start().await;
    // A service error page that echoes a credential back.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(format!(
            "{{\"message\":\"bad token {TOKEN} secret {SECRET}\"}}"
        )))
        .mount(&server)
        .await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let err = result.expect_err("raises");
    let text = err.to_string();
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
    match err {
        ProviderError::Http { status, .. } => assert_eq!(status, 400),
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
}

#[tokio::test]
async fn a_response_body_sends_content_the_way_converse_expects() {
    // The request the adapter builds reaches the wire whole: system, tools,
    // toolUse, toolResult and inferenceConfig.
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let mut request = Request::new("model-a", "hi", 321);
    request.temperature = Some(0.5);
    request.system = vec![Content::text("be brief")];
    request.tools = vec![kgraf_provider::message::ToolDefinition {
        name: "top_song".into(),
        description: "Get the most popular song.".into(),
        input_schema: json!({"type": "object"}),
    }];
    let (result, _l, _i) = complete(&server, &request).await;
    result.expect("complete");
    let seen = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&seen[0].body).expect("json body");
    assert_eq!(
        body["inferenceConfig"],
        json!({"maxTokens": 321, "temperature": 0.5})
    );
    assert_eq!(body["system"], json!([{"text": "be brief"}]));
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "top_song"
    );
    assert_eq!(
        seen[0]
            .headers
            .get("content-type")
            .expect("ct")
            .to_str()
            .expect("ascii"),
        "application/json"
    );
}

#[tokio::test]
async fn cache_points_reach_the_wire_only_where_the_config_honors_them() {
    let mut request = Request::new("model-a", "hi", 100);
    request.system = vec![Content::cached_text("stable prefix")];
    for honors in [false, true] {
        let server = MockServer::start().await;
        mount(&server, json_response(fixture("converse-cache.json"))).await;
        let mut cfg = config(&server.uri());
        if honors {
            cfg = cfg.honoring_cache_control();
        }
        let adapter = BedrockAdapter::with_auth(cfg, auth()).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, "model-a");
        adapter.complete(&request, &token).await.expect("complete");
        let seen = server.received_requests().await.expect("requests");
        let sent = String::from_utf8(seen[0].body.clone()).expect("utf8");
        assert_eq!(
            sent.contains("cachePoint"),
            honors,
            "honors={honors}: {sent}"
        );
        assert_eq!(
            adapter.capabilities().cache_control.is_some(),
            honors,
            "the capability follows the flag"
        );
    }
}

// ---- credentials through the adapter ------------------------------------------

#[tokio::test]
async fn credentials_are_resolved_per_call_through_the_lookup_and_sign_the_request() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let adapter = BedrockAdapter::with_lookup(config(&server.uri()), |name| match name {
        "AWS_ACCESS_KEY_ID" => Some("AKIAIOSFODNN7EXAMPLE".into()),
        "AWS_SECRET_ACCESS_KEY" => Some(SECRET.into()),
        "AWS_REGION" => Some("us-east-1".into()),
        _ => None,
    })
    .expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, "model-a");
    adapter
        .complete(&Request::new("model-a", "hi", 100), &token)
        .await
        .expect("signed with looked-up keys");
    let seen = server.received_requests().await.expect("requests");
    assert!(seen[0].headers.get("x-amz-security-token").is_none());
}

#[tokio::test]
async fn missing_credentials_fail_before_sending_and_say_what_to_set() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().to_string_lossy().into_owned();
    let adapter = BedrockAdapter::with_lookup(config(&server.uri()), move |name| {
        (name == "HOME").then(|| home.clone())
    })
    .expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, "model-a");
    let id = token.id().to_string();
    let err = adapter
        .complete(&Request::new("model-a", "hi", 100), &token)
        .await
        .expect_err("raises");
    drop(token);
    assert!(err.to_string().contains("AWS_ACCESS_KEY_ID"), "{err}");
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
}

#[test]
fn the_region_fills_the_endpoint_template_and_the_model_id_is_encoded_whole() {
    let cfg = config("https://bedrock-runtime.{region}.amazonaws.com");
    let adapter = BedrockAdapter::with_auth(cfg, auth()).expect("adapter");
    assert_eq!(
        adapter.url(
            &auth(),
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.x",
            "converse"
        ),
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/\
         arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Fus.x/converse"
    );
}

#[test]
fn an_inclusive_configuration_is_refused_at_construction() {
    let mut cfg = config("https://x");
    cfg.protocol = Protocol::Inclusive;
    let err = BedrockAdapter::with_auth(cfg, auth())
        .err()
        .expect("refused");
    assert!(matches!(err, ProviderError::CapabilityNotHonored { .. }));
}

#[test]
fn the_adapter_declares_streaming_and_no_reasoning_capability() {
    let adapter = BedrockAdapter::with_auth(config("https://x"), auth()).expect("adapter");
    assert_eq!(adapter.protocol(), Protocol::Additive);
    let names = adapter.capabilities().names();
    assert_eq!(names, vec!["streaming"]);
}

// ---- ConverseStream ------------------------------------------------------------

async fn stream(
    server: &MockServer,
    request: &Request,
) -> (
    Result<Response, ProviderError>,
    Vec<StreamEvent>,
    Ledger,
    String,
) {
    let adapter = adapter(server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &request.model);
    let id = token.id().to_string();
    let mut events = Vec::new();
    let result = Streaming::stream(&adapter, request, &token, &mut |e| events.push(e)).await;
    drop(token);
    (result, events, ledger, id)
}

#[tokio::test]
async fn a_documented_event_stream_decodes_and_is_counted() {
    let server = MockServer::start().await;
    mount(
        &server,
        eventstream_response(stream_bytes("converse-stream-text.events.json")),
    )
    .await;
    let (result, events, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    let response = result.expect("stream");
    assert_eq!(response.text(), " Title: The");
    assert_eq!(response.stop_reason, StopReason::MaxTokens);
    assert_eq!(response.request_id.as_deref(), Some("req-stream-1"));
    assert_eq!(response.usage.uncached_input, 47);
    assert_eq!(response.usage.text_output, 20);

    assert!(matches!(events.first(), Some(StreamEvent::Started { .. })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Usage { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Finished {
            stop_reason: StopReason::MaxTokens
        }
    )));
    let deltas: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Delta { content } => content.as_text().map(str::to_owned),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, " Title: The");

    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Counted);
    assert_eq!(row.usage.uncached_input, 47);
    // The streamed request was signed for the streaming operation.
    let seen = server.received_requests().await.expect("requests");
    assert!(seen[0].url.path().ends_with("/converse-stream"));
}

#[tokio::test]
async fn a_streamed_tool_call_reassembles_from_fragments_with_reasoning_and_text() {
    let server = MockServer::start().await;
    mount(
        &server,
        eventstream_response(stream_bytes("converse-stream-tool-use.events.json")),
    )
    .await;
    let (result, events, _l, _i) = stream(&server, &Request::new("model-a", "hi", 100)).await;
    let response = result.expect("stream");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.reasoning(), "The user wants a song.");
    assert_eq!(response.text(), "Looking it up.");
    let calls = response.tool_uses();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "tooluse_abc123");
    assert_eq!(calls[0].2, &json!({"sign": "WZPZ"}));
    // Order follows the block index, not arrival.
    assert!(matches!(response.content[0], Content::Reasoning { .. }));
    assert!(matches!(response.content[2], Content::ToolUse { .. }));
    // The tool call is sunk once, whole.
    let sunk: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                StreamEvent::Delta {
                    content: Content::ToolUse { .. }
                }
            )
        })
        .collect();
    assert_eq!(sunk.len(), 1);
}

#[tokio::test]
async fn a_stream_delivered_one_byte_at_a_time_decodes_the_same() {
    // Chunk boundaries fall inside preludes, headers, payloads and checksums. A raw
    // server is needed to control chunking, so this drives the decoder the adapter
    // uses with every split of a real stream.
    let bytes = stream_bytes("converse-stream-tool-use.events.json");
    let whole = EventStreamDecoder::new().push(&bytes).expect("whole");
    let mut decoder = EventStreamDecoder::new();
    let mut pieces = Vec::new();
    for chunk in bytes.chunks(7) {
        pieces.extend(decoder.push(chunk).expect("chunk"));
    }
    assert_eq!(whole, pieces);
    assert_eq!(whole.len(), 13);
}

#[tokio::test]
async fn a_stream_that_ends_before_metadata_has_no_usage_and_stays_unreconciled() {
    let server = MockServer::start().await;
    let mut bytes = Vec::new();
    for (name, payload) in [
        ("messageStart", json!({"role": "assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": "partial"}}),
        ),
    ] {
        bytes.extend(testing::event(name, &payload.to_string()));
    }
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::UsageMissing { .. }
    ));
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert_eq!(row.request_id.as_deref(), Some("req-stream-1"));
}

#[tokio::test]
async fn metadata_without_a_message_stop_is_truncated() {
    let server = MockServer::start().await;
    let mut bytes = testing::event("messageStart", r#"{"role":"assistant"}"#);
    bytes.extend(testing::event(
        "metadata",
        r#"{"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#,
    ));
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::StreamTruncated { .. }
    ));
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert_eq!(row.usage.uncached_input, 1, "the seen usage is the floor");
}

#[tokio::test]
async fn an_exception_frame_inside_a_200_stream_raises_and_keeps_the_row_open() {
    let server = MockServer::start().await;
    let mut bytes = testing::event("messageStart", r#"{"role":"assistant"}"#);
    bytes.extend(testing::exception(
        "modelStreamErrorException",
        r#"{"message":"upstream failed"}"#,
    ));
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    let err = result.expect_err("raises");
    assert!(
        err.to_string().contains("modelStreamErrorException"),
        "{err}"
    );
    assert!(err.to_string().contains("upstream failed"), "{err}");
    assert_eq!(record(&ledger, &id).settlement, Settlement::Unreconciled);
}

#[tokio::test]
async fn a_corrupt_frame_raises() {
    let server = MockServer::start().await;
    let mut bytes = testing::event("messageStart", r#"{"role":"assistant"}"#);
    let n = bytes.len();
    bytes[n - 5] ^= 0xff;
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::Decode { .. }
    ));
    assert_eq!(record(&ledger, &id).settlement, Settlement::Unreconciled);
}

#[tokio::test]
async fn a_streamed_guardrail_stop_raises_and_stays_counted() {
    let server = MockServer::start().await;
    let mut bytes = testing::event("messageStart", r#"{"role":"assistant"}"#);
    bytes.extend(testing::event(
        "messageStop",
        r#"{"stopReason":"guardrail_intervened"}"#,
    ));
    bytes.extend(testing::event(
        "metadata",
        r#"{"usage":{"inputTokens":10,"outputTokens":0,"totalTokens":10}}"#,
    ));
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    match result.expect_err("raises") {
        ProviderError::TurnNotUsable { stop_reason, .. } => {
            assert_eq!(stop_reason, "guardrail_intervened");
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(record(&ledger, &id).settlement, Settlement::Counted);
}

#[tokio::test]
async fn a_streamed_unknown_stop_reason_raises_after_counting() {
    let server = MockServer::start().await;
    let mut bytes = testing::event("messageStop", r#"{"stopReason":"brand_new_reason"}"#);
    bytes.extend(testing::event(
        "metadata",
        r#"{"usage":{"inputTokens":10,"outputTokens":1,"totalTokens":11}}"#,
    ));
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    assert!(
        result
            .expect_err("raises")
            .to_string()
            .contains("brand_new_reason")
    );
    assert_eq!(record(&ledger, &id).settlement, Settlement::Counted);
}

#[tokio::test]
async fn a_streamed_cache_usage_is_additive() {
    let server = MockServer::start().await;
    let mut bytes = testing::event("messageStop", r#"{"stopReason":"end_turn"}"#);
    bytes.extend(testing::event(
        "metadata",
        &json!({"usage": fixture("converse-cache.json")["usage"]}).to_string(),
    ));
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, _l, _i) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    let usage = result.expect("stream").usage;
    assert_eq!(usage.total_input(), 1050);
    assert_eq!(usage.cache_read, 900);
}

#[tokio::test]
async fn a_streamed_request_for_a_model_outside_the_set_is_refused_before_sending() {
    let server = MockServer::start().await;
    mount(&server, eventstream_response(Vec::new())).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("typo-model", "hi", 20)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::ModelUnsupported { .. }
    ));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
}

#[test]
fn path_segments_are_encoded_to_the_unreserved_set() {
    assert_eq!(encode_path_segment("a.b-c_d~e"), "a.b-c_d~e");
    assert_eq!(encode_path_segment("v1:0"), "v1%3A0");
    assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
}

// ---- request ids, redirects, error bodies, tiers -------------------------------

fn metadata_variant(name: &str, edit: impl Fn(&mut Value)) -> Vec<u8> {
    let fixture = fixture(name);
    let mut bytes = Vec::new();
    for event in fixture["events"].as_array().expect("events") {
        let mut payload = event["payload"].clone();
        if event["event"] == "metadata" {
            edit(&mut payload);
        }
        bytes.extend(testing::event(
            event["event"].as_str().expect("name"),
            &payload.to_string(),
        ));
    }
    bytes
}

#[tokio::test]
async fn an_error_response_puts_its_request_id_on_the_row() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(424)
                .set_body_string(r#"{"message":"model error"}"#)
                .insert_header("x-amzn-requestid", "req-424"),
        )
        .mount(&server)
        .await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    assert!(result.is_err());
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert_eq!(row.request_id.as_deref(), Some("req-424"));

    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    assert!(result.is_err());
    assert_eq!(record(&ledger, &id).request_id.as_deref(), Some("req-424"));
}

#[tokio::test]
async fn a_redirect_is_an_error_and_is_never_followed_with_the_credentials() {
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&elsewhere)
        .await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/model/x/converse", elsewhere.uri())),
        )
        .mount(&server)
        .await;
    let (result, _l, _i) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    assert!(matches!(
        result.expect_err("raises"),
        ProviderError::Http { status: 307, .. }
    ));
    assert!(
        elsewhere
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "the redirect target received the signed request"
    );
}

#[tokio::test]
async fn an_error_body_that_stalls_keeps_the_bytes_that_arrived() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer);
        stream
            .write_all(
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 1000\r\n\r\nfirst-part",
            )
            .expect("write");
        stream.flush().expect("flush");
        std::thread::sleep(std::time::Duration::from_millis(1500));
    });
    let http = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}"))
        .send()
        .await
        .expect("send");
    let text = read_error_body(http, std::time::Duration::from_millis(300)).await;
    handle.join().expect("server thread");
    assert!(text.starts_with("first-part"), "{text}");
    assert!(text.contains("did not finish in time"), "{text}");
}

#[tokio::test]
async fn a_non_finite_temperature_is_refused_before_sending() {
    let server = MockServer::start().await;
    mount(&server, json_response(fixture("converse-text.json"))).await;
    for bad in [f32::NAN, f32::INFINITY] {
        let mut request = Request::new("model-a", "hi", 100);
        request.temperature = Some(bad);
        let (result, ledger, id) = complete(&server, &request).await;
        assert!(matches!(
            result.expect_err("raises"),
            ProviderError::CapabilityNotHonored { .. }
        ));
        assert_eq!(record(&ledger, &id).settlement, Settlement::Failed);
    }
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
}

#[test]
fn usage_counts_that_overflow_are_refused_with_a_floor_that_fits() {
    let raw = json!({"inputTokens": u64::MAX, "outputTokens": 5, "cacheReadInputTokens": 7});
    assert!(matches!(
        wire::normalize_usage(&raw),
        Err(ProviderError::Decode { .. })
    ));
    let floor = wire::lower_bound_usage(&raw).expect("floor");
    // The ledger adds these fields; the sum must not overflow.
    assert_eq!(floor.total(), u64::MAX);
    assert_eq!(floor.uncached_input, u64::MAX);
}

#[tokio::test]
async fn an_overflowing_usage_leaves_the_row_open_with_a_floor() {
    let server = MockServer::start().await;
    let mut body = fixture("converse-text.json");
    body["usage"]["inputTokens"] = json!(u64::MAX);
    mount(&server, json_response(body)).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    assert!(result.is_err());
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert_eq!(row.usage.uncached_input, u64::MAX);
}

#[tokio::test]
async fn a_non_default_service_tier_is_recorded_and_never_priced_as_standard() {
    let server = MockServer::start().await;
    let mut body = fixture("converse-text.json");
    body["serviceTier"] = json!({"type": "priority"});
    mount(&server, json_response(body)).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    let response = result.expect("the answer is kept");
    assert!(!response.text().is_empty());
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled, "not priced");
    assert_eq!(row.usage.uncached_input, 125, "the counts are kept");
    assert_eq!(
        row.usage
            .raw
            .as_ref()
            .and_then(|r| r.get("kgraf_service_tier")),
        Some(&json!({"type": "priority"}))
    );
}

#[tokio::test]
async fn the_default_tier_and_five_minute_cache_writes_are_priced() {
    let server = MockServer::start().await;
    let mut body = fixture("converse-text.json");
    body["serviceTier"] = json!({"type": "default"});
    body["usage"]["cacheDetails"] = json!([{"ttl": "5m", "inputTokens": 0}]);
    mount(&server, json_response(body)).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    result.expect("complete");
    assert_eq!(record(&ledger, &id).settlement, Settlement::Counted);
}

#[tokio::test]
async fn cache_writes_on_a_longer_ttl_are_not_priced_as_standard() {
    let server = MockServer::start().await;
    let mut body = fixture("converse-text.json");
    body["usage"]["cacheDetails"] = json!([{"ttl": "1h", "inputTokens": 40}]);
    mount(&server, json_response(body)).await;
    let (result, ledger, id) = complete(&server, &Request::new("model-a", "hi", 100)).await;
    result.expect("the answer is kept");
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert!(
        row.usage
            .raw
            .as_ref()
            .is_some_and(|r| r.get("cacheDetails").is_some()),
        "the TTL detail is kept in the raw usage"
    );
}

#[tokio::test]
async fn a_streamed_non_default_tier_is_not_priced_as_standard() {
    let server = MockServer::start().await;
    let bytes = metadata_variant("converse-stream-text.events.json", |payload| {
        payload["serviceTier"] = json!({"type": "flex"});
    });
    mount(&server, eventstream_response(bytes)).await;
    let (result, _e, ledger, id) = stream(&server, &Request::new("model-a", "hi", 20)).await;
    result.expect("the answer is kept");
    let row = record(&ledger, &id);
    assert_eq!(row.settlement, Settlement::Unreconciled);
    assert_eq!(row.usage.uncached_input, 47);
    assert_eq!(
        row.usage
            .raw
            .as_ref()
            .and_then(|r| r.get("kgraf_service_tier")),
        Some(&json!({"type": "flex"}))
    );
}
