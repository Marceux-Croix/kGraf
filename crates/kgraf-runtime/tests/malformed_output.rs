//! A server that fails to parse the model's own tool call answers HTTP 500
//! (Ollama does, for a malformed XML or JSON call). The real Chat Completions
//! adapter runs through the worker loop against a stub server: the round is sent
//! again within the attempt, a bounded number of times, and every try is its own
//! ledger call.

use kgraf_core::Protocol;
use kgraf_provider::{Content, Ledger, Provider, ProviderConfig, Settlement};
use kgraf_provider_openai::{ChatCompletionsAdapter, UsageDialect};
use kgraf_runtime::{
    CallMode, ContextConfig, NeutralPrefixEncoder, NoticeKind, PrefixRegistry, PrefixSnapshot,
    SharedPrefix, SpawnOptions, WorkerBrief, WorkerEnv, WorkerEvent, WorkerOutcome, WorkerResult,
    spawn_worker,
};
use kgraf_tools::ToolRegistry;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "parse-model";
const PARSE_500: &str = r#"{"error":{"message":"XML syntax error on line 8: element <function> closed by </parameter>","type":"api_error","param":null,"code":null}}"#;

fn ok_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "model": MODEL,
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "all done"}
        }],
        "usage": {
            "prompt_tokens": 100,
            "prompt_tokens_details": {"cached_tokens": 40},
            "completion_tokens": 7
        }
    })
}

fn env(url: &str) -> (Arc<WorkerEnv>, Arc<Ledger>) {
    let provider: Arc<dyn Provider> = Arc::new(
        ChatCompletionsAdapter::new(
            ProviderConfig::local(url.to_owned(), vec![MODEL.into()], Protocol::Inclusive),
            UsageDialect::Details,
        )
        .expect("adapter"),
    );
    let registry = ToolRegistry::new();
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "parse-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a test agent.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("prefix");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger.clone(), provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    (Arc::new(env), ledger)
}

async fn run(env: Arc<WorkerEnv>, budget: u32) -> (Vec<WorkerEvent>, WorkerResult) {
    let brief = WorkerBrief {
        task: "parse-task".into(),
        worker: "parse-worker".into(),
        instruction: "Do the thing.".into(),
        workspace_root: std::env::temp_dir(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: budget,
        context: ContextConfig::for_model(1_000_000).expect("config"),
        write_scope: None,
    };
    let mut handle = spawn_worker(env, brief, SpawnOptions::default())
        .await
        .expect("spawn");
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut events = Vec::new();
        while let Some(event) = handle.next_event().await {
            events.push(event);
        }
        (events, handle.result().await.expect("result"))
    })
    .await
    .expect("the worker finished in time")
}

async fn fail_n_times(server: &MockServer, n: u64, status: u16, body: &str) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
        .up_to_n_times(n)
        .mount(server)
        .await;
}

async fn serve_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(server)
        .await;
}

/// The text of a message's `content`, whether the wire sends a string or parts.
fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        other => panic!("unexpected content {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_malformed_generation_is_sent_again_and_the_attempt_completes() {
    let server = MockServer::start().await;
    fail_n_times(&server, 1, 500, PARSE_500).await;
    serve_ok(&server).await;
    let (env, ledger) = env(&server.uri());

    let (events, result) = run(env, 6).await;

    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(result.answer, "all done");
    assert!(
        result.route_fault.is_none(),
        "a parse failure is not a route fault"
    );
    // Both calls are in the ledger: the failed one stays open (a 5xx may have
    // billed), the retry is counted.
    let calls = ledger.calls().expect("calls");
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].settlement, Settlement::Unreconciled);
    assert_eq!(calls[1].settlement, Settlement::Counted);
    assert_eq!(result.calls.len(), 2);
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::Notice {
            kind: NoticeKind::MalformedOutput,
            ..
        }
    )));

    // The retry carries a short note at the end of the transcript and leaves the
    // rest of the request unchanged, so the provider's cached prefix still hits.
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 2);
    let first: Value = serde_json::from_slice(&requests[0].body).expect("json");
    let second: Value = serde_json::from_slice(&requests[1].body).expect("json");
    let (m1, m2) = (
        first["messages"].as_array().expect("messages"),
        second["messages"].as_array().expect("messages"),
    );
    assert_eq!(m2.len(), m1.len() + 1, "the note is one added message");
    assert_eq!(
        &m2[..m1.len()],
        &m1[..],
        "every earlier message is byte-identical"
    );
    assert_eq!(first["tools"], second["tools"]);
    let note = text_of(m2.last().unwrap());
    assert!(note.contains("malformed tool call"), "{note}");

    // The retried round's row settles unreconciled with no usage and no
    // request id, so the transcript keeps the server's answer: its status and
    // its parse error. Without them a reader cannot tell this server fault
    // from a call dropped mid-flight.
    let reasons = abandon_reasons(&events);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons[0].starts_with(kgraf_runtime::MALFORMED_OUTPUT_ABANDON_REASON),
        "{reasons:?}"
    );
    assert!(reasons[0].contains("HTTP 500"), "{reasons:?}");
    assert!(
        reasons[0].contains("XML syntax error on line 8"),
        "{reasons:?}"
    );
}

/// The `RoundAbandoned` reasons, in order.
fn abandon_reasons(events: &[WorkerEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::RoundAbandoned { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

/// A server that quotes the whole unparsed call (Ollama's `error parsing tool
/// call: raw='...'`) does not put that whole call in the transcript: the
/// reason keeps the status and the head of the body, and marks the cut. The
/// raw text is two-byte characters, so a cut at a fixed byte count would
/// split one without the boundary check.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_parse_error_is_kept_bounded_in_the_abandon_reason() {
    let raw = "\u{e9}".repeat(20_000);
    let body = format!(
        r#"{{"error":{{"message":"error parsing tool call: raw='{raw}', err=unexpected end","type":"api_error"}}}}"#
    );
    let server = MockServer::start().await;
    fail_n_times(&server, 1, 500, &body).await;
    serve_ok(&server).await;
    let (env, _) = env(&server.uri());

    let (events, result) = run(env, 6).await;

    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    let reasons = abandon_reasons(&events);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    let reason = &reasons[0];
    assert!(reason.contains("HTTP 500"), "{reason}");
    assert!(reason.contains("error parsing tool call"), "{reason}");
    assert!(reason.contains("more bytes omitted]"), "{reason}");
    assert!(
        reason.len() < 2048 + 200,
        "the reason is bounded: {} bytes",
        reason.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn three_malformed_generations_fail_the_attempt_naming_the_parse_cause() {
    let server = MockServer::start().await;
    fail_n_times(&server, 3, 500, PARSE_500).await;
    serve_ok(&server).await;
    let (env, ledger) = env(&server.uri());

    let (_, result) = run(env, 8).await;

    let WorkerOutcome::Failed { error } = &result.outcome else {
        panic!("expected Failed, got {:?}", result.outcome);
    };
    assert!(error.contains("XML syntax error"), "{error}");
    assert!(
        error.contains("could not parse the model's own tool-call output"),
        "{error}"
    );
    assert!(error.contains("sent again 2 times"), "{error}");
    assert!(result.route_fault.is_none());
    // One try and two retries, each its own row; the stub never got a fourth.
    assert_eq!(ledger.calls().expect("calls").len(), 3);
    assert_eq!(server.received_requests().await.expect("r").len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_generic_500_is_not_retried() {
    let server = MockServer::start().await;
    fail_n_times(&server, 1, 500, "internal server error").await;
    serve_ok(&server).await;
    let (env, ledger) = env(&server.uri());

    let (events, result) = run(env, 6).await;

    let WorkerOutcome::Failed { error } = &result.outcome else {
        panic!("expected Failed, got {:?}", result.outcome);
    };
    assert!(error.contains("HTTP 500"), "{error}");
    assert!(!error.contains("tool-call output"), "{error}");
    assert!(
        result.route_fault.is_none(),
        "an HTTP status is not a route fault"
    );
    assert_eq!(ledger.calls().expect("calls").len(), 1);
    assert_eq!(server.received_requests().await.expect("r").len(), 1);
    assert!(!events.iter().any(|e| matches!(
        e,
        WorkerEvent::Notice {
            kind: NoticeKind::MalformedOutput,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_generation_on_the_last_round_fails_without_a_retry() {
    let server = MockServer::start().await;
    fail_n_times(&server, 2, 500, PARSE_500).await;
    serve_ok(&server).await;
    let (env, _) = env(&server.uri());

    // Budget 2: the retry is the final round, which never starts another.
    let (_, result) = run(env, 2).await;

    let WorkerOutcome::Failed { error } = &result.outcome else {
        panic!("expected Failed, got {:?}", result.outcome);
    };
    assert!(error.contains("XML syntax error"), "{error}");
    assert!(error.contains("no round is left"), "{error}");
    assert_eq!(server.received_requests().await.expect("r").len(), 1);
}
