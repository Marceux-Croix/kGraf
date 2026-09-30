//! The run's non-generative model routes, for the orchestrator's own use
//! (decision 0089).
//!
//! The orchestrator is told which routes of each kind the run has (the
//! decomposition request lists them). Their tools (`decide`, `embed`,
//! `rerank`) are in every worker's tool set, and a contract names one in its
//! `tools` where its worker should use it. The orchestrator's own model may
//! call them while it plans: the decomposition request offers them as tools,
//! and each call it makes runs through [`ModelKindCalls::call_tool`]. Code in
//! this crate may call them directly through the typed methods below. Nothing
//! calls one on its own: using them is the orchestrator's choice, and no
//! default path does. Decision 0045 still holds: no decider supplies the
//! collision expectation handshake.
//!
//! Every call goes through `kgraf_tools::ModelCalls`, the same path a worker's
//! tool call takes, attributed to `orchestrator:<run>` with the trace role
//! `orchestrator`.
//!
//! **Spend.** Every ledger row these calls open is logged
//! ([`ModelKindCalls::take_calls`]); the orchestrator settles them into the
//! run's spend book after each planning call, as it settles the planner's own
//! rows, so the run's reported spend includes them and a resume, which seeds
//! the book from the ledger, finds nothing missing.
//!
//! **Stop and failure.** A tool call made while the run is stopped is
//! cancelled through the token the run's handle cancels
//! ([`ModelKindCalls::with_cancel`]). A tool call that fails is kept as a
//! notice ([`ModelKindCalls::take_failures`]) for the run's report, besides
//! the error the planning model reads.

use std::sync::{Arc, Mutex, PoisonError};

use kgraf_core::{CallId, KindError};
use kgraf_decide::{Answer, Question, State};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{Embeddings, Ranked, Ranking};
use kgraf_tools::{
    Attribution, CallLog, Called, CancellationToken, ModelCallError, ModelCalls, ToolError,
    ToolOutput,
};

/// The most characters of a failed tool's reply kept in its notice.
const FAILURE_EXCERPT_CHARS: usize = 300;

/// The orchestrator's handle on the run's non-generative routes.
#[derive(Clone, Debug)]
pub struct ModelKindCalls {
    /// The routes as given: what worker tools are built from.
    calls: ModelCalls,
    /// The same routes, logging every row the orchestrator opens.
    logged: ModelCalls,
    who: Attribution,
    log: CallLog,
    cancel: CancellationToken,
    failures: Arc<Mutex<Vec<String>>>,
}

impl ModelKindCalls {
    /// Calls attributed to `orchestrator:<run>`, task `run`.
    pub fn new(calls: ModelCalls, run: &str) -> Self {
        let log = CallLog::default();
        Self {
            logged: calls.logging_to(log.clone()),
            calls,
            who: Attribution {
                agent: format!("orchestrator:{run}"),
                task: run.to_owned(),
                role: "orchestrator".to_owned(),
            },
            log,
            cancel: CancellationToken::new(),
            failures: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Cancel every tool call in flight when `cancel` fires: the run's stop.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// The shared call path, for building the worker tools from the same
    /// routes. Not logged: a worker's calls are its own, not the
    /// orchestrator's.
    pub fn calls(&self) -> &ModelCalls {
        &self.calls
    }

    /// One line per kind present, as the planner is told.
    pub fn summary_lines(&self) -> Result<Vec<String>, KindError> {
        self.calls.routes().summary_lines()
    }

    /// The model tools the run's routes offer, as the orchestrator's own model
    /// is shown them. Empty when the run declares no model route.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        kgraf_tools::model_tools(&self.logged)
            .iter()
            .map(|t| t.definition())
            .collect()
    }

    /// Every ledger row opened through these calls since the last take.
    pub fn take_calls(&self) -> Vec<CallId> {
        self.log.take()
    }

    /// Every failed tool call since the last take, as a notice line. Scrubbed
    /// of secrets: the text may quote a route's reply.
    pub fn take_failures(&self) -> Vec<String> {
        std::mem::take(&mut *self.failures.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn note_failure(&self, tool: &str, detail: &str) {
        let excerpt: String = detail.chars().take(FAILURE_EXCERPT_CHARS).collect();
        let line = kgraf_secrets::scrub_persist_owned(format!(
            "the orchestrator's `{tool}` model tool call failed while planning: {excerpt}"
        ));
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    /// Run a model tool the orchestrator's own model asked for, attributed to
    /// the orchestrator. `None` when no model tool has that name.
    pub async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        let outcome =
            kgraf_tools::run_model_tool(&self.logged, &self.who, name, input, self.cancel.clone())
                .await?;
        match &outcome {
            Ok(output) if output.is_error => self.note_failure(name, &output.clone().render()),
            Ok(_) => {}
            Err(error) => self.note_failure(name, &error.to_string()),
        }
        Some(outcome)
    }

    pub async fn embed(
        &self,
        route: Option<&str>,
        texts: &[String],
    ) -> Result<Called<Embeddings>, ModelCallError> {
        self.logged.embed(route, texts, &self.who).await
    }

    pub async fn similar(
        &self,
        route: Option<&str>,
        query: &str,
        texts: &[String],
    ) -> Result<Called<Vec<Ranked>>, ModelCallError> {
        self.logged.similar(route, query, texts, &self.who).await
    }

    pub async fn rerank(
        &self,
        route: Option<&str>,
        query: &str,
        documents: &[String],
        top_n: Option<usize>,
    ) -> Result<Called<Ranking>, ModelCallError> {
        self.logged
            .rerank(route, query, documents, top_n, &self.who)
            .await
    }

    pub async fn decide(
        &self,
        route: Option<&str>,
        state: &State,
        questions: &[Question],
    ) -> Result<Called<Vec<Answer>>, ModelCallError> {
        self.logged.decide(route, state, questions, &self.who).await
    }
}
