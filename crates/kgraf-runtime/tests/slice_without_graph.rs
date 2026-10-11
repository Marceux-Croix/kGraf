//! A brief that names a graph slice never fails the spawn because the slice
//! cannot be rendered. Seen live: a later contract of a run named a node the
//! run's graph held, while its worker had been given no graph, and both
//! attempts died at spawn with "no graph is configured for this worker".
//! The worker starts without the slice; its opening and its result name the
//! node and the reason.

use kgraf_core::{NodeId, Protocol, TokenUsage};
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, StopReason,
};
use kgraf_runtime::{
    CallMode, ContextConfig, GraphSlices, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot,
    SharedPrefix, SpawnOptions, WorkerBrief, WorkerEnv, WorkerOutcome, WorkerResult, spawn_worker,
};
use kgraf_tools::ToolRegistry;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MODEL: &str = "fake-model";

/// Answers every request with a plain end of turn and keeps the requests.
struct Fake {
    requests: Mutex<Vec<Request>>,
}

#[async_trait::async_trait]
impl Provider for Fake {
    fn adapter(&self) -> &'static str {
        "fake"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.requests.lock().expect("lock").push(request.clone());
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            finish_reason: Some("stop".into()),
            model: request.model.clone(),
            content: vec![Content::text("done")],
            stop_reason: StopReason::EndTurn,
            usage,
            request_id: None,
        })
    }
}

fn env(provider: Arc<Fake>, slices: Option<GraphSlices>) -> Arc<WorkerEnv> {
    let registry = ToolRegistry::new();
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "fake-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a kGraf worker.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("snapshot");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    if let Some(slices) = slices {
        env.slices = Arc::new(slices);
    }
    Arc::new(env)
}

fn brief(root: &std::path::Path) -> WorkerBrief {
    WorkerBrief {
        task: "task-w1".into(),
        worker: "w1".into(),
        instruction: "Say done.".into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![NodeId::new("d9356611710dc032")],
        skills: vec![],
        step_budget: 5,
        context: ContextConfig::for_model(100_000).expect("config"),
        write_scope: None,
    }
}

async fn run(env: Arc<WorkerEnv>, root: &std::path::Path) -> WorkerResult {
    let mut handle = spawn_worker(env, brief(root), SpawnOptions::default())
        .await
        .expect("the spawn does not fail on a slice that cannot be rendered");
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.next_event().await.is_some() {}
        handle.result().await.expect("result")
    })
    .await
    .expect("in time")
}

fn first_request_text(provider: &Fake) -> String {
    let requests = provider.requests.lock().expect("lock");
    requests[0]
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| c.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_named_slice_with_no_graph_configured_starts_the_worker_and_names_the_node() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Arc::new(Fake {
        requests: Mutex::new(Vec::new()),
    });
    let result = run(env(provider.clone(), None), dir.path()).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(result.slices_left_out.len(), 1);
    assert_eq!(result.slices_left_out[0].node.as_str(), "d9356611710dc032");
    assert!(result.slices_left_out[0].reason.contains("no graph"));
    let text = first_request_text(&provider);
    assert!(text.contains("slices left out"), "{text}");
    assert!(text.contains("d9356611710dc032"), "{text}");
}

#[tokio::test]
async fn a_named_slice_missing_from_the_configured_graph_starts_the_worker_the_same_way() {
    let dir = tempfile::tempdir().expect("dir");
    let store = kgraf_graph::GraphStore::open_in_memory(dir.path()).expect("store");
    let provider = Arc::new(Fake {
        requests: Mutex::new(Vec::new()),
    });
    let result = run(
        env(provider.clone(), Some(GraphSlices::new(store, 2_000))),
        dir.path(),
    )
    .await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(result.slices_left_out.len(), 1);
    assert!(result.slices_left_out[0].reason.contains("no longer holds"));
    assert!(first_request_text(&provider).contains("d9356611710dc032"));
}
