//! Decision 0089 through a whole run: the planner is told the run's routes of
//! other kinds, a contract may be granted their tool, nothing calls them by
//! default, and the orchestrator's own call is ledgered to it.

mod support;

use kgraf_core::cost::ModelPrice;
use kgraf_core::{ModelKind, Protocol, RunId, TokenUsage};
use kgraf_orchestrator::{
    CostBasis, Decomposer, ModelKindCalls, Orchestrator, PlanError, PlanOutput, PlanRequest,
    RunStatus,
};
use kgraf_provider::{
    CallToken, Embedder, Embeddings, ModelRoute, ModelRoutes, ProviderError, Settlement,
};
use kgraf_tools::{ModelCalls, model_tools};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;
const EMBEDDER: &str = "fixed-embedder";

struct FixedEmbedder;

#[async_trait::async_trait]
impl Embedder for FixedEmbedder {
    fn adapter(&self) -> &'static str {
        EMBEDDER
    }
    fn protocol(&self) -> Protocol {
        Protocol::Inclusive
    }
    async fn embed(
        &self,
        _model: &str,
        texts: &[String],
        token: &CallToken,
    ) -> Result<Embeddings, ProviderError> {
        let usage = TokenUsage {
            uncached_input: texts.len() as u64,
            ..TokenUsage::default()
        };
        token.settle_async(&usage, None).await?;
        Ok(Embeddings {
            vectors: texts
                .iter()
                .map(|t| {
                    if t == "far" {
                        vec![0.0, 1.0]
                    } else {
                        vec![1.0, 0.0]
                    }
                })
                .collect(),
            usage,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_lists_its_model_routes_grants_their_tool_and_calls_nothing_by_default() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = json!({"contracts": [{
        "id": "a",
        "instruction": "make a WRITE src/a.rs",
        "acceptance": "src/a.rs exists",
        "writes": [{"path": "src/a.rs"}],
        "tools": ["write_file", "embed"],
    }]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(10),
    );
    let embedder: Arc<dyn Embedder> = Arc::new(FixedEmbedder);
    let calls = ModelCalls::new(
        p.ledger.clone(),
        Arc::new(ModelRoutes::new(vec![ModelRoute::new(
            "local:e",
            ModelKind::EMBEDDING,
            EMBEDDER,
            Protocol::Inclusive,
            "e",
            embedder,
        )])),
    );
    let tools = model_tools(&calls);
    let o = Orchestrator::new(config(), p.services_with(WINDOW, &tools))
        .expect("orchestrator")
        .with_model_kinds(Arc::new(ModelKindCalls::new(calls, "r1")));
    let mine = o.model_kinds().cloned().expect("model kinds");

    let report = o
        .start(RunId::new("r1"), "build a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    // The planner was told the embedding route and its tool.
    let prompt = p.planner.prompts().join("\n");
    assert!(
        prompt.contains("embedding: local:e (tools embed;"),
        "{prompt}"
    );
    assert!(
        prompt.contains("embed"),
        "the Tools line offers it: {prompt}"
    );
    // The run offers the tools to its own planning model too.
    assert!(
        prompt.contains("You may call these tools yourself"),
        "{prompt}"
    );

    // Granted, not used: the fake workers never call it, and nothing else does.
    let embed_calls = |ledger: &kgraf_provider::Ledger| {
        ledger
            .calls()
            .expect("rows")
            .into_iter()
            .filter(|r| r.adapter == EMBEDDER)
            .collect::<Vec<_>>()
    };
    assert!(embed_calls(&p.ledger).is_empty(), "no call by default");

    // The orchestrator's own call is ledgered to it.
    let ranked = mine
        .similar(None, "q", &["far".into(), "near".into()])
        .await
        .expect("similar");
    assert_eq!(ranked.value[0].index, 1);
    let rows = embed_calls(&p.ledger);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent_id, "orchestrator:r1");
    assert_eq!(rows[0].settlement, Settlement::Counted);
    assert_eq!(rows[0].trace.role.as_deref(), Some("orchestrator"));
}

/// An embedder whose every call uses 1,000 input tokens, on a model priced at
/// one micro-dollar a token: its spend is plain in the run's total.
struct PricedEmbedder;

const PRICED: &str = "priced-embedder";

#[async_trait::async_trait]
impl Embedder for PricedEmbedder {
    fn adapter(&self) -> &'static str {
        PRICED
    }
    fn protocol(&self) -> Protocol {
        Protocol::Inclusive
    }
    async fn embed(
        &self,
        _model: &str,
        texts: &[String],
        token: &CallToken,
    ) -> Result<Embeddings, ProviderError> {
        let usage = TokenUsage {
            uncached_input: 1_000,
            ..TokenUsage::default()
        };
        token.settle_async(&usage, None).await?;
        Ok(Embeddings {
            vectors: texts.iter().map(|_| vec![1.0, 0.0]).collect(),
            usage,
        })
    }
}

/// The planner calls `embed` itself before it answers.
struct EmbedsThenPlans(Arc<dyn Decomposer>);

#[async_trait::async_trait]
impl Decomposer for EmbedsThenPlans {
    fn cost_basis(&self) -> CostBasis {
        self.0.cost_basis()
    }
    async fn decompose(&self, request: &PlanRequest) -> Result<PlanOutput, PlanError> {
        let tools = request.model_calls.as_ref().expect("offered");
        let out = tools
            .call_tool("embed", json!({"texts": ["a", "b"]}))
            .await
            .expect("an embed tool")
            .expect("ran");
        assert!(!out.is_error, "{out:?}");
        self.0.decompose(request).await
    }
    async fn interfaces(
        &self,
        request: &kgraf_orchestrator::InterfaceRequest,
    ) -> Result<kgraf_orchestrator::InterfaceReply, PlanError> {
        self.0.interfaces(request).await
    }
}

/// Decision 0089: every token is attributed, including a model tool's. The
/// planner's own `embed` call and a worker's both reach the run's spend, which
/// equals what a resume seeds from the ledger (each row at its price), so a
/// resume adds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_tool_spend_of_the_planner_and_a_worker_is_in_the_run_spend() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = json!({"contracts": [{
        "id": "a",
        "instruction": "make a WRITE src/a.rs EMBED",
        "acceptance": "src/a.rs exists",
        "writes": [{"path": "src/a.rs"}],
        "tools": ["write_file", "embed"],
    }]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(10),
    );
    let embedder: Arc<dyn Embedder> = Arc::new(PricedEmbedder);
    let calls = ModelCalls::new(
        p.ledger.clone(),
        Arc::new(ModelRoutes::new(vec![ModelRoute::new(
            "local:e",
            ModelKind::EMBEDDING,
            PRICED,
            Protocol::Inclusive,
            "e",
            embedder,
        )])),
    );
    let tools = model_tools(&calls);
    let mut services = p.services_with(WINDOW, &tools);
    services.prices.set(
        "e",
        ModelPrice {
            uncached_input: 1_000_000,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            reasoning: 0,
        },
    );
    services.decomposer = Arc::new(EmbedsThenPlans(services.decomposer.clone()));
    let prices = services.prices.clone();
    let o = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .with_model_kinds(Arc::new(ModelKindCalls::new(calls, "r1")));
    let report = o
        .start(RunId::new("r1"), "build a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let rows = p.ledger.calls().expect("rows");
    let embeds: Vec<_> = rows.iter().filter(|r| r.adapter == PRICED).collect();
    assert!(
        embeds.iter().any(|r| r.agent_id == "orchestrator:r1"),
        "the planner's own call"
    );
    assert!(
        embeds.iter().any(|r| r.task_id.starts_with("r1/")),
        "a worker's call"
    );
    // What a resume seeds: every row of the run at its price.
    let mut seeded = kgraf_core::Money::ZERO;
    for r in rows.iter().filter(|r| {
        r.task_id == "r1" || r.task_id.starts_with("r1/") || r.agent_id == "orchestrator:r1"
    }) {
        if r.settlement == Settlement::Counted {
            seeded += prices
                .cost_on(r.route.as_deref(), &r.model, &r.usage)
                .expect("priced");
        }
    }
    assert_eq!(report.spent, seeded, "{report:?}");
    let embed_spend = kgraf_core::Money::from_micros(1_000 * embeds.len() as i64);
    assert!(report.spent >= embed_spend, "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_without_model_routes_tells_the_planner_nothing_about_them() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(10),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    assert!(o.model_kinds().is_none());
    let report = o
        .start(RunId::new("r1"), "build a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let prompt = p.planner.prompts().join("\n");
    assert!(!prompt.contains("Model routes of other kinds"), "{prompt}");
}
