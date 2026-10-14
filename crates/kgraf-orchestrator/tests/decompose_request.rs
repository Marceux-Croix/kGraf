//! With contract checks of their own off (the default, and every benchmark
//! driver's pin), the whole `decompose` request a run sends (system prompt,
//! messages, output cap, tools, reasoning) is byte for byte the request kGraf
//! sent before own checks existed. The fixture was written by this same test
//! run against the code from before them, with `KGRAF_WRITE_DECOMPOSE_GOLDEN`
//! set to the fixture's path.

mod support;

use kgraf_core::{Protocol, RunId, TokenUsage};
use kgraf_orchestrator::{ModelCaller, ModelDecomposer, Orchestrator};
use kgraf_provider::{CallToken, Content, Provider, ProviderError, Request, Response, StopReason};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// The planner's model: records each request whole and answers one plan.
struct Capture {
    requests: Mutex<Vec<String>>,
    reply: String,
}

#[async_trait::async_trait]
impl Provider for Capture {
    fn adapter(&self) -> &'static str {
        "fake-plan"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.requests
            .lock()
            .expect("lock")
            .push(format!("{request:#?}"));
        let usage = TokenUsage {
            uncached_input: 200,
            text_output: 50,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![Content::text(self.reply.clone())],
            stop_reason: StopReason::EndTurn,
            usage,
            request_id: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_decompose_request_with_own_checks_off_is_unchanged() {
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        |_| String::new(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let capture = Arc::new(Capture {
        requests: Mutex::new(Vec::new()),
        reply: plan_json(&[c("a", "make a", "src/a.rs", &[])]),
    });
    let mut services = p.services(WINDOW);
    services.decomposer = Arc::new(ModelDecomposer(ModelCaller {
        reasoning: Default::default(),
        provider: capture.clone(),
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 4_096,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    }));
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    o.start(RunId::new("r1"), "make a and test it".into())
        .await
        .expect("run");
    let first = capture
        .requests
        .lock()
        .expect("lock")
        .first()
        .cloned()
        .expect("a decompose request");
    let got = first.replace(&dir.path().display().to_string(), "<dir>");
    if let Ok(path) = std::env::var("KGRAF_WRITE_DECOMPOSE_GOLDEN") {
        std::fs::write(path, &got).expect("write the fixture");
        return;
    }
    assert!(got.contains("make a and test it"), "{got}");
    let golden = include_str!("fixtures/decompose-request/own-checks-off.txt");
    assert!(
        got == golden,
        "the decompose request changed with own checks off:\n--- before\n{golden}\n--- now\n{got}"
    );
}
