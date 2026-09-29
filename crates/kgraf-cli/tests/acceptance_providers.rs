//! Provider acceptance, clause by clause.
//!
//! The roadmap's criterion reads:
//!
//! > Issue the same brief to two different providers through one interface, with
//! > per-worker credentials and base URL. Usage and cost recorded for both,
//! > **normalized per protocol**, with the additive and inclusive cache conventions
//! > each handled correctly and the raw usage blob persisted. An adapter asked for a
//! > model it cannot serve **fails loudly rather than silently substituting**. An
//! > interrupted stream is reconciled rather than losing its spend. No code path can
//! > reach a provider without being counted.
//!
//! This test lives in the CLI crate because it is the only place that depends on
//! both adapter crates at once, which is itself the point of the seam: nothing
//! below here knows there is more than one provider.
//!
//! Both providers are local mock endpoints. That is not a shortcut: the clauses
//! under test are about the protocol translation, the accounting arithmetic and the
//! failure behavior, all of which are fully determined by the bytes on the wire.
//! What a mock cannot test is whether a real vendor's current schema still matches
//! the one modeled here, which is exactly why the raw usage blob is persisted
//! verbatim (decision 0006).

use kgraf_core::cost::{ModelPrice, PriceTable};
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::Streaming;
use kgraf_provider::{Ledger, Provider, ProviderConfig, ProviderError, Request, Settlement};
use kgraf_provider_anthropic::AnthropicAdapter;
use kgraf_provider_openai::OpenAiAdapter;
use serde_json::json;
use wiremock::matchers::{header, header_exists, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The same real call, reported under each convention.
///
/// 150 genuinely uncached input tokens, 900 served from cache, and on the additive
/// side 50 written to cache. Both totals come to the same true figures, which is
/// what normalization has to produce.
fn additive_body(model: &str) -> serde_json::Value {
    json!({
        "id": "msg_additive",
        "model": model,
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": "plan: split the work in two"}],
        "usage": {
            "input_tokens": 150,
            "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 50,
            "output_tokens": 30
        }
    })
}

fn inclusive_body(model: &str) -> serde_json::Value {
    json!({
        "id": "resp_inclusive",
        "model": model,
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": "plan: split the work in two"}]
        }],
        "usage": {
            "input_tokens": 1050,
            "input_tokens_details": {"cached_tokens": 900},
            "output_tokens": 30,
            "output_tokens_details": {"reasoning_tokens": 0}
        }
    })
}

fn prices() -> PriceTable {
    let mut table = PriceTable::new();
    // Round numbers, not any real vendor's list. Prices are configuration; nothing
    // is compiled into the binary.
    let rate = ModelPrice {
        uncached_input: 3_000_000,
        cache_read: 300_000,
        cache_write: 3_750_000,
        output: 15_000_000,
        reasoning: 15_000_000,
    };
    table.set("additive-model", rate);
    table.set("inclusive-model", rate);
    table
}

#[tokio::test]
async fn clause_the_same_brief_goes_to_two_providers_through_one_interface() {
    let additive_server = MockServer::start().await;
    Mock::given(method("POST"))
        // Per-worker credential: this endpoint gets its own key, in its own header.
        .and(header("x-api-key", "credential-for-worker-one"))
        .respond_with(ResponseTemplate::new(200).set_body_json(additive_body("additive-model")))
        .mount(&additive_server)
        .await;

    let inclusive_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer credential-for-worker-two"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inclusive_body("inclusive-model")))
        .mount(&inclusive_server)
        .await;

    // Per-worker base URL and credentials, varying within one process.
    let additive = AnthropicAdapter::new(
        ProviderConfig::new(
            "anthropic-messages",
            additive_server.uri(),
            "credential-for-worker-one",
            vec!["additive-model".into()],
            Protocol::Additive,
        )
        .expect("config"),
    )
    .expect("adapter");
    let inclusive = OpenAiAdapter::new(
        ProviderConfig::new(
            "openai-responses",
            inclusive_server.uri(),
            "credential-for-worker-two",
            vec!["inclusive-model".into()],
            Protocol::Inclusive,
        )
        .expect("config"),
    )
    .expect("adapter");

    // One interface. Nothing below this line names a vendor.
    let fleet: Vec<(&dyn Provider, &str, &str)> = vec![
        (&additive, "additive-model", "worker-one"),
        (&inclusive, "inclusive-model", "worker-two"),
    ];

    let ledger = Ledger::open_in_memory().expect("ledger");
    let brief = "Split this roadmap phase into two independent parts.";
    let mut answers = Vec::new();
    for (provider, model, worker) in &fleet {
        let token = ledger
            .begin(
                provider.adapter(),
                provider.protocol(),
                model,
                worker,
                "task-phase-2",
            )
            .expect("begin");
        let response = provider
            .complete(&Request::new(*model, brief, 500), &token)
            .await
            .unwrap_or_else(|e| panic!("{} failed: {e}", provider.adapter()));
        answers.push(response);
    }

    assert_eq!(answers.len(), 2);
    for answer in &answers {
        assert_eq!(answer.text(), "plan: split the work in two");
    }

    // Usage recorded for both, normalized per protocol. The two protocols report
    // the same real call differently and must normalize to the same numbers.
    let recorded = ledger.calls().expect("calls");
    assert_eq!(recorded.len(), 2);
    for call in &recorded {
        assert_eq!(call.settlement, Settlement::Counted);
        assert_eq!(
            call.usage.uncached_input, 150,
            "{} reported the wrong uncached input under {}",
            call.adapter, call.protocol
        );
        assert_eq!(call.usage.cache_read, 900);
        assert_eq!(call.usage.text_output, 30);
        assert!(
            call.usage.raw.is_some(),
            "the raw usage blob must be persisted verbatim for {}",
            call.adapter
        );
    }

    // The one legitimate difference: cache writes are billed and reported on the
    // additive protocol and are not reported at all on the inclusive one.
    let by_adapter = |name: &str| {
        recorded
            .iter()
            .find(|c| c.adapter == name)
            .unwrap_or_else(|| panic!("no call for {name}"))
    };
    assert_eq!(by_adapter("anthropic-messages").usage.cache_write, 50);
    assert_eq!(by_adapter("openai-responses").usage.cache_write, 0);

    // Cost recorded for both, derived from a price table at read time.
    let table = prices();
    let (total, unpriced) = ledger.total_cost(&table).expect("cost");
    assert!(unpriced.is_empty(), "unpriced: {unpriced:?}");
    assert!(total.micros() > 0, "no cost was derived");
    let per_worker_one = ledger.cost_for_agent("worker-one", &table).expect("cost");
    let per_worker_two = ledger.cost_for_agent("worker-two", &table).expect("cost");
    assert!(per_worker_one.micros() > 0 && per_worker_two.micros() > 0);
    assert_eq!(
        per_worker_one + per_worker_two,
        total,
        "per-agent attribution must account for the whole total"
    );
    // The additive call also paid for a cache write, so it costs more for the same
    // conversation. That difference is real and must survive into the readout.
    assert!(per_worker_one.micros() > per_worker_two.micros());
}

#[tokio::test]
async fn clause_an_adapter_asked_for_a_model_it_cannot_serve_fails_loudly() {
    // Both refusals happen before any request is sent, because one provider
    // silently remaps an unknown model name to its cheap model and returns HTTP
    // 200: that is a correctness bug in a worker pool, not only a billing one.
    let server = MockServer::start().await;
    let adapter = AnthropicAdapter::new(
        ProviderConfig::new(
            "anthropic-messages",
            server.uri(),
            "k",
            vec!["the-model-we-pinned".into()],
            Protocol::Additive,
        )
        .expect("config"),
    )
    .expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            "anthropic-messages",
            Protocol::Additive,
            "stale-model",
            "w",
            "t",
        )
        .expect("begin");
    let err = adapter
        .complete(&Request::new("stale-model", "hi", 10), &token)
        .await
        .expect_err("must refuse");
    match err {
        ProviderError::ModelUnsupported { available, .. } => {
            assert!(available.contains("the-model-we-pinned"));
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "nothing should have been sent"
    );

    // And the after-the-fact assertion, for a substitution the pre-flight check
    // cannot see.
    let sneaky = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(additive_body("a-cheaper-model")))
        .mount(&sneaky)
        .await;
    let adapter = AnthropicAdapter::new(
        ProviderConfig::new(
            "anthropic-messages",
            sneaky.uri(),
            "k",
            vec!["additive-model".into()],
            Protocol::Additive,
        )
        .expect("config"),
    )
    .expect("adapter");
    let token = ledger
        .begin(
            "anthropic-messages",
            Protocol::Additive,
            "additive-model",
            "w",
            "t",
        )
        .expect("begin");
    let err = adapter
        .complete(&Request::new("additive-model", "hi", 10), &token)
        .await
        .expect_err("must raise");
    assert!(matches!(err, ProviderError::ModelSubstituted { .. }));
}

#[tokio::test]
async fn clause_an_interrupted_stream_is_reconciled_rather_than_losing_its_spend() {
    // One provider states explicitly that an interrupted stream may never deliver
    // its usage chunk. A request that billed but was not counted violates the accounting rule,
    // so it must appear as a hole with its request id, and be closeable once the
    // real figure is known.
    //
    // This fixture carries usage in `message_start`, as a real Anthropic stream
    // always does. That usage arrives before the connection is cut, so the
    // adapter raises `StreamTruncated` (distinct from `UsageMissing`, which is
    // for a stream that reports no usage at all) and settles the row through
    // `CallToken::leave_unreconciled_with`, which, unlike the plain
    // `leave_unreconciled` this adapter used before that method existed, keeps
    // both the partial usage and the request id on the same still-open row.
    let server = MockServer::start().await;
    let truncated = concat!(
        "data: {\"type\":\"message_start\",\"message\":",
        "{\"id\":\"msg_interrupted\",\"model\":\"additive-model\",",
        "\"usage\":{\"input_tokens\":5000,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":",
        "{\"type\":\"text_delta\",\"text\":\"half an ans\"}}\n\n",
    );
    Mock::given(method("POST"))
        .and(header_exists("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(truncated))
        .mount(&server)
        .await;

    let adapter = AnthropicAdapter::new(
        ProviderConfig::new(
            "anthropic-messages",
            server.uri(),
            "k",
            vec!["additive-model".into()],
            Protocol::Additive,
        )
        .expect("config"),
    )
    .expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            "anthropic-messages",
            Protocol::Additive,
            "additive-model",
            "w",
            "t",
        )
        .expect("begin");
    let err = Streaming::stream(
        &adapter,
        &Request::new("additive-model", "hi", 100),
        &token,
        &mut |_| {},
    )
    .await
    .expect_err("a stream truncated after message_start must raise");
    assert!(
        matches!(err, ProviderError::StreamTruncated { .. }),
        "{err:?}"
    );

    let open = ledger.unreconciled().expect("query");
    assert_eq!(open.len(), 1, "the spend must be visible as a hole");
    assert_eq!(
        open[0].request_id.as_deref(),
        Some("msg_interrupted"),
        "without the provider's request id the call cannot be reconciled at all"
    );
    // `message_start` already reported 5000 input tokens before the connection
    // was cut; that known partial usage must be visible on the still-open row,
    // not just the id, or a real reconciler has to guess at the size of the
    // hole it is closing.
    assert_eq!(
        open[0].usage.uncached_input, 5000,
        "the partial usage already reported must not be discarded"
    );
    // It is excluded from the total rather than folded in as zero, which would look
    // complete while understating spend.
    assert!(ledger.usage_by_model().expect("usage").is_empty());

    ledger
        .reconcile(
            &open[0].id,
            &TokenUsage {
                uncached_input: 150,
                text_output: 11,
                ..TokenUsage::default()
            },
        )
        .expect("reconcile");
    assert!(ledger.unreconciled().expect("query").is_empty());
    let (total, _) = ledger.total_cost(&prices()).expect("cost");
    assert!(total.micros() > 0, "the recovered spend must now count");
}

#[tokio::test]
async fn clause_no_code_path_reaches_a_provider_without_being_counted() {
    // This clause is enforced by the type system rather than by this test, and the
    // test's job is to state the mechanism so a future edit that weakens it is
    // visibly a change of design.
    //
    // `Provider::complete` requires a `&CallToken`. `CallToken` has no public
    // constructor; the only source is `Ledger::begin`, which writes the call's row
    // before returning. So a call that is not counted does not compile. There is no
    // negative test to write here, because the failing program cannot be expressed.
    //
    // What is testable is the consequence: a call that is *begun* is always
    // recorded, even when nobody remembers to settle it.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(additive_body("additive-model")))
        .mount(&server)
        .await;
    let adapter = AnthropicAdapter::new(
        ProviderConfig::new(
            "anthropic-messages",
            server.uri(),
            "k",
            vec!["additive-model".into()],
            Protocol::Additive,
        )
        .expect("config"),
    )
    .expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");

    // A caller that drops the token on the floor, having made the call.
    {
        let token = ledger
            .begin(
                "anthropic-messages",
                Protocol::Additive,
                "additive-model",
                "w",
                "t",
            )
            .expect("begin");
        let _ = adapter
            .complete(&Request::new("additive-model", "hi", 10), &token)
            .await;
    }
    // A caller that begins a call and never issues it at all.
    {
        let _abandoned = ledger
            .begin(
                "anthropic-messages",
                Protocol::Additive,
                "additive-model",
                "w",
                "t",
            )
            .expect("begin");
    }

    let all = ledger.calls().expect("calls");
    assert_eq!(all.len(), 2, "every begun call is on the record");
    // The issued one settled; the abandoned one is a reported hole rather than an
    // absence.
    assert_eq!(
        all.iter()
            .filter(|c| c.settlement == Settlement::Counted)
            .count(),
        1
    );
    assert_eq!(ledger.unreconciled().expect("query").len(), 1);
}

#[tokio::test]
async fn clause_capabilities_are_opt_in_and_a_provider_without_one_says_so() {
    // Not a separate roadmap clause, but the shape decision 0006 requires of the
    // seam, and the thing that breaks silently if it is ever replaced by booleans.
    let anthropic = AnthropicAdapter::new(
        ProviderConfig::new(
            "anthropic-messages",
            "https://example.invalid",
            "k",
            vec!["m".into()],
            Protocol::Additive,
        )
        .expect("config")
        .honoring_cache_control(),
    )
    .expect("adapter");
    let openai = OpenAiAdapter::new(
        ProviderConfig::new(
            "openai-responses",
            "https://example.invalid",
            "k",
            vec!["m".into()],
            Protocol::Inclusive,
        )
        .expect("config"),
    )
    .expect("adapter");

    assert!(anthropic.capabilities().cache_control.is_some());
    assert!(anthropic.capabilities().streaming.is_some());
    // The protocol has no breakpoint mechanism, so the capability is absent rather
    // than a no-op that would silently discard breakpoints.
    assert!(openai.capabilities().cache_control.is_none());
    assert!(openai.capabilities().reasoning.is_some());
}
