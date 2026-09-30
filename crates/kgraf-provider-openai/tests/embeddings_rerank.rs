//! The embedding and rerank adapters against recorded replies.
//!
//! The embeddings replies are the published shape
//! (<https://platform.openai.com/docs/api-reference/embeddings/create>) and
//! the request body asserted below is the one a live call to
//! `api.openai.com/v1/embeddings` accepted (decision 0089).
//!
//! The rerank replies are written from the shape vLLM and Jina publish
//! (<https://docs.vllm.ai/en/latest/serving/openai_compatible_server.html>,
//! <https://jina.ai/reranker/>). The Ollama and TEI replies cite their own
//! sources beside each test. No rerank server, Ollama or TEI server has been
//! reached: those dialects are unverified.

use kgraf_core::Protocol;
use kgraf_provider::{Embedder, Ledger, ProviderConfig, ProviderError, Reranker, Settlement};
use kgraf_provider_openai::embeddings::{EmbedDialect, RerankDialect};
use kgraf_provider_openai::{
    EMBEDDINGS_ADAPTER, EchoRule, OpenAiEmbeddings, OpenAiRerank, RERANK_ADAPTER,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EMBED_MODEL: &str = "text-embedding-3-small";
const RERANK_MODEL: &str = "BAAI/bge-reranker-v2-m3";

fn config(server: &MockServer, model: &str) -> ProviderConfig {
    ProviderConfig::new(
        "test",
        format!("{}/v1", server.uri()),
        "test-key",
        vec![model.to_owned()],
        Protocol::Inclusive,
    )
    .expect("config")
}

fn texts(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// The published embeddings reply, with the rows deliberately out of order.
fn embeddings_reply(model: &str) -> serde_json::Value {
    json!({
        "object": "list",
        "data": [
            {"object": "embedding", "index": 1, "embedding": [0.0, 1.0, 0.0]},
            {"object": "embedding", "index": 0, "embedding": [0.5, -0.25, 0.125]}
        ],
        "model": model,
        "usage": {"prompt_tokens": 7, "total_tokens": 7}
    })
}

#[tokio::test]
async fn an_embedding_request_is_the_published_shape_and_settles_its_input_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_json(
            json!({"model": EMBED_MODEL, "input": ["kgraf", "graph"]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(embeddings_reply(EMBED_MODEL)))
        .expect(1)
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(adapter.adapter(), adapter.protocol(), EMBED_MODEL, "a", "t")
        .expect("begin");
    let out = adapter
        .embed(EMBED_MODEL, &texts(&["kgraf", "graph"]), &token)
        .await
        .expect("embed");
    // Input order, not arrival order.
    assert_eq!(
        out.vectors,
        vec![vec![0.5, -0.25, 0.125], vec![0.0, 1.0, 0.0]]
    );
    assert_eq!(out.usage.uncached_input, 7);
    assert_eq!(out.usage.text_output, 0);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(record.adapter, EMBEDDINGS_ADAPTER);
    assert_eq!(record.usage.uncached_input, 7);
    assert!(record.usage.raw.is_some(), "the raw usage blob is kept");
}

#[tokio::test]
async fn a_substituted_embedding_model_raises_and_the_row_stays_open() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(embeddings_reply("cheaper-embedding")),
        )
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            EMBEDDINGS_ADAPTER,
            Protocol::Inclusive,
            EMBED_MODEL,
            "a",
            "t",
        )
        .expect("begin");
    let err = adapter
        .embed(EMBED_MODEL, &texts(&["a", "b"]), &token)
        .await
        .expect_err("substituted");
    assert!(
        matches!(err, ProviderError::ModelSubstituted { .. }),
        "{err}"
    );
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Unreconciled);
    // The usage the body carried is kept on the open row.
    assert_eq!(record.usage.uncached_input, 7);
}

#[tokio::test]
async fn an_embedding_reply_without_usage_raises_rather_than_settling_zero() {
    let server = MockServer::start().await;
    let mut reply = embeddings_reply(EMBED_MODEL);
    reply.as_object_mut().expect("object").remove("usage");
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            EMBEDDINGS_ADAPTER,
            Protocol::Inclusive,
            EMBED_MODEL,
            "a",
            "t",
        )
        .expect("begin");
    let err = adapter
        .embed(EMBED_MODEL, &texts(&["a", "b"]), &token)
        .await
        .expect_err("no usage");
    assert!(matches!(err, ProviderError::UsageMissing { .. }), "{err}");
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Unreconciled);
}

#[tokio::test]
async fn a_vector_count_that_differs_from_the_input_raises() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embeddings_reply(EMBED_MODEL)))
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            EMBEDDINGS_ADAPTER,
            Protocol::Inclusive,
            EMBED_MODEL,
            "a",
            "t",
        )
        .expect("begin");
    let err = adapter
        .embed(EMBED_MODEL, &texts(&["only one"]), &token)
        .await
        .expect_err("count");
    assert!(err.to_string().contains("2 vectors for 1 texts"), "{err}");
}

#[tokio::test]
async fn an_unconfigured_model_is_refused_before_sending_and_billed_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(EMBEDDINGS_ADAPTER, Protocol::Inclusive, "other", "a", "t")
        .expect("begin");
    let err = adapter
        .embed("other", &texts(&["a"]), &token)
        .await
        .expect_err("refused");
    assert!(
        matches!(err, ProviderError::ModelUnsupported { .. }),
        "{err}"
    );
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Failed);
}

fn rerank_reply(model: &str) -> serde_json::Value {
    // vLLM's /v1/rerank reply; Jina's differs only in carrying `prompt_tokens`
    // beside `total_tokens` in `usage`.
    json!({
        "id": "rerank-0",
        "model": model,
        "usage": {"total_tokens": 38},
        "results": [
            {"index": 2, "document": {"text": "c"}, "relevance_score": 0.91},
            {"index": 0, "document": {"text": "a"}, "relevance_score": 0.07}
        ]
    })
}

#[tokio::test]
async fn a_rerank_request_is_the_common_shape_and_its_ranking_is_best_first() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .and(body_json(json!({
            "model": RERANK_MODEL,
            "query": "which file parses the graph",
            "documents": ["a", "b", "c"],
            "top_n": 2
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(rerank_reply(RERANK_MODEL)))
        .expect(1)
        .mount(&server)
        .await;
    let adapter = OpenAiRerank::new(config(&server, RERANK_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(RERANK_ADAPTER, Protocol::Inclusive, RERANK_MODEL, "a", "t")
        .expect("begin");
    let out = adapter
        .rerank(
            RERANK_MODEL,
            "which file parses the graph",
            &texts(&["a", "b", "c"]),
            Some(2),
            &token,
        )
        .await
        .expect("rerank");
    assert_eq!(
        out.results.iter().map(|r| r.index).collect::<Vec<_>>(),
        vec![2, 0]
    );
    assert!((out.results[0].score - 0.91).abs() < 1e-12);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(record.usage.uncached_input, 38);
}

#[tokio::test]
async fn a_rerank_index_outside_the_documents_raises() {
    let server = MockServer::start().await;
    let mut reply = rerank_reply(RERANK_MODEL);
    reply["results"][0]["index"] = json!(9);
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .mount(&server)
        .await;
    let adapter = OpenAiRerank::new(config(&server, RERANK_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(RERANK_ADAPTER, Protocol::Inclusive, RERANK_MODEL, "a", "t")
        .expect("begin");
    let err = adapter
        .rerank(RERANK_MODEL, "q", &texts(&["a", "b", "c"]), None, &token)
        .await
        .expect_err("outside");
    assert!(err.to_string().contains("outside the 3 documents"), "{err}");
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Unreconciled);
}

#[tokio::test]
async fn a_rejected_rerank_request_closes_as_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{\"error\":\"bad\"}"))
        .mount(&server)
        .await;
    let adapter = OpenAiRerank::new(config(&server, RERANK_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(RERANK_ADAPTER, Protocol::Inclusive, RERANK_MODEL, "a", "t")
        .expect("begin");
    let err = adapter
        .rerank(RERANK_MODEL, "q", &texts(&["a"]), None, &token)
        .await
        .expect_err("400");
    assert!(
        matches!(err, ProviderError::Http { status: 400, .. }),
        "{err}"
    );
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Failed);
}

fn begin<'l>(
    ledger: &'l Ledger,
    adapter: &'static str,
    model: &str,
) -> kgraf_provider::CallToken<'l> {
    ledger
        .begin(adapter, Protocol::Inclusive, model, "a", "t")
        .expect("begin")
}

/// OpenAI states rate limits per model, so an embedding model's headers are
/// recorded under that model's own key, never under the base URL the chat
/// adapter records the chat model's limits under. A 429 is recorded under
/// both: the route was refused.
#[tokio::test]
async fn embedding_rate_limits_are_recorded_per_model_and_a_429_under_the_route_too() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-limit-requests", "3000")
                .insert_header("x-ratelimit-limit-tokens", "1000000")
                .set_body_json(embeddings_reply(EMBED_MODEL)),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(429).set_body_string("{\"error\":\"slow down\"}"))
        .mount(&server)
        .await;
    let base = format!("{}/v1", server.uri());
    let scoped = kgraf_provider::limits::scoped_key(&base, EMBED_MODEL);
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, EMBEDDINGS_ADAPTER, EMBED_MODEL);
    adapter
        .embed(EMBED_MODEL, &texts(&["kgraf", "graph"]), &token)
        .await
        .expect("embed");
    let observed = kgraf_provider::limits::get(&scoped).expect("the model's own key");
    assert_eq!(observed.requests_limit, Some(3_000));
    assert_eq!(observed.tokens_limit, Some(1_000_000));
    assert!(
        kgraf_provider::limits::get(&base).is_none(),
        "the chat model's record on the base URL is not overwritten"
    );
    let token = begin(&ledger, EMBEDDINGS_ADAPTER, EMBED_MODEL);
    adapter
        .embed(EMBED_MODEL, &texts(&["a"]), &token)
        .await
        .expect_err("429");
    assert!(kgraf_provider::last_429(&scoped).is_some());
    assert!(kgraf_provider::last_429(&base).is_some());
}

/// A zero token count for a non-empty text is a count the server did not
/// make: settling it would record a real call as free.
#[tokio::test]
async fn an_ollama_reply_counting_zero_tokens_for_real_input_raises_and_stays_open() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "all-minilm",
            "embeddings": [[0.1, 0.2], [0.3, 0.4]],
            "prompt_eval_count": 0
        })))
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, "all-minilm"))
        .expect("adapter")
        .with_dialect(EmbedDialect::OllamaNative)
        .expect("dialect");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, EMBEDDINGS_ADAPTER, "all-minilm");
    let err = adapter
        .embed("all-minilm", &texts(&["one", "two"]), &token)
        .await
        .expect_err("zero is not a count");
    assert!(matches!(err, ProviderError::UsageMissing { .. }), "{err}");
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Unreconciled);
}

/// A route whose server caps the documents per request (TEI's default
/// `--max-client-batch-size` is 32) refuses more before sending: the server
/// would answer with an error, and nothing is billed.
#[tokio::test]
async fn more_documents_than_the_route_takes_are_refused_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let adapter = OpenAiRerank::new(config(&server, RERANK_MODEL))
        .expect("adapter")
        .with_max_documents(2);
    assert_eq!(adapter.max_documents(), 2);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, RERANK_ADAPTER, RERANK_MODEL);
    let err = adapter
        .rerank(RERANK_MODEL, "q", &texts(&["a", "b", "c"]), None, &token)
        .await
        .expect_err("over the cap");
    assert!(matches!(err, ProviderError::Unsent { .. }), "{err}");
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_ne!(record.settlement, Settlement::Unreconciled);
    assert_ne!(record.settlement, Settlement::Counted);
}

/// Together's rerank reference (<https://docs.together.ai/reference/rerank-1>)
/// shows a request for `Salesforce/Llama-Rank-V1` answered with
/// `"model": "salesforce/turboranker-0.8-3778-6328"`, and states no rule
/// relating the two. With the echo compared, every such call would bill and
/// then raise; the route's claim sets `EchoRule::NotCompared`, and the
/// default still raises on a substituted model.
#[tokio::test]
async fn a_documented_differing_rerank_model_is_accepted_only_where_the_claim_says_so() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "rerank",
            "id": "9dfa1a09-5ebc-4a40-970f-586cb8f4ae47",
            "model": "salesforce/turboranker-0.8-3778-6328",
            "results": [
                {"index": 0, "relevance_score": 0.29980177813003117, "document": {"text": "llama"}},
                {"index": 2, "relevance_score": 0.2752447527354349, "document": {"text": "guanaco"}}
            ],
            "usage": {"prompt_tokens": 1837, "completion_tokens": 0, "total_tokens": 1837}
        })))
        .mount(&server)
        .await;
    let model = "Salesforce/Llama-Rank-V1";
    let documents = texts(&["llama", "panda", "guanaco", "camel"]);
    let ledger = Ledger::open_in_memory().expect("ledger");

    let strict = OpenAiRerank::new(config(&server, model)).expect("adapter");
    let token = begin(&ledger, RERANK_ADAPTER, model);
    let err = strict
        .rerank(model, "q", &documents, Some(2), &token)
        .await
        .expect_err("compared");
    assert!(
        matches!(err, ProviderError::ModelSubstituted { .. }),
        "{err}"
    );

    let documented = OpenAiRerank::new(config(&server, model))
        .expect("adapter")
        .with_echo(EchoRule::NotCompared);
    let token = begin(&ledger, RERANK_ADAPTER, model);
    let out = documented
        .rerank(model, "q", &documents, Some(2), &token)
        .await
        .expect("accepted");
    assert_eq!(
        out.results.iter().map(|r| r.index).collect::<Vec<_>>(),
        vec![0, 2]
    );
    assert_eq!(out.usage.uncached_input, 1_837);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
}

#[tokio::test]
async fn vectors_of_mixed_widths_or_an_empty_vector_raise_and_the_row_stays_open() {
    for bad in [
        json!([{"index": 0, "embedding": [0.1, 0.2]}, {"index": 1, "embedding": [0.3]}]),
        json!([{"index": 0, "embedding": []}, {"index": 1, "embedding": []}]),
    ] {
        let server = MockServer::start().await;
        let reply = json!({
            "data": bad,
            "model": EMBED_MODEL,
            "usage": {"prompt_tokens": 3, "total_tokens": 3}
        });
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;
        let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = begin(&ledger, EMBEDDINGS_ADAPTER, EMBED_MODEL);
        let err = adapter
            .embed(EMBED_MODEL, &texts(&["a", "b"]), &token)
            .await
            .expect_err("unusable vectors");
        let text = err.to_string();
        assert!(
            text.contains("dimensions where row 0 has") || text.contains("empty vector"),
            "{text}"
        );
        let record = ledger.call(token.id()).expect("read").expect("row");
        assert_eq!(record.settlement, Settlement::Unreconciled);
    }
}

#[tokio::test]
async fn an_empty_text_or_too_many_texts_are_refused_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL))
        .expect("adapter")
        .with_limits(kgraf_provider::EmbedLimits {
            max_inputs: 2,
            max_request_tokens: 1_000,
        });
    let ledger = Ledger::open_in_memory().expect("ledger");
    for input in [texts(&["a", ""]), texts(&["a", "b", "c"])] {
        let token = begin(&ledger, EMBEDDINGS_ADAPTER, EMBED_MODEL);
        adapter
            .embed(EMBED_MODEL, &input, &token)
            .await
            .expect_err("refused");
        let record = ledger.call(token.id()).expect("read").expect("row");
        assert_eq!(record.settlement, Settlement::Failed);
    }
}

/// Ollama's `/api/embed` reply, from
/// <https://github.com/ollama/ollama/blob/main/docs/api.md#generate-embeddings>.
#[tokio::test]
async fn the_ollama_dialect_asks_the_native_endpoint_not_to_truncate() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .and(body_json(json!({
            "model": "all-minilm",
            "input": ["Why is the sky blue?"],
            "truncate": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "all-minilm",
            "embeddings": [[0.010071029, -0.0017594862, 0.05007221]],
            "total_duration": 14143917,
            "load_duration": 1019500,
            "prompt_eval_count": 8
        })))
        .expect(1)
        .mount(&server)
        .await;
    let adapter = OpenAiEmbeddings::new(config(&server, "all-minilm"))
        .expect("adapter")
        .with_dialect(EmbedDialect::OllamaNative)
        .expect("dialect");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, EMBEDDINGS_ADAPTER, "all-minilm");
    let out = adapter
        .embed("all-minilm", &texts(&["Why is the sky blue?"]), &token)
        .await
        .expect("embed");
    assert_eq!(out.vectors[0].len(), 3);
    assert_eq!(out.usage.uncached_input, 8);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
}

#[tokio::test]
async fn a_native_dialect_needs_a_base_url_ending_in_v1() {
    let server = MockServer::start().await;
    let config = ProviderConfig::new(
        "test",
        server.uri(),
        "test-key",
        vec!["m".to_owned()],
        Protocol::Inclusive,
    )
    .expect("config");
    let err = OpenAiEmbeddings::new(config)
        .expect("adapter")
        .with_dialect(EmbedDialect::TeiNative)
        .err()
        .expect("refused");
    assert!(err.to_string().contains("/v1"), "{err}");
}

/// TEI's `/embed` and `/rerank` shapes and its `x-compute-tokens` header, from
/// <https://huggingface.github.io/text-embeddings-inference/openapi.json> and
/// the router source (`router/src/lib.rs`, `ResponseMetadata`).
#[tokio::test]
async fn the_tei_dialects_send_truncate_false_and_settle_the_header_token_count() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embed"))
        .and(body_json(json!({"inputs": ["a", "b"], "truncate": false})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-compute-tokens", "6")
                .set_body_json(json!([[0.0, 1.0], [1.0, 0.0]])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .and(body_json(
            json!({"query": "q", "texts": ["a", "b", "c"], "truncate": false}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-compute-tokens", "21")
                .set_body_json(json!([
                    {"index": 1, "score": 0.9},
                    {"index": 2, "score": 0.5},
                    {"index": 0, "score": 0.1}
                ])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let ledger = Ledger::open_in_memory().expect("ledger");
    let embedder = OpenAiEmbeddings::new(config(&server, "bge"))
        .expect("adapter")
        .with_dialect(EmbedDialect::TeiNative)
        .expect("dialect");
    let token = begin(&ledger, EMBEDDINGS_ADAPTER, "bge");
    let out = embedder
        .embed("bge", &texts(&["a", "b"]), &token)
        .await
        .expect("embed");
    assert_eq!(out.vectors, vec![vec![0.0, 1.0], vec![1.0, 0.0]]);
    assert_eq!(out.usage.uncached_input, 6);

    let reranker = OpenAiRerank::new(config(&server, "bge"))
        .expect("adapter")
        .with_dialect(RerankDialect::TeiNative)
        .expect("dialect");
    let token = begin(&ledger, RERANK_ADAPTER, "bge");
    // TEI takes no top_n: the reply holds every document and the best two are
    // kept.
    let out = reranker
        .rerank("bge", "q", &texts(&["a", "b", "c"]), Some(2), &token)
        .await
        .expect("rerank");
    assert_eq!(
        out.results.iter().map(|r| r.index).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(out.usage.uncached_input, 21);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
}

#[tokio::test]
async fn a_tei_reply_without_its_token_header_raises_rather_than_settling_zero() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"index": 0, "score": 0.3}])))
        .mount(&server)
        .await;
    let reranker = OpenAiRerank::new(config(&server, "bge"))
        .expect("adapter")
        .with_dialect(RerankDialect::TeiNative)
        .expect("dialect");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, RERANK_ADAPTER, "bge");
    let err = reranker
        .rerank("bge", "q", &texts(&["a"]), None, &token)
        .await
        .expect_err("no usage");
    assert!(matches!(err, ProviderError::UsageMissing { .. }), "{err}");
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Unreconciled);
}

#[tokio::test]
async fn more_rerank_rows_than_top_n_are_cut_to_the_best_and_the_call_settles() {
    let server = MockServer::start().await;
    let mut reply = rerank_reply(RERANK_MODEL);
    reply["results"] = json!([
        {"index": 0, "relevance_score": 0.2},
        {"index": 1, "relevance_score": 0.8},
        {"index": 2, "relevance_score": 0.5}
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .mount(&server)
        .await;
    let adapter = OpenAiRerank::new(config(&server, RERANK_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, RERANK_ADAPTER, RERANK_MODEL);
    let out = adapter
        .rerank(RERANK_MODEL, "q", &texts(&["a", "b", "c"]), Some(1), &token)
        .await
        .expect("cut, not refused");
    assert_eq!(out.results.len(), 1);
    assert_eq!(out.results[0].index, 1);
    let record = ledger.call(token.id()).expect("read").expect("row");
    assert_eq!(record.settlement, Settlement::Counted);
}

#[tokio::test]
async fn a_429_on_an_embedding_route_is_recorded_for_the_learned_rate() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(429).set_body_string("{\"error\":\"slow down\"}"))
        .mount(&server)
        .await;
    let base = format!("{}/v1", server.uri());
    assert_eq!(kgraf_provider::last_429(&base), None);
    let adapter = OpenAiEmbeddings::new(config(&server, EMBED_MODEL)).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = begin(&ledger, EMBEDDINGS_ADAPTER, EMBED_MODEL);
    let err = adapter
        .embed(EMBED_MODEL, &texts(&["a"]), &token)
        .await
        .expect_err("429");
    assert!(
        matches!(err, ProviderError::Http { status: 429, .. }),
        "{err}"
    );
    assert!(kgraf_provider::last_429(&base).is_some());
}
