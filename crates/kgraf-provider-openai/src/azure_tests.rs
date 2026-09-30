//! Tests for the Azure OpenAI transport option, over the recorded-shape
//! fixtures in `tests/fixtures/azure/`. Each fixture's header names the
//! Microsoft page its shape comes from. Nothing here has run against the
//! live service.

use kgraf_core::Protocol;
use kgraf_provider::capability::Streaming;
use kgraf_provider::{Ledger, Provider, ProviderConfig, ProviderError, Request, Settlement};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::azure::{
    API_KEY_VARIABLE, API_VERSION_VARIABLE, ApiVersion, AzureSettings, AzureSettingsError,
    AzureTransport, CHAT_ADAPTER, DEPLOYMENTS_VARIABLE, RESOURCE_VARIABLE, RESPONSES_ADAPTER,
};
use crate::{ChatCompletionsAdapter, OpenAiAdapter, UsageDialect};

const CHAT_COMPLETION: &str =
    include_str!("../tests/fixtures/azure/chat-completion-annotated.json");
const CHAT_FILTERED: &str = include_str!("../tests/fixtures/azure/chat-finish-content-filter.json");
const CHAT_PROMPT_FILTERED: &str =
    include_str!("../tests/fixtures/azure/chat-prompt-filtered.json");
const PROMPT_BLOCKED: &str = include_str!("../tests/fixtures/azure/error-prompt-blocked.json");
const CHAT_STREAM: &str = include_str!("../tests/fixtures/azure/chat-stream.sse");
const CHAT_STREAM_FILTERED: &str = include_str!("../tests/fixtures/azure/chat-stream-filtered.sse");
const RESPONSES_COMPLETED: &str = include_str!("../tests/fixtures/azure/responses-completed.json");
const RESPONSES_BLOCKED: &str = include_str!("../tests/fixtures/azure/responses-blocked.json");

const KEY: &str = "0123456789abcdef0123456789abcdef";
const CHAT_MODEL: &str = "gpt-35-turbo-instruct";
const RESPONSES_MODEL: &str = "gpt-4.1-nano-2025-04-14";

fn settings(version: &str, deployments: &str) -> AzureSettings {
    AzureSettings::new(KEY, "my-resource", version, deployments).expect("settings")
}

fn chat_deployments() -> String {
    format!("chat-dep={CHAT_MODEL}")
}

fn responses_deployments() -> String {
    format!("resp-dep={RESPONSES_MODEL}")
}

fn config(server: &MockServer, model: &str) -> ProviderConfig {
    ProviderConfig::new(
        CHAT_ADAPTER,
        server.uri(),
        KEY,
        vec![model.to_owned()],
        Protocol::Inclusive,
    )
    .expect("config")
}

fn chat_adapter(server: &MockServer, version: &str) -> ChatCompletionsAdapter {
    let settings = settings(version, &chat_deployments());
    ChatCompletionsAdapter::new(config(server, "chat-dep"), UsageDialect::Details)
        .expect("adapter")
        .with_azure(
            AzureTransport::chat(&settings)
                .expect("transport")
                .with_origin(server.uri()),
        )
        .expect("azure")
}

fn responses_adapter(server: &MockServer, version: &str) -> OpenAiAdapter {
    let settings = settings(version, &responses_deployments());
    OpenAiAdapter::new(config(server, "resp-dep"))
        .expect("adapter")
        .with_azure(
            AzureTransport::responses(&settings)
                .expect("transport")
                .with_origin(server.uri()),
        )
        .expect("azure")
}

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture parses")
}

fn begin<'a>(
    ledger: &'a Ledger,
    adapter: &dyn Provider,
    model: &str,
) -> kgraf_provider::CallToken<'a> {
    ledger
        .begin(adapter.adapter(), Protocol::Inclusive, model, "w", "t")
        .expect("begin")
}

async fn only_request(server: &MockServer) -> wiremock::Request {
    let mut requests = server.received_requests().await.expect("recording on");
    assert_eq!(requests.len(), 1, "exactly one request expected");
    requests.remove(0)
}

// ----- settings -----

#[test]
fn a_resource_that_is_not_a_bare_dns_label_is_refused_before_it_reaches_a_host() {
    for bad in [
        "",
        "a",
        "-abc",
        "abc-",
        "a b",
        "evil.example/x",
        "res.openai.azure.com",
        "res?x=1",
        "res@evil.example",
        "res#frag",
        "https://res",
        "res\nHost: evil",
        &"a".repeat(65),
    ] {
        let err = AzureSettings::new(KEY, bad, "v1", "d=m").expect_err(bad);
        assert_eq!(err.variable(), RESOURCE_VARIABLE, "{bad:?}");
    }
    let ok = AzureSettings::new(KEY, " My-Res-1 ", "v1", "d=m").expect("valid");
    assert_eq!(ok.origin(), "https://my-res-1.openai.azure.com");
}

#[test]
fn an_api_version_is_v1_or_a_calendar_date_and_nothing_else() {
    assert_eq!(ApiVersion::parse("v1").expect("v1"), ApiVersion::V1);
    for good in ["2024-10-21", "2025-04-01-preview", " 2024-06-01 "] {
        assert!(
            matches!(ApiVersion::parse(good), Ok(ApiVersion::Dated(_))),
            "{good}"
        );
    }
    for bad in [
        "",
        "latest",
        "2024-13-01",
        "2024-00-10",
        "2024-10-32",
        "2024-10-21-beta",
        "2024-10-21&x=1",
        "2024-10-21\r\nX: y",
        "24-10-21",
        "v2",
    ] {
        let err = ApiVersion::parse(bad).expect_err(bad);
        assert_eq!(err.variable(), API_VERSION_VARIABLE, "{bad:?}");
    }
}

#[test]
fn the_deployment_mapping_is_validated_and_never_empty() {
    for bad in [
        "no-equals",
        "a=",
        "=m",
        "a b=m",
        "a/b=m",
        "a=m,a=n",
        "a=m n",
    ] {
        let err = AzureSettings::new(KEY, "res-1", "v1", bad).expect_err(bad);
        assert_eq!(err.variable(), DEPLOYMENTS_VARIABLE, "{bad:?}");
    }
    assert_eq!(
        AzureSettings::new(KEY, "res-1", "v1", " , ").expect_err("empty"),
        AzureSettingsError::Missing {
            variable: DEPLOYMENTS_VARIABLE
        }
    );
    let ok = AzureSettings::new(
        KEY,
        "res-1",
        "v1",
        "prod.1=gpt-4o-2024-08-06, mini_2=gpt-4o-mini",
    )
    .expect("valid");
    assert_eq!(ok.deployments().len(), 2);
}

#[test]
fn settings_are_read_from_the_lookup_and_a_missing_variable_is_named() {
    let vars: std::collections::HashMap<&str, &str> = [
        (API_KEY_VARIABLE, KEY),
        (RESOURCE_VARIABLE, "res-1"),
        (API_VERSION_VARIABLE, "2024-10-21"),
        (DEPLOYMENTS_VARIABLE, "d=m"),
    ]
    .into_iter()
    .collect();
    let lookup = |name: &str| vars.get(name).map(|v| (*v).to_owned());
    AzureSettings::from_lookup(&lookup).expect("complete");
    for missing in [
        API_KEY_VARIABLE,
        RESOURCE_VARIABLE,
        API_VERSION_VARIABLE,
        DEPLOYMENTS_VARIABLE,
    ] {
        let without = |name: &str| {
            (name != missing)
                .then(|| vars.get(name).map(|v| (*v).to_owned()))
                .flatten()
        };
        let err = AzureSettings::from_lookup(&without).expect_err(missing);
        assert_eq!(err.variable(), missing);
    }
}

#[test]
fn the_credential_string_round_trips_and_no_debug_output_carries_the_key() {
    let original = settings("2024-10-21", "a=model-a,b=model-b");
    let back = AzureSettings::from_credential(&original.to_credential()).expect("round trip");
    assert_eq!(back, original);
    assert_eq!(back.api_key(), KEY);
    assert!(!format!("{original:?}").contains(KEY));
    assert!(AzureSettings::from_credential("only-a-key").is_err());
    // A key with whitespace would break the newline framing and a header value.
    for bad in ["two words", "line\nbreak", "tab\tkey", ""] {
        assert!(
            AzureSettings::new(bad, "res-1", "v1", "d=m").is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn each_surface_has_its_documented_address() {
    let dated = settings("2024-10-21", "d=m");
    let v1 = settings("v1", "d=m");
    let origin = "https://my-resource.openai.azure.com";

    let chat = AzureTransport::chat(&dated).expect("dated chat");
    assert_eq!(
        chat.url("d"),
        format!("{origin}/openai/deployments/d/chat/completions")
    );
    assert_eq!(chat.query(), Some(("api-version", "2024-10-21")));
    assert!(!chat.body_names_model(), "the deployment is in the URL");

    let chat_v1 = AzureTransport::chat(&v1).expect("v1 chat");
    assert_eq!(
        chat_v1.url("d"),
        format!("{origin}/openai/v1/chat/completions")
    );
    assert_eq!(chat_v1.query(), None);
    assert!(chat_v1.body_names_model());

    let responses = AzureTransport::responses(&v1).expect("v1 responses");
    assert!(responses.body_names_model());

    let responses_v1 = AzureTransport::responses(&v1).expect("v1 responses");
    assert_eq!(
        responses_v1.url("d"),
        format!("{origin}/openai/v1/responses")
    );
    assert_eq!(responses_v1.query(), None);

    assert_eq!(chat.name(), CHAT_ADAPTER);
    assert_eq!(responses.name(), RESPONSES_ADAPTER);
}

#[tokio::test]
async fn a_deployment_with_no_mapped_model_is_refused_when_the_adapter_is_built() {
    let server = MockServer::start().await;
    let settings = settings("v1", "mapped=some-model");
    let unmapped = ProviderConfig::new(
        CHAT_ADAPTER,
        server.uri(),
        KEY,
        vec!["mapped".to_owned(), "unmapped".to_owned()],
        Protocol::Inclusive,
    )
    .expect("config");
    let result = ChatCompletionsAdapter::new(unmapped, UsageDialect::Details)
        .expect("adapter")
        .with_azure(AzureTransport::chat(&settings).expect("transport"));
    match result {
        Err(ProviderError::ModelUnsupported {
            model, available, ..
        }) => {
            assert_eq!(model, "unmapped");
            assert_eq!(available, "mapped");
        }
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("an unmapped deployment must not build"),
    }
}

// ----- Chat Completions -----

#[tokio::test]
async fn a_dated_chat_call_goes_to_the_deployment_url_with_the_api_key_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/deployments/chat-dep/chat/completions"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(CHAT_COMPLETION)))
        .expect(1)
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "2024-10-21");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let response = adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect("complete");

    assert_eq!(
        response.model, CHAT_MODEL,
        "the response names the underlying model"
    );
    assert_eq!(response.text(), "Example model response will be returned ");
    // Inclusive: 64 cached of 100 prompt tokens, 12 reasoning of 40 completion.
    assert_eq!(response.usage.cache_read, 64);
    assert_eq!(response.usage.uncached_input, 36);
    assert_eq!(response.usage.reasoning_output, 12);
    assert_eq!(response.usage.text_output, 28);
    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(record.adapter, CHAT_ADAPTER);

    let request = only_request(&server).await;
    assert!(
        request.headers.get("authorization").is_none(),
        "no bearer header"
    );
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert!(
        body.get("model").is_none(),
        "the deployment is in the URL only"
    );
    assert_eq!(body["max_completion_tokens"], 100);
    assert!(
        body.get("max_tokens").is_none(),
        "reasoning deployments refuse max_tokens"
    );
}

#[tokio::test]
async fn a_v1_chat_call_names_the_deployment_in_the_body_and_sends_no_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("api-key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(CHAT_COMPLETION)))
        .expect(1)
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect("complete");
    let request = only_request(&server).await;
    assert_eq!(
        request.url.query(),
        None,
        "no api-version on the v1 surface"
    );
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["model"], "chat-dep");
}

#[tokio::test]
async fn a_response_naming_another_model_than_the_mapped_one_raises_and_keeps_the_spend() {
    let server = MockServer::start().await;
    let mut body = fixture(CHAT_COMPLETION);
    body["model"] = json!("gpt-4o-mini");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "2024-10-21");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    match err {
        ProviderError::ModelSubstituted {
            requested,
            returned,
        } => {
            assert_eq!(requested, CHAT_MODEL, "the check runs against the mapping");
            assert_eq!(returned, "gpt-4o-mini");
        }
        other => panic!("wrong error: {other:?}"),
    }
    let open = ledger.unreconciled().expect("query");
    assert_eq!(open.len(), 1, "the billed call stays visible");
    assert_eq!(open[0].usage.total_input(), 100);
}

#[tokio::test]
async fn the_deployment_name_is_not_accepted_as_the_response_model() {
    // A response that echoes the deployment instead of the mapped model is a
    // mismatch: the mapping is the only accepted answer.
    let server = MockServer::start().await;
    let mut body = fixture(CHAT_COMPLETION);
    body["model"] = json!("chat-dep");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(
        matches!(err, ProviderError::ModelSubstituted { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_filtered_completion_settles_the_call_and_raises_turn_not_usable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(CHAT_FILTERED)))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "2024-10-21");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    match err {
        ProviderError::TurnNotUsable {
            adapter,
            stop_reason,
        } => {
            assert_eq!(adapter, CHAT_ADAPTER);
            assert_eq!(stop_reason, "content_filter");
        }
        other => panic!("wrong error: {other:?}"),
    }
    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted, "the call billed");
    assert_eq!(record.usage.total_input(), 20);
}

#[tokio::test]
async fn a_filtered_prompt_in_a_completed_response_raises_turn_not_usable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(CHAT_PROMPT_FILTERED)))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "2024-10-21");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    match err {
        ProviderError::TurnNotUsable { stop_reason, .. } => {
            assert_eq!(stop_reason, "prompt_filter_results");
        }
        other => panic!("wrong error: {other:?}"),
    }
    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted);
}

#[tokio::test]
async fn unfiltered_annotations_do_not_make_a_turn_unusable() {
    // Every category is present with `filtered: false` on a normal response.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(CHAT_COMPLETION)))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect("an annotated, unfiltered response is an answer");
}

#[tokio::test]
async fn a_blocked_prompt_is_an_http_error_that_billed_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(PROMPT_BLOCKED))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "2024-10-21");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    match err {
        ProviderError::Http { status, body, .. } => {
            assert_eq!(status, 400);
            assert!(body.contains("content_filter"));
        }
        other => panic!("wrong error: {other:?}"),
    }
    drop(token);
    assert_eq!(
        ledger.calls().expect("calls")[0].settlement,
        Settlement::Failed
    );
}

#[tokio::test]
async fn a_stream_with_an_empty_opening_chunk_is_counted_under_the_mapped_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/deployments/chat-dep/chat/completions"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAT_STREAM))
        .expect(1)
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "2024-10-21");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let response = Streaming::stream(
        &adapter,
        &Request::new("chat-dep", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect("stream");
    assert_eq!(response.text(), "Hello");
    assert_eq!(response.model, CHAT_MODEL);
    assert_eq!(
        response.request_id.as_deref(),
        Some("chatcmpl-stream-0001"),
        "the empty id on the opening chunk is not the request id"
    );
    assert_eq!(response.usage.total_input(), 10);
    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted);

    let body: Value = serde_json::from_slice(&only_request(&server).await.body).expect("json");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn the_empty_model_tolerance_is_azure_only() {
    // The same stream against the plain adapter still raises: an empty `model`
    // is a mismatch everywhere except on the Azure transport.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAT_STREAM))
        .mount(&server)
        .await;
    let plain_config = ProviderConfig::new(
        crate::chat::ADAPTER,
        server.uri(),
        KEY,
        vec![CHAT_MODEL.to_owned()],
        Protocol::Inclusive,
    )
    .expect("config");
    let adapter =
        ChatCompletionsAdapter::new(plain_config, UsageDialect::Details).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, CHAT_MODEL);
    let err = Streaming::stream(
        &adapter,
        &Request::new(CHAT_MODEL, "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect_err("must raise");
    assert!(
        matches!(err, ProviderError::ModelSubstituted { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_streamed_generation_cut_by_the_filter_settles_and_raises() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAT_STREAM_FILTERED))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = Streaming::stream(
        &adapter,
        &Request::new("chat-dep", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect_err("must raise");
    match err {
        ProviderError::TurnNotUsable { stop_reason, .. } => {
            assert_eq!(stop_reason, "content_filter")
        }
        other => panic!("wrong error: {other:?}"),
    }
    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(record.usage.total_input(), 10);
}

#[tokio::test]
async fn a_stream_naming_another_model_than_the_mapped_one_raises() {
    let server = MockServer::start().await;
    let sse = CHAT_STREAM.replace(CHAT_MODEL, "gpt-4o-mini");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = Streaming::stream(
        &adapter,
        &Request::new("chat-dep", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect_err("must raise");
    assert!(
        matches!(err, ProviderError::ModelSubstituted { .. }),
        "{err:?}"
    );
}

// ----- Responses -----

#[test]
fn a_dated_version_is_refused_for_responses_because_only_the_v1_path_is_documented() {
    let dated = settings("2025-04-01-preview", "resp-dep=m");
    let err = AzureTransport::responses(&dated).expect_err("dated responses");
    assert!(
        matches!(&err, ProviderError::Unsent { adapter, reason }
            if *adapter == RESPONSES_ADAPTER && reason.contains("/openai/v1/responses")),
        "{err:?}"
    );
    assert_eq!(
        dated.check_for(true).expect_err("refused").variable(),
        API_VERSION_VARIABLE
    );
    settings("v1", "resp-dep=m")
        .check_for(true)
        .expect("v1 is fine");
}

#[tokio::test]
async fn a_v1_responses_call_uses_the_v1_path_and_no_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/responses"))
        .and(header("api-key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(RESPONSES_COMPLETED)))
        .expect(1)
        .mount(&server)
        .await;
    let adapter = responses_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "resp-dep");
    adapter
        .complete(&Request::new("resp-dep", "hi", 100), &token)
        .await
        .expect("complete");
    assert_eq!(only_request(&server).await.url.query(), None);
}

#[tokio::test]
async fn a_responses_call_naming_another_model_raises() {
    let server = MockServer::start().await;
    let mut body = fixture(RESPONSES_COMPLETED);
    body["model"] = json!("gpt-4o-mini");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = responses_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "resp-dep");
    let err = adapter
        .complete(&Request::new("resp-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(
        matches!(err, ProviderError::ModelSubstituted { .. }),
        "{err:?}"
    );
    assert_eq!(ledger.unreconciled().expect("query").len(), 1);
}

#[tokio::test]
async fn a_blocked_responses_completion_settles_the_call_and_raises() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(RESPONSES_BLOCKED)))
        .mount(&server)
        .await;
    let adapter = responses_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "resp-dep");
    let err = adapter
        .complete(&Request::new("resp-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    match err {
        ProviderError::TurnNotUsable {
            adapter,
            stop_reason,
        } => {
            assert_eq!(adapter, RESPONSES_ADAPTER);
            assert_eq!(stop_reason, "content_filter");
        }
        other => panic!("wrong error: {other:?}"),
    }
    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(record.usage.total_input(), 50);
}

fn responses_sse(terminal_type: &str, response: &Value) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type": "response.created", "response": {"id": response["id"], "model": response["model"]}}),
        json!({"type": terminal_type, "response": response}),
    )
}

#[tokio::test]
async fn a_responses_stream_ends_in_the_mapped_model_and_a_blocked_one_raises() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(responses_sse(
            "response.completed",
            &fixture(RESPONSES_COMPLETED),
        )))
        .mount(&server)
        .await;
    let adapter = responses_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "resp-dep");
    let response = Streaming::stream(
        &adapter,
        &Request::new("resp-dep", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect("stream");
    assert_eq!(response.model, RESPONSES_MODEL);
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("present")
            .settlement,
        Settlement::Counted
    );

    let blocked = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(responses_sse(
            "response.incomplete",
            &fixture(RESPONSES_BLOCKED),
        )))
        .mount(&blocked)
        .await;
    let adapter = responses_adapter(&blocked, "v1");
    let token = begin(&ledger, &adapter, "resp-dep");
    let err = Streaming::stream(
        &adapter,
        &Request::new("resp-dep", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect_err("must raise");
    assert!(
        matches!(err, ProviderError::TurnNotUsable { .. }),
        "{err:?}"
    );
    assert_eq!(
        ledger
            .call(token.id())
            .expect("read")
            .expect("present")
            .settlement,
        Settlement::Counted,
        "the blocked stream billed and settles"
    );
}

// ----- version floor, names, hosts -----

#[test]
fn a_dated_chat_version_older_than_stream_usage_is_refused() {
    for old in ["2024-06-01", "2024-08-01-preview", "2023-05-15"] {
        let dated = settings(old, "d=m");
        let err = AzureTransport::chat(&dated).expect_err(old);
        assert!(
            matches!(&err, ProviderError::Unsent { reason, .. }
                if reason.contains("2024-09-01-preview")),
            "{old}: {err:?}"
        );
        assert!(dated.check_for(false).is_err(), "{old}");
    }
    for ok in [
        "2024-09-01-preview",
        "2024-10-21",
        "2025-04-01-preview",
        "v1",
    ] {
        let dated = settings(ok, "d=m");
        AzureTransport::chat(&dated).expect(ok);
        dated.check_for(false).expect(ok);
    }
}

#[test]
fn a_dated_version_without_preview_between_the_floor_and_the_first_ga_is_refused() {
    for unpublished in ["2024-09-01", "2024-10-01", "2024-10-20"] {
        let dated = settings(unpublished, "d=m");
        let err = AzureTransport::chat(&dated).expect_err(unpublished);
        assert!(
            matches!(&err, ProviderError::Unsent { reason, .. }
                if reason.contains("not a published version")),
            "{unpublished}: {err:?}"
        );
    }
    AzureTransport::chat(&settings("2024-10-01-preview", "d=m")).expect("preview");
}

#[test]
fn a_deployment_named_with_dots_alone_is_refused() {
    for bad in [".", "..", "...", ".=m", "..=m"] {
        let err = AzureSettings::new(KEY, "res-1", "v1", &format!("{bad}=m"))
            .or_else(|_| AzureSettings::new(KEY, "res-1", "v1", bad));
        assert!(err.is_err(), "{bad:?}");
    }
    AzureSettings::new(KEY, "res-1", "v1", "a.b=m").expect("dots inside a name are fine");
}

#[test]
fn a_foundry_host_is_named_as_unsupported_not_as_a_bad_url() {
    for host in [
        "my-project.services.ai.azure.com",
        "https://my-project.services.ai.azure.com/api/projects/p",
        "res.cognitiveservices.azure.com",
    ] {
        let err = AzureSettings::new(KEY, host, "v1", "d=m").expect_err(host);
        assert_eq!(err.variable(), RESOURCE_VARIABLE);
        let text = err.to_string();
        assert!(text.contains("not supported"), "{text}");
        assert!(!text.contains("not a URL"), "{text}");
    }
}

// ----- filter errors, errors' adapter names, limits -----

#[test]
fn a_filter_that_did_not_run_is_reported() {
    // https://learn.microsoft.com/en-us/azure/ai-foundry/openai/concepts/content-filter
    // ("If the content filtering system is down ... an error message in the
    // content_filter_results object").
    let body = json!({
        "choices": [{
            "finish_reason": "stop",
            "content_filter_results": {
                "error": {"code": "content_filter_error", "message": "The contents are not filtered"}
            }
        }]
    });
    assert_eq!(
        crate::azure::chat_filter(&body),
        Some("content_filter_results_error")
    );
    let prompt = json!({
        "prompt_filter_results": [{
            "prompt_index": 0,
            "content_filter_results": {"error": {"code": "content_filter_error", "message": "x"}}
        }],
        "choices": []
    });
    assert_eq!(
        crate::azure::chat_filter(&prompt),
        Some("content_filter_results_error")
    );
    assert_eq!(crate::azure::chat_filter(&fixture(CHAT_COMPLETION)), None);
}

#[tokio::test]
async fn an_unfiltered_completion_whose_filters_did_not_run_settles_and_raises() {
    let server = MockServer::start().await;
    let mut body = fixture(CHAT_COMPLETION);
    body["choices"][0]["content_filter_results"] = json!({
        "error": {"code": "content_filter_error", "message": "The contents are not filtered"}
    });
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("must raise");
    assert!(
        matches!(&err, ProviderError::TurnNotUsable { stop_reason, .. }
            if stop_reason == "content_filter_results_error"),
        "{err:?}"
    );
    let record = ledger.call(token.id()).expect("read").expect("present");
    assert_eq!(record.settlement, Settlement::Counted);
}

#[tokio::test]
async fn errors_name_the_azure_adapter_not_the_plain_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(PROMPT_BLOCKED))
        .mount(&server)
        .await;
    let chat = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &chat, "chat-dep");
    let err = chat
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("400");
    assert!(
        matches!(&err, ProviderError::Http { adapter, .. } if *adapter == CHAT_ADAPTER),
        "{err:?}"
    );
    let token = begin(&ledger, &chat, "not-mapped");
    let err = chat
        .complete(&Request::new("not-mapped", "hi", 100), &token)
        .await
        .expect_err("unmapped");
    assert!(
        matches!(&err, ProviderError::ModelUnsupported { adapter, .. } if *adapter == CHAT_ADAPTER),
        "{err:?}"
    );

    let responses = responses_adapter(&server, "v1");
    let token = begin(&ledger, &responses, "resp-dep");
    let err = responses
        .complete(&Request::new("resp-dep", "hi", 100), &token)
        .await
        .expect_err("400");
    assert!(
        matches!(&err, ProviderError::Http { adapter, .. } if *adapter == RESPONSES_ADAPTER),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_truncated_stream_with_a_filter_error_chunk_is_reported_as_truncated() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
        "\"model\":\"gpt-35-turbo-instruct\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"role\":\"assistant\",\"content\":\"Part\"},\"finish_reason\":null,",
        "\"content_filter_results\":{\"error\":{\"code\":\"content_filter_error\",",
        "\"message\":\"The contents are not filtered\"}}}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = Streaming::stream(
        &adapter,
        &Request::new("chat-dep", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect_err("must raise");
    assert!(
        matches!(&err, ProviderError::StreamTruncated { .. }),
        "a filter that did not run must not hide the truncation: {err:?}"
    );
}

#[test]
fn a_responses_filter_reads_the_documented_content_filters_entries() {
    use crate::azure::responses_filter;
    // https://learn.microsoft.com/en-us/azure/ai-foundry/openai/how-to/responses
    // (Guardrails): entries carry `blocked`, `source_type`, `content_filter_results`.
    let entry = |blocked: Option<bool>, results: serde_json::Value| {
        let mut e = json!({"source_type": "completion", "content_filter_results": results});
        if let Some(b) = blocked {
            e["blocked"] = json!(b);
        }
        json!({"content_filters": [e]})
    };
    let safe = json!({"hate": {"filtered": false, "severity": "safe"}});
    let hit = json!({"violence": {"filtered": true, "severity": "high"}});
    let error = json!({"error": {"code": "content_filter_error", "message": "x"}});
    assert_eq!(
        responses_filter(&entry(Some(true), safe.clone())),
        Some("content_filter")
    );
    assert_eq!(responses_filter(&entry(Some(false), safe.clone())), None);
    // No `blocked` field: a filtered category still cuts the turn.
    assert_eq!(
        responses_filter(&entry(None, hit.clone())),
        Some("content_filter")
    );
    assert_eq!(responses_filter(&entry(None, safe)), None);
    // An explicit `blocked: false` is respected.
    assert_eq!(responses_filter(&entry(Some(false), hit)), None);
    assert_eq!(
        responses_filter(&entry(Some(false), error)),
        Some("content_filter_results_error")
    );
}

#[tokio::test]
async fn a_stream_cut_by_the_filter_with_no_usage_raises_the_filter_reason_and_stays_open() {
    let server = MockServer::start().await;
    let no_usage: String = CHAT_STREAM_FILTERED
        .lines()
        .filter(|line| !line.contains("\"usage\""))
        .collect::<Vec<_>>()
        .join("\n");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(no_usage))
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    let err = Streaming::stream(
        &adapter,
        &Request::new("chat-dep", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect_err("must raise");
    assert!(
        matches!(&err, ProviderError::TurnNotUsable { stop_reason, .. }
            if stop_reason == "content_filter"),
        "{err:?}"
    );
    let id = token.id().clone();
    drop(token);
    let record = ledger.call(&id).expect("read").expect("present");
    assert_ne!(
        record.settlement,
        Settlement::Counted,
        "no usage was seen, so nothing is counted"
    );
}

#[tokio::test]
async fn a_429_is_recorded_per_deployment_with_azure_headers() {
    // https://learn.microsoft.com/en-us/azure/ai-foundry/openai/how-to/quota
    // (rate-limit response headers; retry-after-ms on a 429).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("x-ratelimit-limit-requests", "60")
                .insert_header("x-ratelimit-limit-tokens", "150000")
                .insert_header("x-ratelimit-reset-requests", "10")
                .insert_header("retry-after-ms", "2000")
                .set_body_string("{}"),
        )
        .mount(&server)
        .await;
    let adapter = chat_adapter(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, &adapter, "chat-dep");
    adapter
        .complete(&Request::new("chat-dep", "hi", 100), &token)
        .await
        .expect_err("429");

    let key = kgraf_provider::limits::scoped_key(&server.uri(), "chat-dep");
    let observed = kgraf_provider::limits::get(&key).expect("recorded under the deployment key");
    assert_eq!(observed.requests_limit, Some(60));
    assert_eq!(observed.tokens_limit, Some(150_000));
    assert_eq!(observed.reset_ms, Some(2_000), "the retry wait wins");
    assert!(kgraf_provider::limits::last_429(&key).is_some());
    let other = kgraf_provider::limits::scoped_key(&server.uri(), "other-dep");
    assert!(kgraf_provider::limits::get(&other).is_none());
    assert!(kgraf_provider::limits::get(&server.uri()).is_none());
}

// ------------------------------------------------------------- embeddings

const EMBEDDINGS: &str = include_str!("../tests/fixtures/azure/embeddings.json");
const EMBED_MODEL: &str = "text-embedding-3-small";

fn embedder(server: &MockServer, version: &str) -> crate::OpenAiEmbeddings {
    let settings = settings(version, &format!("embed-dep={EMBED_MODEL}"));
    crate::OpenAiEmbeddings::new(config(server, "embed-dep"))
        .expect("adapter")
        .with_azure(
            AzureTransport::embeddings(&settings)
                .expect("transport")
                .with_origin(server.uri()),
        )
        .expect("azure")
}

fn two_texts() -> Vec<String> {
    vec!["kgraf".to_owned(), "graph".to_owned()]
}

#[tokio::test]
async fn azure_embeddings_on_v1_send_the_deployment_as_model_with_the_key_header() {
    use kgraf_provider::Embedder;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/embeddings"))
        .and(header("api-key", KEY))
        .and(wiremock::matchers::body_json(
            json!({"model": "embed-dep", "input": ["kgraf", "graph"]}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-limit-requests", "120")
                .insert_header("x-ratelimit-reset-requests", "10")
                .set_body_string(EMBEDDINGS),
        )
        .expect(1)
        .mount(&server)
        .await;
    let adapter = embedder(&server, "v1");
    assert_eq!(adapter.adapter(), crate::azure::EMBEDDINGS_ADAPTER);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            adapter.adapter(),
            Protocol::Inclusive,
            "embed-dep",
            "a",
            "t",
        )
        .expect("begin");
    let out = adapter
        .embed("embed-dep", &two_texts(), &token)
        .await
        .expect("embed");
    assert_eq!(out.vectors.len(), 2);
    assert_eq!(out.usage.uncached_input, 8);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
    // Azure limits are per deployment, read with the Azure rules (a bare
    // number of seconds for the reset).
    let key = kgraf_provider::limits::scoped_key(&server.uri(), "embed-dep");
    let observed = kgraf_provider::limits::get(&key).expect("the deployment's key");
    assert_eq!(observed.requests_limit, Some(120));
    assert_eq!(observed.reset_ms, Some(10_000));
    assert!(kgraf_provider::limits::get(&server.uri()).is_none());
}

#[tokio::test]
async fn azure_embeddings_on_a_dated_version_put_the_deployment_in_the_path() {
    use kgraf_provider::Embedder;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/deployments/embed-dep/embeddings"))
        .and(query_param("api-version", "2023-05-15"))
        .and(header("api-key", KEY))
        .and(wiremock::matchers::body_json(
            json!({"input": ["kgraf", "graph"]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(EMBEDDINGS))
        .expect(1)
        .mount(&server)
        .await;
    // Older than the chat floor: the embeddings body has no such floor.
    let adapter = embedder(&server, "2023-05-15");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            adapter.adapter(),
            Protocol::Inclusive,
            "embed-dep",
            "a",
            "t",
        )
        .expect("begin");
    adapter
        .embed("embed-dep", &two_texts(), &token)
        .await
        .expect("embed");
}

#[tokio::test]
async fn an_azure_embedding_reply_naming_another_model_raises_and_stays_open() {
    use kgraf_provider::Embedder;
    let server = MockServer::start().await;
    let mut reply: Value = serde_json::from_str(EMBEDDINGS).expect("fixture");
    reply["model"] = json!("text-embedding-ada-002");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .mount(&server)
        .await;
    let adapter = embedder(&server, "v1");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            adapter.adapter(),
            Protocol::Inclusive,
            "embed-dep",
            "a",
            "t",
        )
        .expect("begin");
    let err = adapter
        .embed("embed-dep", &two_texts(), &token)
        .await
        .expect_err("substituted");
    assert!(
        matches!(err, ProviderError::ModelSubstituted { .. }),
        "{err}"
    );
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Unreconciled);
}

#[test]
fn an_unmapped_embedding_deployment_is_refused_when_built() {
    let settings = settings("v1", &format!("embed-dep={EMBED_MODEL}"));
    let config = ProviderConfig::new(
        CHAT_ADAPTER,
        "https://my-resource.openai.azure.com",
        KEY,
        vec!["other-dep".to_owned()],
        Protocol::Inclusive,
    )
    .expect("config");
    let err = crate::OpenAiEmbeddings::new(config)
        .expect("adapter")
        .with_azure(AzureTransport::embeddings(&settings).expect("transport"))
        .err()
        .expect("unmapped");
    assert!(
        matches!(err, ProviderError::ModelUnsupported { .. }),
        "{err}"
    );
}
