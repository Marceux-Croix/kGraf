//! Adapter behavior against recorded bodies.
//!
//! Every body is written from Google's published references, and each fixture's
//! header cites its URL. None was recorded from a live call, so these tests
//! prove the adapter handles the documented shapes and refuses the inconsistent
//! ones. They do not prove the service sends those shapes.

use std::sync::Arc;

use kgraf_core::Protocol;
use kgraf_provider::capability::{StreamEvent, Streaming};
use kgraf_provider::message::ReasoningEffort;
use kgraf_provider::{
    Content, Ledger, Message, Provider, ProviderConfig, ProviderError, Request, Role, Settlement,
    Signature, StopReason, message::ToolDefinition,
};
use kgraf_provider_anthropic::BearerSource;
use kgraf_provider_google::{
    GEMINI_ADAPTER, GeminiAdapter, VERTEX_ANTHROPIC_ADAPTER, VERTEX_GEMINI_ADAPTER,
    vertex_anthropic_at, vertex_gemini_at,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEXT_THINKING: &str = include_str!("fixtures/gemini_text_thinking.json");
const FUNCTION_CALL: &str = include_str!("fixtures/gemini_function_call.json");
const PROMPT_BLOCKED: &str = include_str!("fixtures/gemini_prompt_blocked.json");
const STREAM_TEXT: &str = include_str!("fixtures/gemini_stream_text.sse");
const STREAM_CALL: &str = include_str!("fixtures/gemini_stream_function_call.sse");
const VERTEX_MESSAGE: &str = include_str!("fixtures/vertex_anthropic_message.json");
const VERTEX_STREAM: &str = include_str!("fixtures/vertex_anthropic_stream.sse");

const MODEL: &str = "gemini-2.5-flash";
const CLAUDE: &str = "claude-3-5-sonnet-v2@20241022";

/// A fixture body without its citation header.
fn fixture(text: &str) -> Value {
    let mut value: Value = serde_json::from_str(text).expect("fixture json");
    let source = value
        .as_object_mut()
        .and_then(|o| o.remove("_source"))
        .expect("every fixture cites its source");
    assert!(
        source.as_str().is_some_and(|s| s.contains("https://")),
        "a fixture must cite a URL"
    );
    value
}

fn sse(text: &str) -> ResponseTemplate {
    assert!(
        text.contains("https://"),
        "a stream fixture must cite a URL"
    );
    ResponseTemplate::new(200).set_body_raw(text.as_bytes().to_vec(), "text/event-stream")
}

struct StaticBearer(&'static str);

#[async_trait::async_trait]
impl BearerSource for StaticBearer {
    async fn bearer(&self) -> Result<String, ProviderError> {
        Ok(self.0.to_owned())
    }
}

struct NoCredentials;

#[async_trait::async_trait]
impl BearerSource for NoCredentials {
    async fn bearer(&self) -> Result<String, ProviderError> {
        Err(ProviderError::Unsent {
            adapter: "test",
            reason: "no credentials".to_owned(),
        })
    }
}

fn gemini(server: &MockServer) -> GeminiAdapter {
    let config = ProviderConfig::new(
        GEMINI_ADAPTER,
        format!("{}/v1beta", server.uri()),
        "test-key",
        vec![MODEL.into()],
        Protocol::Inclusive,
    )
    .expect("config");
    GeminiAdapter::api_key(config).expect("adapter")
}

const VERTEX_GEMINI_ROOT: &str = "/v1/projects/p/locations/r/publishers/google";
const VERTEX_CLAUDE_ROOT: &str = "/v1/projects/p/locations/r/publishers/anthropic";

fn vertex_gemini(server: &MockServer, auth: Arc<dyn BearerSource>) -> GeminiAdapter {
    let config = ProviderConfig::local(
        format!("{}{VERTEX_GEMINI_ROOT}", server.uri()),
        vec![MODEL.into()],
        Protocol::Inclusive,
    );
    vertex_gemini_at(config, auth).expect("adapter")
}

fn vertex_claude(
    server: &MockServer,
    auth: Arc<dyn BearerSource>,
) -> kgraf_provider_anthropic::AnthropicAdapter {
    let config = ProviderConfig::local(
        format!("{}{VERTEX_CLAUDE_ROOT}", server.uri()),
        vec![CLAUDE.into()],
        Protocol::Additive,
    );
    vertex_anthropic_at(config, auth).expect("adapter")
}

fn begin<'a>(
    ledger: &'a Ledger,
    provider: &dyn Provider,
    model: &str,
) -> kgraf_provider::CallToken<'a> {
    ledger
        .begin(
            provider.adapter(),
            provider.protocol(),
            model,
            "agent",
            "task",
        )
        .expect("begin")
}

async fn bodies_received(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recording on")
        .iter()
        .map(|r| r.body_json::<Value>().expect("json body"))
        .collect()
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_current_weather".into(),
        description: "Current weather".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"location": {"type": "string"}},
            "required": ["location"],
            "additionalProperties": false
        }),
    }
}

// ---------------------------------------------------------------------------
// Gemini generateContent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_completion_is_issued_authenticated_and_counted_with_thinking_as_reasoning() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1beta/models/{MODEL}:generateContent")))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(TEXT_THINKING)))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);

    let mut request = Request::new(MODEL, "What is the answer?", 500);
    request.system = vec![Content::text("Be brief.")];
    request.temperature = Some(0.7);
    let response = adapter.complete(&request, &token).await.expect("complete");

    assert_eq!(response.text(), "The answer is 42.");
    assert_eq!(response.reasoning(), "Let me think about it.");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.request_id.as_deref(), Some("resp-0001"));
    // Inclusive: 100 prompt tokens contain the 40 cached ones.
    assert_eq!(response.usage.uncached_input, 60);
    assert_eq!(response.usage.cache_read, 40);
    assert_eq!(response.usage.cache_write, 0);
    assert_eq!(response.usage.text_output, 20);
    assert_eq!(response.usage.reasoning_output, 30);
    assert_eq!(response.usage.total(), 150);

    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted);
    assert!(record.usage.raw.is_some(), "the raw usage blob is kept");

    let sent = bodies_received(&server).await;
    assert_eq!(sent[0]["generationConfig"]["maxOutputTokens"], 500);
    assert_eq!(sent[0]["generationConfig"]["temperature"], 0.7);
    assert_eq!(
        sent[0]["systemInstruction"]["parts"][0]["text"],
        "Be brief."
    );
    assert_eq!(sent[0]["contents"][0]["role"], "user");
    assert_eq!(
        sent[0]["contents"][0]["parts"][0]["text"],
        "What is the answer?"
    );
    assert!(sent[0].get("model").is_none(), "the model is in the URL");
    assert!(sent[0].get("tools").is_none());
}

#[tokio::test]
async fn a_total_above_the_parts_is_billed_as_reasoning() {
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    // Parts sum to 150; 15 billed tokens are in no counted field.
    body["usageMetadata"]["totalTokenCount"] = json!(165);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let response = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect("complete");
    assert_eq!(response.usage.reasoning_output, 45);
    assert_eq!(response.usage.text_output, 20);
    assert_eq!(response.usage.total(), 165);
}

#[tokio::test]
async fn a_total_below_the_parts_is_refused_and_keeps_a_floor() {
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    body["usageMetadata"]["totalTokenCount"] = json!(120);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect_err("must refuse");
    assert!(matches!(error, ProviderError::Decode { .. }), "{error:?}");
    assert!(error.to_string().contains("below the counted fields"));

    let open = ledger.unreconciled().expect("unreconciled");
    assert_eq!(open.len(), 1, "a billed but refused call stays open");
    assert_eq!(
        open[0].usage.cache_read, 40,
        "the floor keeps the reported counts"
    );
    assert_eq!(open[0].usage.text_output, 20);
}

#[tokio::test]
async fn cached_tokens_above_the_prompt_count_raise() {
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    body["usageMetadata"]["cachedContentTokenCount"] = json!(500);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(matches!(error, ProviderError::Usage { .. }), "{error:?}");
}

#[tokio::test]
async fn a_reply_with_no_candidate_count_is_zero_output_not_missing_usage() {
    // A reply made only of thoughts has no candidatesTokenCount in the reference's
    // examples of omitted zero fields.
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    body["usageMetadata"] = json!({
        "promptTokenCount": 10, "thoughtsTokenCount": 90, "totalTokenCount": 100
    });
    body["candidates"][0]["finishReason"] = json!("MAX_TOKENS");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let response = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect("complete");
    assert_eq!(response.stop_reason, StopReason::MaxTokens);
    assert_eq!(response.usage.text_output, 0);
    assert_eq!(response.usage.reasoning_output, 90);
}

#[tokio::test]
async fn missing_usage_and_missing_model_version_raise() {
    for (remove, expect_usage) in [("usageMetadata", true), ("modelVersion", false)] {
        let server = MockServer::start().await;
        let mut body = fixture(TEXT_THINKING);
        body.as_object_mut().expect("object").remove(remove);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let adapter = gemini(&server);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, &adapter, MODEL);
        let error = adapter
            .complete(&Request::new(MODEL, "hi", 100), &token)
            .await
            .expect_err("must raise");
        if expect_usage {
            assert!(
                matches!(error, ProviderError::UsageMissing { .. }),
                "{error:?}"
            );
        } else {
            assert!(matches!(error, ProviderError::Decode { .. }), "{error:?}");
        }
        assert_eq!(ledger.unreconciled().expect("open").len(), 1);
    }
}

#[tokio::test]
async fn a_substituted_model_raises_and_keeps_the_call_open() {
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    body["modelVersion"] = json!("gemini-2.5-flash-lite");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(
        matches!(error, ProviderError::ModelSubstituted { .. }),
        "{error:?}"
    );
    let open = ledger.unreconciled().expect("open");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].usage.text_output, 20, "the billed figure is kept");
}

#[tokio::test]
async fn a_model_outside_the_configured_set_is_refused_before_any_request() {
    let server = MockServer::start().await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "typo-model");
    let error = adapter
        .complete(&Request::new("typo-model", "hi", 100), &token)
        .await
        .expect_err("must refuse");
    assert!(matches!(error, ProviderError::ModelUnsupported { .. }));
    assert!(server.received_requests().await.expect("on").is_empty());
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("row")
            .settlement,
        Settlement::Failed
    );
}

#[tokio::test]
async fn a_model_id_that_could_change_the_path_is_refused() {
    let server = MockServer::start().await;
    let config = ProviderConfig::new(
        GEMINI_ADAPTER,
        format!("{}/v1beta", server.uri()),
        "test-key",
        vec!["a/../b".into()],
        Protocol::Inclusive,
    )
    .expect("config");
    let adapter = GeminiAdapter::api_key(config).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "a/../b");
    let error = adapter
        .complete(&Request::new("a/../b", "hi", 100), &token)
        .await
        .expect_err("must refuse");
    assert!(matches!(error, ProviderError::ModelUnsupported { .. }));
    assert!(server.received_requests().await.expect("on").is_empty());
}

#[tokio::test]
async fn an_http_rejection_is_reported_and_closed_as_unbilled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"code": 400, "message": "API key not valid.", "status": "INVALID_ARGUMENT"}
        })))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect_err("must raise");
    match error {
        ProviderError::Http { status, body, .. } => {
            assert_eq!(status, 400);
            assert!(body.contains("API key not valid"));
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("row")
            .settlement,
        Settlement::Failed
    );
}

#[tokio::test]
async fn the_api_key_is_never_in_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(TEXT_THINKING)))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect("complete");
    let requests = server.received_requests().await.expect("on");
    assert!(!requests[0].url.as_str().contains("test-key"));
    assert!(requests[0].url.query().is_none());
}

// --- tool calling ---------------------------------------------------------

#[tokio::test]
async fn a_function_call_becomes_a_tool_use_and_the_declarations_use_json_schema() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(FUNCTION_CALL)))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let mut request = Request::new(MODEL, "Weather in Boston?", 500);
    request.tools = vec![weather_tool()];
    let response = adapter.complete(&request, &token).await.expect("complete");

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    let calls = response.tool_uses();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "get_current_weather");
    assert_eq!(calls[0].2, &json!({"location": "Boston"}));
    assert!(!calls[0].0.is_empty(), "a call with no id is given one");
    // 80 + 12 + 20 thoughts.
    assert_eq!(response.usage.reasoning_output, 20);

    let sent = bodies_received(&server).await;
    let declaration = &sent[0]["tools"][0]["functionDeclarations"][0];
    assert_eq!(declaration["name"], "get_current_weather");
    assert_eq!(declaration["description"], "Current weather");
    assert_eq!(
        declaration["parametersJsonSchema"]["required"],
        json!(["location"])
    );
    assert!(declaration.get("parameters").is_none());
}

#[tokio::test]
async fn a_tool_round_trip_names_the_function_and_returns_the_thought_signature() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(FUNCTION_CALL)))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");

    let mut first = Request::new(MODEL, "Weather in Boston?", 500);
    first.tools = vec![weather_tool()];
    let token = begin(&ledger, &adapter, MODEL);
    let response = adapter.complete(&first, &token).await.expect("first");
    let (id, name, input) = {
        let calls = response.tool_uses();
        (
            calls[0].0.to_owned(),
            calls[0].1.to_owned(),
            calls[0].2.clone(),
        )
    };
    let signature = response.content.iter().find_map(|block| match block {
        Content::ToolUse { signature, .. } => signature.clone(),
        _ => None,
    });
    assert_eq!(
        signature.as_ref().map(|s| s.value.as_str()),
        Some("c2lnbmF0dXJlLWJ5dGVz")
    );
    assert!(
        signature
            .as_ref()
            .is_some_and(|s| s.is_from(&format!("{GEMINI_ADAPTER}:{MODEL}"))),
        "the token is tagged with the adapter and model that made it"
    );

    let mut second = first.clone();
    second.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolUse {
            id: id.clone(),
            name,
            input,
            signature,
        }],
    });
    second.messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: id,
            content: "72F and sunny".into(),
            is_error: false,
        }],
    });
    let token = begin(&ledger, &adapter, MODEL);
    adapter.complete(&second, &token).await.expect("second");

    let sent = bodies_received(&server).await;
    let contents = &sent[1]["contents"];
    assert_eq!(contents[1]["role"], "model");
    let call = &contents[1]["parts"][0];
    assert_eq!(call["functionCall"]["name"], "get_current_weather");
    assert_eq!(call["functionCall"]["args"], json!({"location": "Boston"}));
    assert_eq!(call["thoughtSignature"], "c2lnbmF0dXJlLWJ5dGVz");
    assert!(
        call["functionCall"].get("id").is_none(),
        "an id the adapter made up is not sent"
    );
    assert_eq!(contents[2]["role"], "user");
    let result = &contents[2]["parts"][0]["functionResponse"];
    assert_eq!(result["name"], "get_current_weather");
    assert_eq!(result["response"], json!({"output": "72F and sunny"}));
}

#[tokio::test]
async fn a_failed_tool_result_is_sent_under_the_error_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(TEXT_THINKING)))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let mut request = Request::new(MODEL, "go", 100);
    request.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolUse {
            id: "call-9".into(),
            name: "read_file".into(),
            input: json!({"path": "x"}),
            signature: None,
        }],
    });
    request.messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: "call-9".into(),
            content: "ERROR: not found".into(),
            is_error: true,
        }],
    });
    let token = begin(&ledger, &adapter, MODEL);
    adapter.complete(&request, &token).await.expect("complete");
    let sent = bodies_received(&server).await;
    let contents = &sent[0]["contents"];
    // A real id (one the model sent) goes back on both parts.
    assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "call-9");
    let result = &contents[2]["parts"][0]["functionResponse"];
    assert_eq!(result["id"], "call-9");
    assert_eq!(result["response"], json!({"error": "ERROR: not found"}));
}

#[tokio::test]
async fn a_tool_result_with_no_matching_call_is_refused_before_sending() {
    let server = MockServer::start().await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let mut request = Request::new(MODEL, "go", 100);
    request.messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: "orphan".into(),
            content: "x".into(),
            is_error: false,
        }],
    });
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&request, &token)
        .await
        .expect_err("must refuse");
    assert!(
        matches!(error, ProviderError::CapabilityNotHonored { .. }),
        "{error:?}"
    );
    assert!(server.received_requests().await.expect("on").is_empty());
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("row")
            .settlement,
        Settlement::Failed
    );
}

// --- thinking config ------------------------------------------------------

#[tokio::test]
async fn the_reasoning_effort_reaches_the_thinking_config_by_model_family() {
    for (model, effort, expected) in [
        (
            "gemini-2.5-flash",
            ReasoningEffort::Low,
            json!({"thinkingBudget": 1024, "includeThoughts": true}),
        ),
        (
            "gemini-2.5-flash",
            ReasoningEffort::High,
            json!({"thinkingBudget": 24576, "includeThoughts": true}),
        ),
        (
            "gemini-3-pro-preview",
            ReasoningEffort::High,
            json!({"thinkingLevel": "high", "includeThoughts": true}),
        ),
    ] {
        let server = MockServer::start().await;
        let mut body = fixture(TEXT_THINKING);
        body["modelVersion"] = json!(model);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let config = ProviderConfig::new(
            GEMINI_ADAPTER,
            format!("{}/v1beta", server.uri()),
            "test-key",
            vec![model.into()],
            Protocol::Inclusive,
        )
        .expect("config");
        let adapter = GeminiAdapter::api_key(config).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, &adapter, model);
        let mut request = Request::new(model, "hi", 60_000);
        request.reasoning_effort = Some(effort);
        adapter.complete(&request, &token).await.expect("complete");
        let sent = bodies_received(&server).await;
        assert_eq!(
            sent[0]["generationConfig"]["thinkingConfig"], expected,
            "{model}"
        );
    }
}

#[tokio::test]
async fn no_thinking_config_is_sent_unless_an_effort_is_asked_for() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(TEXT_THINKING)))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect("complete");
    let sent = bodies_received(&server).await;
    assert!(sent[0]["generationConfig"].get("thinkingConfig").is_none());
}

// --- finish reasons -------------------------------------------------------

#[tokio::test]
async fn finish_reasons_map_and_an_unknown_one_raises_after_settling() {
    // Ends the turn normally, or at the ceiling.
    for (reason, expected) in [
        ("STOP", StopReason::EndTurn),
        ("MAX_TOKENS", StopReason::MaxTokens),
    ] {
        let server = MockServer::start().await;
        let mut body = fixture(TEXT_THINKING);
        body["candidates"][0]["finishReason"] = json!(reason);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let adapter = gemini(&server);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, &adapter, MODEL);
        let response = adapter
            .complete(&Request::new(MODEL, "hi", 100), &token)
            .await
            .expect("complete");
        assert_eq!(response.stop_reason, expected, "{reason}");
    }
    // A turn that is not an answer: billed and recorded, then refused with the reason.
    for reason in [
        "SAFETY",
        "RECITATION",
        "BLOCKLIST",
        "PROHIBITED_CONTENT",
        "SPII",
        "MALFORMED_FUNCTION_CALL",
        "UNEXPECTED_TOOL_CALL",
        "OTHER",
    ] {
        let server = MockServer::start().await;
        let mut body = fixture(TEXT_THINKING);
        body["candidates"][0]["finishReason"] = json!(reason);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let adapter = gemini(&server);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, &adapter, MODEL);
        let error = adapter
            .complete(&Request::new(MODEL, "hi", 100), &token)
            .await
            .expect_err("must raise");
        match error {
            ProviderError::TurnNotUsable { stop_reason, .. } => assert_eq!(stop_reason, reason),
            other => panic!("{reason}: wrong error {other:?}"),
        }
        assert_eq!(
            ledger
                .call(token.id())
                .expect("read")
                .expect("row")
                .settlement,
            Settlement::Counted,
            "{reason}: the call was billed and is counted"
        );
    }
    // A reason this build does not name raises, and the call is still counted.
    for reason in ["A_REASON_ADDED_LATER", "FINISH_REASON_UNSPECIFIED"] {
        let server = MockServer::start().await;
        let mut body = fixture(TEXT_THINKING);
        body["candidates"][0]["finishReason"] = json!(reason);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let adapter = gemini(&server);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, &adapter, MODEL);
        let error = adapter
            .complete(&Request::new(MODEL, "hi", 100), &token)
            .await
            .expect_err("must raise");
        assert!(
            matches!(error, ProviderError::Decode { .. }),
            "{reason}: {error:?}"
        );
        assert!(error.to_string().contains(reason));
        assert_eq!(
            ledger
                .call(token.id())
                .expect("read")
                .expect("row")
                .settlement,
            Settlement::Counted
        );
    }
}

#[tokio::test]
async fn a_blocked_prompt_raises_with_the_reason_and_stays_counted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(PROMPT_BLOCKED)))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect_err("must raise");
    match error {
        ProviderError::TurnNotUsable { stop_reason, .. } => {
            assert!(stop_reason.contains("PROHIBITED_CONTENT"), "{stop_reason}");
        }
        other => panic!("wrong error: {other:?}"),
    }
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(record.usage.uncached_input, 12);
}

#[tokio::test]
async fn an_unmodeled_response_part_raises_instead_of_dropping_output() {
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    body["candidates"][0]["content"]["parts"] = json!([
        {"inlineData": {"mimeType": "image/png", "data": "AAAA"}}
    ]);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(error.to_string().contains("inlineData"), "{error}");
}

// --- streaming ------------------------------------------------------------

#[tokio::test]
async fn a_stream_delivers_deltas_and_takes_the_last_cumulative_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{MODEL}:streamGenerateContent"
        )))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(sse(STREAM_TEXT))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let mut events = Vec::new();
    let response = adapter
        .stream(&Request::new(MODEL, "hi", 100), &token, &mut |e| {
            events.push(e)
        })
        .await
        .expect("stream");

    assert_eq!(response.text(), "Hello, world!");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    // Cumulative, not summed: the last report is 10 prompt (4 cached), 5 candidates,
    // 7 thoughts.
    assert_eq!(response.usage.uncached_input, 6);
    assert_eq!(response.usage.cache_read, 4);
    assert_eq!(response.usage.text_output, 5);
    assert_eq!(response.usage.reasoning_output, 7);
    assert_eq!(response.request_id.as_deref(), Some("resp-s1"));

    assert!(matches!(
        &events[0],
        StreamEvent::Started { model, request_id }
            if model == MODEL && request_id.as_deref() == Some("resp-s1")
    ));
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Delta {
                content: Content::Text { text, .. },
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hello", ", wor", "ld!"]);
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Finished {
            stop_reason: StopReason::EndTurn
        })
    ));
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(record.usage.total(), 22);
}

#[tokio::test]
async fn a_streamed_function_call_arrives_whole_with_its_thinking() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(STREAM_CALL))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let mut events = Vec::new();
    let mut request = Request::new(MODEL, "Weather?", 100);
    request.tools = vec![weather_tool()];
    let response = adapter
        .stream(&request, &token, &mut |e| events.push(e))
        .await
        .expect("stream");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.reasoning(), "I should look up the weather.");
    assert_eq!(response.tool_uses().len(), 1);
    assert_eq!(response.usage.reasoning_output, 6);
    assert_eq!(response.usage.text_output, 9);
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Delta { content: Content::ToolUse { name, .. } } if name == "get_current_weather"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Delta {
            content: Content::Reasoning { .. }
        }
    )));
}

#[tokio::test]
async fn a_stream_cut_before_its_finish_reason_is_truncated_with_a_floor() {
    let server = MockServer::start().await;
    let cut: String = STREAM_TEXT
        .split("\n\n")
        .take(3)
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\n";
    Mock::given(method("POST"))
        .respond_with(sse(&cut))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .stream(&Request::new(MODEL, "hi", 100), &token, &mut |_| {})
        .await
        .expect_err("must raise");
    assert!(
        matches!(error, ProviderError::StreamTruncated { .. }),
        "{error:?}"
    );
    let open = ledger.unreconciled().expect("open");
    assert_eq!(open.len(), 1);
    assert_eq!(
        open[0].usage.total(),
        10 + 3,
        "the last seen figure is the floor"
    );
}

#[tokio::test]
async fn a_stream_with_no_usage_raises() {
    let server = MockServer::start().await;
    // The shape is the reference's (https://ai.google.dev/api/generate-content), with
    // usageMetadata omitted.
    let body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-2.5-flash\"}\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .stream(&Request::new(MODEL, "hi", 100), &token, &mut |_| {})
        .await
        .expect_err("must raise");
    assert!(
        matches!(error, ProviderError::UsageMissing { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_stream_error_event_raises() {
    let server = MockServer::start().await;
    let body = "data: {\"error\":{\"code\":503,\"message\":\"The model is overloaded.\",\"status\":\"UNAVAILABLE\"}}\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .stream(&Request::new(MODEL, "hi", 100), &token, &mut |_| {})
        .await
        .expect_err("must raise");
    assert!(error.to_string().contains("overloaded"), "{error}");
}

#[tokio::test]
async fn a_streamed_substituted_model_raises_before_the_output_is_consumed() {
    let server = MockServer::start().await;
    let swapped = STREAM_TEXT.replace(
        "\"modelVersion\":\"gemini-2.5-flash\"",
        "\"modelVersion\":\"gemini-2.0-flash\"",
    );
    Mock::given(method("POST"))
        .respond_with(sse(&swapped))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let mut deltas = 0;
    let error = adapter
        .stream(&Request::new(MODEL, "hi", 100), &token, &mut |e| {
            if matches!(e, StreamEvent::Delta { .. }) {
                deltas += 1;
            }
        })
        .await
        .expect_err("must raise");
    assert!(
        matches!(error, ProviderError::ModelSubstituted { .. }),
        "{error:?}"
    );
    assert_eq!(deltas, 0);
}

// ---------------------------------------------------------------------------
// Vertex AI: Gemini
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vertex_gemini_posts_to_the_publisher_path_with_a_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{VERTEX_GEMINI_ROOT}/models/{MODEL}:generateContent"
        )))
        .and(header("authorization", "Bearer ya29.test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(TEXT_THINKING)))
        .mount(&server)
        .await;
    let adapter = vertex_gemini(&server, Arc::new(StaticBearer("ya29.test-token")));
    assert_eq!(adapter.adapter(), VERTEX_GEMINI_ADAPTER);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let response = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect("complete");
    assert_eq!(response.usage.total(), 150);
    let requests = server.received_requests().await.expect("on");
    assert!(
        requests[0].headers.get("x-goog-api-key").is_none(),
        "no API key goes to Vertex"
    );
}

#[tokio::test]
async fn vertex_gemini_streams_on_the_same_path_with_alt_sse() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{VERTEX_GEMINI_ROOT}/models/{MODEL}:streamGenerateContent"
        )))
        .and(query_param("alt", "sse"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(sse(STREAM_TEXT))
        .mount(&server)
        .await;
    let adapter = vertex_gemini(&server, Arc::new(StaticBearer("tok")));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let response = adapter
        .stream(&Request::new(MODEL, "hi", 100), &token, &mut |_| {})
        .await
        .expect("stream");
    assert_eq!(response.text(), "Hello, world!");
}

#[tokio::test]
async fn a_credential_failure_sends_nothing_and_closes_the_call_as_unbilled() {
    let server = MockServer::start().await;
    let adapter = vertex_gemini(&server, Arc::new(NoCredentials));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(matches!(error, ProviderError::Unsent { .. }), "{error:?}");
    assert!(server.received_requests().await.expect("on").is_empty());
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("row")
            .settlement,
        Settlement::Failed
    );
}

// ---------------------------------------------------------------------------
// Vertex AI: Anthropic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vertex_anthropic_posts_the_messages_body_to_the_model_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{VERTEX_CLAUDE_ROOT}/models/{CLAUDE}:rawPredict"
        )))
        .and(header("authorization", "Bearer ya29.test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(VERTEX_MESSAGE)))
        .mount(&server)
        .await;
    let adapter = vertex_claude(&server, Arc::new(StaticBearer("ya29.test-token")));
    assert_eq!(adapter.adapter(), VERTEX_ANTHROPIC_ADAPTER);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, CLAUDE);
    let mut request = Request::new(CLAUDE, "hi", 100);
    request.tools = vec![weather_tool()];
    let response = adapter.complete(&request, &token).await.expect("complete");

    assert_eq!(response.text(), "Hello from Vertex.");
    // The versioned id is spelled name@version in the path and name-version in the
    // response, and that is not a substitution.
    assert_eq!(response.model, "claude-3-5-sonnet-v2-20241022");
    // Additive: the 25 input tokens exclude cache reads and writes.
    assert_eq!(response.usage.uncached_input, 25);
    assert_eq!(response.usage.text_output, 6);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);

    let requests = server.received_requests().await.expect("on");
    let body: Value = requests[0].body_json().expect("json");
    assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
    assert!(body.get("model").is_none(), "the model is in the URL");
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["tools"][0]["name"], "get_current_weather");
    assert!(requests[0].headers.get("x-api-key").is_none());
    assert!(
        requests[0].headers.get("anthropic-version").is_none(),
        "the version is in the body on this transport"
    );
}

#[tokio::test]
async fn vertex_anthropic_streams_through_the_stream_verb() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{VERTEX_CLAUDE_ROOT}/models/{CLAUDE}:streamRawPredict"
        )))
        .and(header("authorization", "Bearer tok"))
        .respond_with(sse(VERTEX_STREAM))
        .mount(&server)
        .await;
    let adapter = vertex_claude(&server, Arc::new(StaticBearer("tok")));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, CLAUDE);
    let mut events = Vec::new();
    let response = adapter
        .capabilities()
        .streaming
        .expect("streams")
        .stream(&Request::new(CLAUDE, "hi", 100), &token, &mut |e| {
            events.push(e)
        })
        .await
        .expect("stream");
    assert_eq!(response.text(), "Hello from Vertex.");
    assert_eq!(response.usage.uncached_input, 25);
    assert_eq!(response.usage.text_output, 6);
    let body: Value = server.received_requests().await.expect("on")[0]
        .body_json()
        .expect("json");
    assert_eq!(body["stream"], true);
    assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
    assert!(body.get("model").is_none());
}

#[tokio::test]
async fn vertex_anthropic_still_refuses_a_different_model() {
    let server = MockServer::start().await;
    let mut body = fixture(VERTEX_MESSAGE);
    body["model"] = json!("claude-3-haiku-20240307");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = vertex_claude(&server, Arc::new(StaticBearer("tok")));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, CLAUDE);
    let error = adapter
        .complete(&Request::new(CLAUDE, "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(
        matches!(error, ProviderError::ModelSubstituted { .. }),
        "{error:?}"
    );
    assert_eq!(ledger.unreconciled().expect("open").len(), 1);
}

#[tokio::test]
async fn vertex_anthropic_credential_failure_sends_nothing() {
    let server = MockServer::start().await;
    let adapter = vertex_claude(&server, Arc::new(NoCredentials));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, CLAUDE);
    let error = adapter
        .complete(&Request::new(CLAUDE, "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(matches!(error, ProviderError::Unsent { .. }), "{error:?}");
    assert!(server.received_requests().await.expect("on").is_empty());
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("row")
            .settlement,
        Settlement::Failed
    );
}

// --- thought signatures ---------------------------------------------------

/// The documented value for a function call that no Gemini model produced.
/// https://cloud.google.com/vertex-ai/generative-ai/docs/thought-signatures
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

fn gemini_for(server: &MockServer, model: &str) -> GeminiAdapter {
    let config = ProviderConfig::new(
        GEMINI_ADAPTER,
        format!("{}/v1beta", server.uri()),
        "test-key",
        vec![model.into()],
        Protocol::Inclusive,
    )
    .expect("config");
    GeminiAdapter::api_key(config).expect("adapter")
}

/// The tag the Developer API adapter puts on a token from `model`.
fn producer(model: &str) -> String {
    format!("{GEMINI_ADAPTER}:{model}")
}

/// A history whose assistant turn holds `calls` (id, signature) function calls,
/// each followed by its result.
fn call_history(model: &str, calls: Vec<(&str, Option<&str>)>) -> Request {
    let mut request = Request::new(model, "Weather?", 500);
    request.tools = vec![weather_tool()];
    request.messages.push(Message {
        role: Role::Assistant,
        content: calls
            .iter()
            .map(|(id, signature)| Content::ToolUse {
                id: (*id).into(),
                name: "get_current_weather".into(),
                input: json!({"location": "Boston"}),
                signature: signature.map(|s| Signature::new(s, producer(model))),
            })
            .collect(),
    });
    request.messages.push(Message {
        role: Role::User,
        content: calls
            .iter()
            .map(|(id, _)| Content::ToolResult {
                tool_use_id: (*id).into(),
                content: "72F".into(),
                is_error: false,
            })
            .collect(),
    });
    request
}

async fn sent_contents(server: &MockServer, model: &str, request: &Request) -> Value {
    let adapter = gemini_for(server, model);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, model);
    adapter.complete(request, &token).await.expect("complete");
    bodies_received(server).await[0]["contents"].clone()
}

async fn text_server(model: &str) -> MockServer {
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    body["modelVersion"] = json!(model);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_call_from_another_route_gets_the_documented_skip_value_on_gemini_3() {
    let model = "gemini-3-pro-preview";
    let server = text_server(model).await;
    let request = call_history(model, vec![("c1", None), ("c2", None)]);
    let contents = sent_contents(&server, model, &request).await;
    let parts = contents[1]["parts"].as_array().expect("parts");
    // The service validates the first call of a step; the parallel calls after it
    // carry no signature of their own.
    assert_eq!(parts[0]["thoughtSignature"], SKIP_SIGNATURE);
    assert!(parts[1].get("thoughtSignature").is_none());
}

#[tokio::test]
async fn no_skip_value_is_sent_to_a_model_that_does_not_validate_signatures() {
    let server = text_server(MODEL).await;
    let request = call_history(MODEL, vec![("c1", None)]);
    let contents = sent_contents(&server, MODEL, &request).await;
    assert!(contents[1]["parts"][0].get("thoughtSignature").is_none());
}

#[tokio::test]
async fn a_persisted_signature_is_sent_back_by_a_fresh_adapter() {
    // A new adapter has no memory of the earlier response, as after a resume or
    // in a new worker process: the signature travels with the history.
    let model = "gemini-3-pro-preview";
    let server = text_server(model).await;
    let request = call_history(model, vec![("c1", Some("c2ln"))]);
    let contents = sent_contents(&server, model, &request).await;
    assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "c2ln");
}

#[tokio::test]
async fn a_signature_survives_the_transcript_encoding() {
    let block = Content::ToolUse {
        id: "c1".into(),
        name: "f".into(),
        input: json!({}),
        signature: Some("c2ln".into()),
    };
    let text = serde_json::to_string(&block).expect("encode");
    assert_eq!(
        serde_json::from_str::<Content>(&text).expect("decode"),
        block
    );
    // A transcript written before the field existed still reads, with none.
    let old = r#"{"type":"tool_use","id":"c1","name":"f","input":{}}"#;
    assert!(matches!(
        serde_json::from_str::<Content>(old).expect("old form"),
        Content::ToolUse {
            signature: None,
            ..
        }
    ));
}

// --- function declarations by host ---------------------------------------

fn tool_named(name: &str, schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: "d".into(),
        input_schema: schema,
    }
}

#[tokio::test]
async fn vertex_declarations_use_parameters_and_carry_no_call_ids() {
    let server = text_server(MODEL).await;
    let adapter = vertex_gemini(&server, Arc::new(StaticBearer("tok")));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let mut request = call_history(MODEL, vec![("toolu_1", None)]);
    request.tools = vec![
        weather_tool(),
        tool_named("no_arguments", json!({"type": "object", "properties": {}})),
    ];
    adapter.complete(&request, &token).await.expect("complete");
    let sent = bodies_received(&server).await;
    let declarations = &sent[0]["tools"][0]["functionDeclarations"];
    let weather = &declarations[0];
    assert!(weather.get("parametersJsonSchema").is_none());
    assert_eq!(weather["parameters"]["type"], "OBJECT");
    assert_eq!(weather["parameters"]["required"], json!(["location"]));
    assert_eq!(
        weather["parameters"]["properties"]["location"]["type"],
        "STRING"
    );
    // The object-level `additionalProperties: false` is not part of the subset.
    assert!(weather["parameters"].get("additionalProperties").is_none());
    // A tool with no parameters sends none.
    assert!(declarations[1].get("parameters").is_none());
    assert!(declarations[1].get("parametersJsonSchema").is_none());
    // Vertex documents no id on a call or a response.
    let call = &sent[0]["contents"][1]["parts"][0]["functionCall"];
    assert!(call.get("id").is_none(), "{call}");
    let result = &sent[0]["contents"][2]["parts"][0]["functionResponse"];
    assert!(result.get("id").is_none(), "{result}");
}

#[tokio::test]
async fn the_developer_api_keeps_full_json_schema_and_call_ids() {
    let server = text_server(MODEL).await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let request = call_history(MODEL, vec![("call_1", None)]);
    adapter.complete(&request, &token).await.expect("complete");
    let sent = bodies_received(&server).await;
    let weather = &sent[0]["tools"][0]["functionDeclarations"][0];
    assert!(weather.get("parameters").is_none());
    assert_eq!(
        weather["parametersJsonSchema"]["additionalProperties"],
        false
    );
    assert_eq!(
        sent[0]["contents"][1]["parts"][0]["functionCall"]["id"],
        "call_1"
    );
}

#[tokio::test]
async fn a_schema_keyword_the_vertex_subset_cannot_express_is_refused_before_sending() {
    let server = text_server(MODEL).await;
    let adapter = vertex_gemini(&server, Arc::new(StaticBearer("tok")));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let mut request = Request::new(MODEL, "hi", 100);
    request.tools = vec![tool_named(
        "t",
        json!({"type": "object", "properties": {"a": {"oneOf": [{"type": "string"}]}}}),
    )];
    let error = adapter
        .complete(&request, &token)
        .await
        .expect_err("must refuse");
    let ProviderError::Unsent { reason, .. } = &error else {
        panic!("{error:?}");
    };
    assert!(
        reason.contains("oneOf") && reason.contains("`t`"),
        "{reason}"
    );
    assert!(bodies_received(&server).await.is_empty(), "nothing sent");
}

#[tokio::test]
async fn function_names_are_limited_per_endpoint() {
    let name = "n".repeat(100);
    // 100 characters: within the Developer API limit, over the Vertex one.
    let server = text_server(MODEL).await;
    let mut request = Request::new(MODEL, "hi", 100);
    request.tools = vec![tool_named(&name, json!({"type": "object"}))];
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    adapter.complete(&request, &token).await.expect("developer");

    let adapter = vertex_gemini(&server, Arc::new(StaticBearer("tok")));
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&request, &token)
        .await
        .expect_err("vertex refuses");
    assert!(matches!(error, ProviderError::Unsent { .. }), "{error:?}");
    assert_eq!(
        bodies_received(&server).await.len(),
        1,
        "only the first sent"
    );

    // 129 characters is over both.
    request.tools = vec![tool_named(&"n".repeat(129), json!({"type": "object"}))];
    let adapter = gemini(&server);
    let token = begin(&ledger, &adapter, MODEL);
    assert!(adapter.complete(&request, &token).await.is_err());
}

// --- thinking control ------------------------------------------------------

async fn thinking_outcome(
    model: &str,
    effort: ReasoningEffort,
    max_output: u32,
) -> Result<Value, ProviderError> {
    let server = text_server(model).await;
    let adapter = gemini_for(&server, model);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, model);
    let mut request = Request::new(model, "hi", max_output);
    request.reasoning_effort = Some(effort);
    adapter.complete(&request, &token).await?;
    Ok(bodies_received(&server).await[0]["generationConfig"]["thinkingConfig"].clone())
}

#[tokio::test]
async fn a_level_the_model_does_not_document_is_refused_not_substituted() {
    // gemini-3-pro-preview documents low and high only.
    let error = thinking_outcome("gemini-3-pro-preview", ReasoningEffort::Medium, 4000)
        .await
        .expect_err("medium is not offered");
    assert!(
        matches!(error, ProviderError::CapabilityNotHonored { .. }),
        "{error:?}"
    );
    let low = thinking_outcome("gemini-3-pro-preview", ReasoningEffort::Low, 4000)
        .await
        .expect("low");
    assert_eq!(low["thinkingLevel"], "low");
    // A model that documents medium takes it.
    let medium = thinking_outcome("gemini-3-flash-preview", ReasoningEffort::Medium, 4000)
        .await
        .expect("medium");
    assert_eq!(medium["thinkingLevel"], "medium");
}

#[tokio::test]
async fn an_unknown_model_family_is_not_assumed_to_take_a_budget() {
    let error = thinking_outcome("gemini-9-ultra", ReasoningEffort::Low, 4000)
        .await
        .expect_err("unknown family");
    assert!(
        matches!(error, ProviderError::CapabilityNotHonored { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_thinking_budget_leaves_room_for_the_answer() {
    // High asks for 24576; the ceiling is 4000, so the budget is capped at half.
    let config = thinking_outcome(MODEL, ReasoningEffort::High, 4000)
        .await
        .expect("capped");
    assert_eq!(config["thinkingBudget"], 2000);
    // A ceiling too small to hold any useful budget is refused.
    let error = thinking_outcome(MODEL, ReasoningEffort::Low, 100)
        .await
        .expect_err("too small");
    assert!(
        matches!(error, ProviderError::CapabilityNotHonored { .. }),
        "{error:?}"
    );
}

// --- finish reasons added to the enum -------------------------------------

#[tokio::test]
async fn escalation_and_policy_limits_end_the_turn_as_not_usable() {
    for reason in ["ESCALATION", "PUP_LIMITED_DISABLED"] {
        let server = MockServer::start().await;
        let mut body = fixture(TEXT_THINKING);
        body["candidates"][0]["finishReason"] = json!(reason);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let adapter = gemini(&server);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, &adapter, MODEL);
        let error = adapter
            .complete(&Request::new(MODEL, "hi", 100), &token)
            .await
            .expect_err("must raise");
        assert!(
            matches!(&error, ProviderError::TurnNotUsable { stop_reason, .. } if stop_reason == reason),
            "{reason}: {error:?}"
        );
    }
}

// --- the echoed model ------------------------------------------------------

async fn echo_outcome(requested: &str, returned: &str) -> Result<(), ProviderError> {
    let server = text_server(returned).await;
    let adapter = gemini_for(&server, requested);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, requested);
    adapter
        .complete(&Request::new(requested, "hi", 100), &token)
        .await
        .map(|_| ())
}

#[tokio::test]
async fn an_alias_answered_with_its_versioned_id_is_the_same_model() {
    echo_outcome("gemini-2.0-flash", "gemini-2.0-flash-001")
        .await
        .expect("versioned");
    echo_outcome("gemini-2.5-flash", "gemini-2.5-flash-05-20")
        .await
        .expect("dated");
}

#[tokio::test]
async fn a_different_model_with_the_requested_name_as_prefix_is_refused() {
    for returned in [
        "gemini-2.0-flash-lite",
        "gemini-2.0-flash-preview-05-20",
        "gemini-2.0-flash-",
        "gemini-2.0-flash-0a1",
    ] {
        let error = echo_outcome("gemini-2.0-flash", returned)
            .await
            .expect_err(returned);
        assert!(
            matches!(error, ProviderError::ModelSubstituted { .. }),
            "{returned}: {error:?}"
        );
    }
}

// --- streamed usage --------------------------------------------------------

#[tokio::test]
async fn a_later_usage_report_without_a_prompt_count_keeps_the_earlier_one() {
    // A stream whose last report restates only the totals that changed.
    let body = "\
: Shaped after the streamGenerateContent reference (https://ai.google.dev/api/generate-content#method:-models.streamgeneratecontent).
: A later usageMetadata omits fields an earlier one carried. Not recorded from a live call.

data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hi\"}]},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11},\"modelVersion\":\"gemini-2.5-flash\",\"responseId\":\"r1\"}

data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"!\"}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"candidatesTokenCount\":2,\"totalTokenCount\":12},\"modelVersion\":\"gemini-2.5-flash\",\"responseId\":\"r1\"}

";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(body))
        .mount(&server)
        .await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    let response = adapter
        .stream(&Request::new(MODEL, "hi", 100), &token, &mut |_| {})
        .await
        .expect("stream");
    assert_eq!(response.usage.uncached_input, 10);
    assert_eq!(response.usage.text_output, 2);
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("row")
            .settlement,
        Settlement::Counted
    );
}

// --- Claude on Vertex ------------------------------------------------------

#[tokio::test]
async fn vertex_claude_errors_name_the_vertex_adapter() {
    let server = MockServer::start().await;
    let mut body = fixture(VERTEX_MESSAGE);
    body.as_object_mut().expect("object").remove("usage");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = vertex_claude(&server, Arc::new(StaticBearer("tok")));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, CLAUDE);
    let error = adapter
        .complete(&Request::new(CLAUDE, "hi", 100), &token)
        .await
        .expect_err("no usage");
    assert!(
        matches!(&error, ProviderError::UsageMissing { adapter } if *adapter == VERTEX_ANTHROPIC_ADAPTER),
        "{error:?}"
    );
}

#[tokio::test]
async fn vertex_claude_accepts_an_alias_answered_with_its_dated_id() {
    let server = MockServer::start().await;
    let mut body = fixture(VERTEX_MESSAGE);
    body["model"] = json!("claude-sonnet-4-5-20250929");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let config = ProviderConfig::local(
        format!("{}{VERTEX_CLAUDE_ROOT}", server.uri()),
        vec!["claude-sonnet-4-5".into()],
        Protocol::Additive,
    );
    let adapter = vertex_anthropic_at(config, Arc::new(StaticBearer("tok"))).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "claude-sonnet-4-5");
    adapter
        .complete(&Request::new("claude-sonnet-4-5", "hi", 100), &token)
        .await
        .expect("alias resolved to a dated id");
}

// --- signature producer ------------------------------------------------------

/// A token is valid only for the model that made it. One made by another
/// adapter or model is not sent; on a model that validates signatures the
/// documented skip value takes its place, and elsewhere nothing is sent.
#[tokio::test]
async fn a_signature_from_another_model_or_host_is_not_sent() {
    let model = "gemini-3-pro-preview";
    let server = text_server(model).await;
    let mut request = call_history(model, vec![("c1", Some("c2ln"))]);
    for foreign in [
        "vertex-generate-content:gemini-3-pro-preview",
        "gemini-generate-content:gemini-3-flash-preview",
    ] {
        if let Some(Content::ToolUse { signature, .. }) = request.messages[1].content.first_mut() {
            *signature = Some(Signature::new("c2ln", foreign));
        }
        let contents = sent_contents(&server, model, &request).await;
        let sent = &contents[1]["parts"][0]["thoughtSignature"];
        assert_eq!(sent, SKIP_SIGNATURE, "{foreign}");
    }
    // A token whose maker was never recorded matches nothing either.
    if let Some(Content::ToolUse { signature, .. }) = request.messages[1].content.first_mut() {
        *signature = Some("c2ln".into());
    }
    let contents = sent_contents(&server, model, &request).await;
    assert_eq!(contents[1]["parts"][0]["thoughtSignature"], SKIP_SIGNATURE);

    // A model that does not validate gets nothing.
    let other = text_server(MODEL).await;
    let mut request = call_history(MODEL, vec![("c1", None)]);
    if let Some(Content::ToolUse { signature, .. }) = request.messages[1].content.first_mut() {
        *signature = Some(Signature::new("c2ln", producer("gemini-3-pro-preview")));
    }
    let contents = sent_contents(&other, MODEL, &request).await;
    assert!(contents[1]["parts"][0].get("thoughtSignature").is_none());
}

// --- text-part signatures ------------------------------------------------------

async fn signed_text_response(parts: Value) -> kgraf_provider::Response {
    let model = "gemini-3-flash-preview";
    let server = MockServer::start().await;
    let mut body = fixture(TEXT_THINKING);
    body["modelVersion"] = json!(model);
    body["candidates"][0]["content"]["parts"] = parts;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = gemini_for(&server, model);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, model);
    adapter
        .complete(&Request::new(model, "hi", 100), &token)
        .await
        .expect("complete")
}

fn text_signatures(response: &kgraf_provider::Response) -> Vec<(String, Option<String>)> {
    response
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text {
                text, signature, ..
            } => Some((text.clone(), signature.as_ref().map(|s| s.value.clone()))),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_signature_on_a_text_part_is_kept_and_tagged() {
    let response =
        signed_text_response(json!([{ "text": "The answer.", "thoughtSignature": "dGV4dA==" }]))
            .await;
    assert_eq!(
        text_signatures(&response),
        vec![("The answer.".into(), Some("dGV4dA==".into()))]
    );
    let Some(Content::Text {
        signature: Some(signature),
        ..
    }) = response.content.first()
    else {
        panic!("{:?}", response.content);
    };
    assert!(signature.is_from("gemini-generate-content:gemini-3-flash-preview"));
}

#[tokio::test]
async fn a_trailing_signature_on_an_empty_part_joins_the_last_text_block() {
    let response = signed_text_response(json!([
        { "text": "First." },
        { "text": "", "thoughtSignature": "dHJhaWw=" }
    ]))
    .await;
    assert_eq!(
        text_signatures(&response),
        vec![("First.".into(), Some("dHJhaWw=".into()))]
    );
    // A part with the token and no `text` key at all trails the same way.
    let bare = signed_text_response(json!([
        { "text": "First." },
        { "thoughtSignature": "YmFyZQ==" }
    ]))
    .await;
    assert_eq!(
        text_signatures(&bare),
        vec![("First.".into(), Some("YmFyZQ==".into()))]
    );
    // With no text at all, the token rides on an empty text block.
    let only = signed_text_response(json!([{ "text": "", "thoughtSignature": "b25seQ==" }])).await;
    assert_eq!(
        text_signatures(&only),
        vec![(String::new(), Some("b25seQ==".into()))]
    );
}

#[tokio::test]
async fn a_signed_text_part_is_never_merged_with_an_unsigned_one() {
    let response = signed_text_response(json!([
        { "text": "one " },
        { "text": "two ", "thoughtSignature": "czI=" },
        { "text": "three" }
    ]))
    .await;
    assert_eq!(
        text_signatures(&response),
        vec![
            ("one ".into(), None),
            ("two ".into(), Some("czI=".into())),
            ("three".into(), None)
        ]
    );
    assert_eq!(response.text(), "one two three");
}

#[tokio::test]
async fn a_thought_summary_signature_is_not_kept_and_does_not_sign_the_text() {
    let response = signed_text_response(json!([
        { "text": "thinking", "thought": true, "thoughtSignature": "dGg=" },
        { "text": "answer" }
    ]))
    .await;
    assert_eq!(text_signatures(&response), vec![("answer".into(), None)]);
}

#[tokio::test]
async fn a_signed_text_block_goes_back_as_a_signed_part_only_to_its_maker() {
    let model = "gemini-3-flash-preview";
    let server = text_server(model).await;
    let mut request = Request::new(model, "hi", 100);
    let foreign = || Some(Signature::new("Zm9y", "vertex-generate-content:other"));
    request.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            Content::Text {
                text: "signed".into(),
                cache_breakpoint: false,
                signature: Some(Signature::new("c2ln", producer(model))),
            },
            Content::text("plain"),
            Content::Text {
                text: String::new(),
                cache_breakpoint: false,
                signature: Some(Signature::new("ZW1wdHk=", producer(model))),
            },
            Content::Text {
                text: "foreign".into(),
                cache_breakpoint: false,
                signature: foreign(),
            },
            Content::Text {
                text: String::new(),
                cache_breakpoint: false,
                signature: foreign(),
            },
        ],
    });
    request.messages.push(Message::user("next"));
    let contents = sent_contents(&server, model, &request).await;
    let parts = contents[1]["parts"].as_array().expect("parts");
    assert_eq!(
        parts,
        &vec![
            json!({"text": "signed", "thoughtSignature": "c2ln"}),
            json!({"text": "plain"}),
            json!({"text": "", "thoughtSignature": "ZW1wdHk="}),
            json!({"text": "foreign"}),
        ]
    );
}

#[tokio::test]
async fn a_streamed_signature_after_the_text_joins_the_last_text_block() {
    let model = "gemini-3-flash-preview";
    let stream = format!(
        ": https://ai.google.dev/gemini-api/docs/thought-signatures\n\n\
         data: {}\n\ndata: {}\n\n",
        json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Hel"}]},"index":0}],
               "usageMetadata":{"promptTokenCount":4},"modelVersion":model,"responseId":"r"}),
        json!({"candidates":[{"content":{"role":"model",
               "parts":[{"text":"","thoughtSignature":"c3Ry"}]},
               "finishReason":"STOP","index":0}],
               "usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6},
               "modelVersion":model,"responseId":"r"}),
    );
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&stream))
        .mount(&server)
        .await;
    let adapter = gemini_for(&server, model);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, model);
    let response = adapter
        .stream(&Request::new(model, "hi", 100), &token, &mut |_| {})
        .await
        .expect("stream");
    assert_eq!(
        text_signatures(&response),
        vec![("Hel".into(), Some("c3Ry".into()))]
    );
}

// --- request checks --------------------------------------------------------

#[tokio::test]
async fn a_latest_alias_is_refused_before_sending() {
    let server = text_server("gemini-flash-latest").await;
    let config = ProviderConfig::new(
        GEMINI_ADAPTER,
        format!("{}/v1beta", server.uri()),
        "test-key",
        vec!["gemini-flash-latest".into()],
        Protocol::Inclusive,
    )
    .expect("config");
    let adapter = GeminiAdapter::api_key(config).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "gemini-flash-latest");
    let error = adapter
        .complete(&Request::new("gemini-flash-latest", "hi", 100), &token)
        .await
        .expect_err("must refuse");
    let ProviderError::ModelUnsupported { available, .. } = &error else {
        panic!("{error:?}");
    };
    assert!(available.contains("concrete"), "{available}");
    assert!(bodies_received(&server).await.is_empty(), "nothing sent");
}

#[tokio::test]
async fn a_function_name_the_service_would_reject_is_refused_in_tools_and_history() {
    let server = text_server(MODEL).await;
    let adapter = gemini(&server);
    let ledger = Ledger::open_in_memory().expect("ledger");

    let mut request = Request::new(MODEL, "hi", 100);
    request.tools = vec![tool_named("9lives", json!({"type": "object"}))];
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter.complete(&request, &token).await.expect_err("tool");
    assert!(
        matches!(&error, ProviderError::Unsent { reason, .. } if reason.contains("9lives")),
        "{error:?}"
    );

    let mut request = call_history(MODEL, vec![("c1", None)]);
    if let Some(Content::ToolUse { name, .. }) = request.messages[1].content.first_mut() {
        *name = "bad name".into();
    }
    request.tools.clear();
    let token = begin(&ledger, &adapter, MODEL);
    let error = adapter
        .complete(&request, &token)
        .await
        .expect_err("history");
    assert!(
        matches!(&error, ProviderError::Unsent { reason, .. } if reason.contains("bad name")),
        "{error:?}"
    );
    assert!(bodies_received(&server).await.is_empty(), "nothing sent");
}

#[tokio::test]
async fn the_minimum_thinking_budget_of_flash_lite_is_512() {
    // Half a 1000-token ceiling is 500: under Flash-Lite's 512, over Flash's floor.
    let error = thinking_outcome("gemini-2.5-flash-lite", ReasoningEffort::High, 1000)
        .await
        .expect_err("under the minimum");
    assert!(
        matches!(error, ProviderError::CapabilityNotHonored { .. }),
        "{error:?}"
    );
    let config = thinking_outcome("gemini-2.5-flash-lite", ReasoningEffort::High, 1024)
        .await
        .expect("512 is the floor");
    assert_eq!(config["thinkingBudget"], 512);
    let config = thinking_outcome("gemini-2.5-flash", ReasoningEffort::High, 1000)
        .await
        .expect("500 is fine on Flash");
    assert_eq!(config["thinkingBudget"], 500);
}

#[tokio::test]
async fn the_image_model_levels_are_documented_ones() {
    let ok = thinking_outcome("gemini-3-pro-image-preview", ReasoningEffort::High, 4000)
        .await
        .expect("high");
    assert_eq!(ok["thinkingLevel"], "high");
    let error = thinking_outcome("gemini-3-pro-image-preview", ReasoningEffort::Low, 4000)
        .await
        .expect_err("high only");
    assert!(
        matches!(error, ProviderError::CapabilityNotHonored { .. }),
        "{error:?}"
    );
    let medium = thinking_outcome("gemini-3.1-flash-lite", ReasoningEffort::Medium, 4000)
        .await
        .expect("flash-lite documents medium");
    assert_eq!(medium["thinkingLevel"], "medium");
}

// --- quota project ---------------------------------------------------------

struct UserBearer;

#[async_trait::async_trait]
impl BearerSource for UserBearer {
    async fn bearer(&self) -> Result<String, ProviderError> {
        Ok("ya29.user".to_owned())
    }

    fn quota_project(&self) -> Option<String> {
        Some("billing-project".to_owned())
    }
}

#[tokio::test]
async fn a_user_credential_sends_its_quota_project_on_both_vertex_routes() {
    let server = text_server(MODEL).await;
    let adapter = vertex_gemini(&server, Arc::new(UserBearer));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect("complete");
    let requests = server.received_requests().await.expect("on");
    assert_eq!(
        requests[0]
            .headers
            .get("x-goog-user-project")
            .map(|v| v.to_str().expect("ascii")),
        Some("billing-project")
    );

    let claude = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(VERTEX_MESSAGE)))
        .mount(&claude)
        .await;
    let adapter = vertex_claude(&claude, Arc::new(UserBearer));
    let token = begin(&ledger, &adapter, CLAUDE);
    adapter
        .complete(&Request::new(CLAUDE, "hi", 100), &token)
        .await
        .expect("complete");
    let requests = claude.received_requests().await.expect("on");
    assert_eq!(
        requests[0]
            .headers
            .get("x-goog-user-project")
            .map(|v| v.to_str().expect("ascii")),
        Some("billing-project")
    );
}

#[tokio::test]
async fn a_credential_with_no_quota_project_sends_no_header() {
    let server = text_server(MODEL).await;
    let adapter = vertex_gemini(&server, Arc::new(StaticBearer("tok")));
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, MODEL);
    adapter
        .complete(&Request::new(MODEL, "hi", 100), &token)
        .await
        .expect("complete");
    let requests = server.received_requests().await.expect("on");
    assert!(requests[0].headers.get("x-goog-user-project").is_none());
}
