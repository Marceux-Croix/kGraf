//! A retry after an attempt that never reached a verifier carries what stopped
//! it, the way a verifier's refusal is carried: a stalled worker's repeated
//! call goes into the next attempt's brief, as data. The worker is a real
//! worker loop over a scripted model; the brief is read from the request the
//! next attempt's model receives.

mod support;

use kgraf_core::model_profile::ProfileStore;
use kgraf_core::{Protocol, RunId, TokenUsage};
use kgraf_orchestrator::{
    DirWorkspaces, ModelCaller, ModelDecomposer, ModelGrounder, Orchestrator, RunStatus, Services,
    SystemClock,
};
use kgraf_provider::{
    CallToken, Content, Provider, ProviderError, Request, Response, Role, StopReason,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// Every round issues the same read, so the stall detector ends the attempt;
/// the first user message of each attempt is kept.
#[derive(Default)]
struct RepeatingReader {
    openings: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for RepeatingReader {
    fn adapter(&self) -> &'static str {
        "fake-repeating-reader"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let assistant_turns = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        if assistant_turns == 0 {
            let opening = request
                .messages
                .iter()
                .find(|m| m.role == Role::User)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(Content::as_text)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            self.openings.lock().expect("lock").push(opening);
        }
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![Content::ToolUse {
                id: format!("t{assistant_turns}"),
                name: "read_file".into(),
                input: json!({"path": "out/sha.txt"}),
                signature: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage,
            request_id: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_retry_after_a_stall_names_the_repeated_call() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    let plan = json!({"contracts":[
        {"id":"b","instruction":"write the file WRITE src/b.rs","acceptance":"src/b.rs exists",
         "writes":[{"path":"src/b.rs"}],"tools":["write_file","read_file"],"route":"alpha"}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let reader = Arc::new(RepeatingReader::default());
    let caller = |provider: Arc<ScriptFake>| ModelCaller {
        provider,
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 2_000,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    };
    let services = Services {
        graph: p.graph.clone(),
        ledger: p.ledger.clone(),
        prices: prices(),
        routes: vec![
            route_with(
                "alpha",
                "vendor-a",
                ALPHA_MODEL,
                reader.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
            route_with(
                "beta",
                "vendor-b",
                BETA_MODEL,
                p.beta.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
        ],
        decomposer: Arc::new(ModelDecomposer(caller(p.planner.clone()))),
        grounder: Arc::new(ModelGrounder(caller(p.grounder.clone()))),
        verifier: p.verifier.clone(),
        workspaces: Arc::new(DirWorkspaces {
            base: p.dir.join("worktrees"),
        }),
        clock: Arc::new(SystemClock),
        admission: p.admission.clone(),
        runs_dir: p.dir.join("runs"),
        profiles: Arc::new(Mutex::new(ProfileStore::default())),
        profiles_dir: p.dir.join("profiles"),
    };
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "write the file".into())
        .await
        .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    let openings = reader.openings.lock().expect("lock").clone();
    assert!(openings.len() >= 2, "a retry was dispatched: {openings:?}");
    assert!(
        !openings[0].contains("ended before verification"),
        "the first attempt has nothing to carry: {}",
        openings[0]
    );
    let retry = &openings[1];
    assert!(retry.contains("ended before verification"), "{retry}");
    assert!(retry.contains("<verifier_refusal>"), "{retry}");
    assert!(retry.contains("stalled: the call `read_file"), "{retry}");
    assert!(retry.contains("out/sha.txt"), "{retry}");
}
