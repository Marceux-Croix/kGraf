//! Call traits for the non-generative model kinds, and the route handle that
//! carries a model of any kind (decision 0089).
//!
//! [`crate::Provider`] stays the generative seam, unchanged. Each other kind
//! has its own small trait here ([`Embedder`], [`Reranker`]) or in its own crate
//! (`kgraf_decide::Decider` for typed decisions), shaped the same way: every
//! call takes a ledger-minted [`CallToken`], so no model of any kind is reached
//! outside the accounting path (decision 0039). Folding the kinds into one wide
//! trait would make each implementation stub the others (decision 0006).
//!
//! [`ModelRoute`] is how a caller holds a route without knowing its kind at
//! compile time: the kind, the ledger identity (adapter, protocol, model) and
//! the call handle, stored as `Any`. [`ModelRoute::handle`] returns the handle
//! only for the kind asked for; any other kind is a [`KindError::Mismatch`]. A
//! new kind stores its own trait object the same way, with no change here.
//!
//! The stored handle types, by kind:
//!
//! | kind | handle |
//! |---|---|
//! | generative | `Arc<dyn Provider>` |
//! | typed-decision | `Arc<dyn kgraf_decide::Decider>` |
//! | embedding | `Arc<dyn Embedder>` |
//! | rerank | `Arc<dyn Reranker>` |

use crate::accounting::CallToken;
use crate::error::ProviderError;
use crate::ratelimit::RouteLimiter;
use kgraf_core::{KindError, ModelKind, Protocol, TokenUsage};
use std::any::Any;
use std::sync::Arc;

/// One embedding call's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Embeddings {
    /// One vector per input text, in input order.
    pub vectors: Vec<Vec<f32>>,
    /// What the call was settled with.
    pub usage: TokenUsage,
}

/// What one embedding request may carry, as the endpoint documents it.
///
/// Callers split a larger input into requests within these bounds
/// ([`Self::batches`]), one ledger row per request. The token bound is
/// enforced on bytes: every tokenizer in use for these models (byte-level BPE,
/// SentencePiece) spends at least one byte of input per token, so a request
/// whose texts total `max_request_tokens` bytes, less a per-text allowance for
/// the tokens a model adds around each input, cannot exceed the token bound. A
/// single text over the per-input limit is the server's to handle. Most
/// servers kGraf reaches through a claimed route refuse it (see the adapter's
/// notes on each); a route whose vendor documents that it truncates instead
/// says so in its catalog claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbedLimits {
    /// Most texts in one request.
    pub max_inputs: usize,
    /// Most tokens summed across the texts of one request.
    pub max_request_tokens: usize,
}

impl EmbedLimits {
    /// OpenAI's bounds: at most 2,048 inputs per request
    /// (<https://platform.openai.com/docs/api-reference/embeddings/create>).
    /// The same reference says some models also bound the tokens summed across
    /// the inputs, without giving the figure; 100,000 is a conservative
    /// choice, since a smaller batch costs only one more request and ledger
    /// row, while a request over the server's bound is refused whole. The
    /// default for every embedding route that documents no tighter bound.
    pub const OPENAI: EmbedLimits = EmbedLimits {
        max_inputs: 2_048,
        max_request_tokens: 100_000,
    };

    /// Tokens a model may add around each input (a start and an end marker,
    /// with room to spare), counted against the request bound.
    pub const PER_INPUT_ALLOWANCE: usize = 8;

    /// Split `texts` into consecutive index ranges, each within the bounds. A
    /// single text larger than the token bound goes alone, for the server to
    /// refuse. Never returns an empty range.
    pub fn batches(&self, texts: &[String]) -> Vec<std::ops::Range<usize>> {
        let max_inputs = self.max_inputs.max(1);
        let mut out = Vec::new();
        let mut start = 0;
        let mut spent = 0usize;
        for (i, text) in texts.iter().enumerate() {
            let cost = text.len() + Self::PER_INPUT_ALLOWANCE;
            let full = i - start == max_inputs || spent + cost > self.max_request_tokens;
            if i > start && full {
                out.push(start..i);
                start = i;
                spent = 0;
            }
            spent += cost;
        }
        if start < texts.len() {
            out.push(start..texts.len());
        }
        out
    }
}

/// A model that turns texts into vectors.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// The adapter's stable name, for errors and the ledger.
    fn adapter(&self) -> &'static str;

    /// The usage convention of the endpoint.
    fn protocol(&self) -> Protocol;

    /// What one request may carry. Callers batch within it.
    fn limits(&self) -> EmbedLimits {
        EmbedLimits::OPENAI
    }

    /// Embed `texts` with `model`, settling `token` with the reported usage.
    ///
    /// `texts` is one request: within [`Self::limits`]. Implementations must
    /// refuse a model they do not serve before sending, assert the response
    /// model is the one requested (or the one a documented rule maps it to),
    /// return exactly one vector per text in input order, all of one nonzero
    /// length, and raise when usage is absent.
    async fn embed(
        &self,
        model: &str,
        texts: &[String],
        token: &CallToken,
    ) -> Result<Embeddings, ProviderError>;
}

/// One document's place in a ranking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ranked {
    /// Index into the documents as given.
    pub index: usize,
    /// Relevance score, higher is more relevant. Its scale is the model's own.
    pub score: f64,
}

/// One rerank call's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Ranking {
    /// Best first.
    pub results: Vec<Ranked>,
    pub usage: TokenUsage,
}

/// The most documents one rerank request carries where the route documents no
/// tighter bound.
pub const DEFAULT_RERANK_MAX_DOCUMENTS: usize = 256;

/// A model that scores documents against a query.
#[async_trait::async_trait]
pub trait Reranker: Send + Sync {
    fn adapter(&self) -> &'static str;

    fn protocol(&self) -> Protocol;

    /// The most documents one request may carry. A server with a lower cap
    /// answers a larger request with an error, so callers refuse more before
    /// a row is opened, and implementations refuse it before sending.
    fn max_documents(&self) -> usize {
        DEFAULT_RERANK_MAX_DOCUMENTS
    }

    /// Rank `documents` against `query`, keeping the best `top_n` when given.
    ///
    /// Implementations must refuse a model they do not serve and more than
    /// [`Self::max_documents`] documents before sending, assert the response
    /// model (or state the documented reason it cannot be), refuse an index
    /// outside the documents, and raise when usage is absent.
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: Option<usize>,
        token: &CallToken,
    ) -> Result<Ranking, ProviderError>;
}

/// Cosine similarity of two vectors, or `None` when their lengths differ or
/// either has zero length: a similarity of vectors from two models, or of an
/// empty vector, is not a number to rank by.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// Rank `candidates` against `query` by cosine similarity, best first. Raises
/// (as `None`) when any vector's length differs from the query's, rather than
/// dropping that candidate.
pub fn rank_by_similarity(query: &[f32], candidates: &[Vec<f32>]) -> Option<Vec<Ranked>> {
    let mut out = candidates
        .iter()
        .enumerate()
        .map(|(index, v)| cosine_similarity(query, v).map(|score| Ranked { index, score }))
        .collect::<Option<Vec<_>>>()?;
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
    Some(out)
}

/// A configured route of any kind: `route:model`, what it returns, and how to
/// call it.
#[derive(Clone)]
pub struct ModelRoute {
    name: String,
    kind: ModelKind,
    adapter: &'static str,
    protocol: Protocol,
    model: String,
    handle: Arc<dyn Any + Send + Sync>,
    /// The route's rate limiter, shared with its generative calls when the
    /// route also serves those (`--rate-limit` is per route and key).
    limiter: Option<Arc<RouteLimiter>>,
}

impl ModelRoute {
    /// A route whose call handle is `handle` (see the module table for the
    /// handle type each kind stores).
    pub fn new<H: Any + Send + Sync>(
        name: impl Into<String>,
        kind: ModelKind,
        adapter: &'static str,
        protocol: Protocol,
        model: impl Into<String>,
        handle: H,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            adapter,
            protocol,
            model: model.into(),
            handle: Arc::new(handle),
            limiter: None,
        }
    }

    /// Pace this route's calls through `limiter`, the one its generative
    /// calls use.
    pub fn with_limiter(mut self, limiter: Arc<RouteLimiter>) -> Self {
        self.limiter = Some(limiter);
        self
    }

    /// The route's limiter, if one is configured.
    pub fn limiter(&self) -> Option<&Arc<RouteLimiter>> {
        self.limiter.as_ref()
    }

    /// Take a slot on the route's limiter for the call `token` opened, or
    /// close the token as never sent and return the refusal. A route with no
    /// limiter admits at once. Callers take this after opening the row and
    /// before calling the handle, as [`crate::RateLimited`] does for a
    /// generative call.
    pub async fn admit(&self, token: &CallToken<'_>) -> Result<(), ProviderError> {
        match &self.limiter {
            Some(limiter) => limiter.admit(self.adapter, token).await,
            None => Ok(()),
        }
    }

    /// `route:model`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The catalog route id: the name up to its first `:`. A catalog route id
    /// never holds a `:`, and a model name may.
    pub fn catalog_route(&self) -> &str {
        self.name
            .split_once(':')
            .map_or(&self.name, |(route, _)| route)
    }

    pub fn kind(&self) -> ModelKind {
        self.kind
    }

    /// The adapter name the ledger records.
    pub fn adapter(&self) -> &'static str {
        self.adapter
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// The call handle, when this route is of `kind` and holds an `H`.
    ///
    /// A route of another kind is [`KindError::Mismatch`]. A route of the
    /// right kind holding another handle type is a wiring defect, reported as
    /// the same mismatch (the route cannot serve the call either way).
    pub fn handle<H: Any>(&self, kind: ModelKind) -> Result<&H, KindError> {
        self.kind.require(kind, &self.name)?;
        self.handle
            .downcast_ref::<H>()
            .ok_or_else(|| KindError::Mismatch {
                route: self.name.clone(),
                expected: kind,
                actual: self.kind,
            })
    }
}

impl std::fmt::Debug for ModelRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRoute")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("adapter", &self.adapter)
            .field("limited", &self.limiter.is_some())
            .finish_non_exhaustive()
    }
}

/// The run's routes of every kind.
#[derive(Clone, Debug, Default)]
pub struct ModelRoutes {
    routes: Vec<ModelRoute>,
}

impl ModelRoutes {
    pub fn new(routes: Vec<ModelRoute>) -> Self {
        Self { routes }
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModelRoute> {
        self.routes.iter()
    }

    /// Routes of `kind`, in declaration order.
    pub fn of_kind(&self, kind: ModelKind) -> impl Iterator<Item = &ModelRoute> {
        self.routes.iter().filter(move |r| r.kind == kind)
    }

    /// The kinds present, in registry order.
    pub fn kinds(&self) -> Vec<ModelKind> {
        kgraf_core::KINDS
            .iter()
            .map(|spec| spec.kind)
            .filter(|kind| self.of_kind(*kind).next().is_some())
            .collect()
    }

    /// The route to call for `kind`: the named one, else the first declared.
    ///
    /// A route is found by kind and name together: one `route:model` may be
    /// declared as two kinds (a server that serves both `/embeddings` and
    /// `/rerank` for one model), and each declaration is its own route. A name
    /// declared only as another kind raises [`KindError::Mismatch`], never
    /// falls back to a route of the right kind: the caller asked for that
    /// route.
    pub fn pick(&self, kind: ModelKind, name: Option<&str>) -> Result<&ModelRoute, KindError> {
        let Some(name) = name else {
            return self.of_kind(kind).next().ok_or(KindError::NoRoute { kind });
        };
        if let Some(route) = self.of_kind(kind).find(|r| r.name == name) {
            return Ok(route);
        }
        match self.routes.iter().find(|r| r.name == name) {
            Some(other) => Err(KindError::Mismatch {
                route: other.name.clone(),
                expected: kind,
                actual: other.kind,
            }),
            None => Err(KindError::UnknownRoute {
                route: name.to_owned(),
                kind,
                known: self.names_of(kind),
            }),
        }
    }

    fn names_of(&self, kind: ModelKind) -> String {
        let names: Vec<&str> = self.of_kind(kind).map(|r| r.name.as_str()).collect();
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    }

    /// One line per kind present: `embedding: openai:text-embedding-3-small
    /// (tool `embed`; one vector per text; ...)`. What the planner is told.
    ///
    /// Every kind listed comes from the registry, so each has its row; the
    /// error is [`ModelKind::spec`]'s, kept rather than skipped.
    pub fn summary_lines(&self) -> Result<Vec<String>, KindError> {
        self.kinds()
            .into_iter()
            .map(|kind| {
                let spec = kind.spec()?;
                let tools = if spec.tools.is_empty() {
                    String::new()
                } else {
                    format!("tools {}; ", spec.tools.join(", "))
                };
                Ok(format!(
                    "{kind}: {} ({tools}{})",
                    self.names_of(kind),
                    spec.summary
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    fn routes() -> ModelRoutes {
        ModelRoutes::new(vec![
            ModelRoute::new(
                "openai:e",
                ModelKind::EMBEDDING,
                "fake",
                Protocol::Inclusive,
                "e",
                Arc::new(Fake),
            ),
            ModelRoute::new(
                "local:r",
                ModelKind::RERANK,
                "fake",
                Protocol::Inclusive,
                "r",
                Arc::new(Fake),
            ),
        ])
    }

    #[test]
    fn a_handle_is_given_only_for_its_own_kind() {
        let routes = routes();
        let route = routes.pick(ModelKind::EMBEDDING, None).expect("pick");
        route
            .handle::<Arc<Fake>>(ModelKind::EMBEDDING)
            .expect("same kind");
        assert!(matches!(
            route.handle::<Arc<Fake>>(ModelKind::GENERATIVE),
            Err(KindError::Mismatch { .. })
        ));
        // Right kind, wrong handle type: a wiring defect, still refused.
        assert!(route.handle::<String>(ModelKind::EMBEDDING).is_err());
    }

    #[test]
    fn the_catalog_route_is_the_name_before_the_first_colon() {
        let route = ModelRoute::new(
            "azure-openai:ns:model",
            ModelKind::EMBEDDING,
            "fake",
            Protocol::Inclusive,
            "ns:model",
            Arc::new(Fake),
        );
        assert_eq!(route.catalog_route(), "azure-openai");
    }

    #[test]
    fn a_named_route_of_another_kind_raises_rather_than_falling_back() {
        let err = routes()
            .pick(ModelKind::EMBEDDING, Some("local:r"))
            .expect_err("mismatch");
        assert!(matches!(err, KindError::Mismatch { .. }), "{err}");
    }

    #[test]
    fn one_route_declared_as_two_kinds_is_found_by_kind_and_name() {
        let routes = ModelRoutes::new(vec![
            ModelRoute::new(
                "local:m",
                ModelKind::EMBEDDING,
                "fake-e",
                Protocol::Inclusive,
                "m",
                Arc::new(Fake),
            ),
            ModelRoute::new(
                "local:m",
                ModelKind::RERANK,
                "fake-r",
                Protocol::Inclusive,
                "m",
                Arc::new(Fake),
            ),
        ]);
        let rerank = routes
            .pick(ModelKind::RERANK, Some("local:m"))
            .expect("the rerank declaration");
        assert_eq!(rerank.kind(), ModelKind::RERANK);
        assert_eq!(rerank.adapter(), "fake-r");
        let embed = routes
            .pick(ModelKind::EMBEDDING, Some("local:m"))
            .expect("the embedding declaration");
        assert_eq!(embed.adapter(), "fake-e");
    }

    #[test]
    fn an_absent_kind_says_how_to_declare_one() {
        let err = routes()
            .pick(ModelKind::TYPED_DECISION, None)
            .expect_err("none");
        assert!(
            err.to_string().contains("typed-decision=route:model"),
            "{err}"
        );
        let err = routes()
            .pick(ModelKind::EMBEDDING, Some("nope:x"))
            .expect_err("unknown");
        assert!(err.to_string().contains("openai:e"), "{err}");
    }

    #[test]
    fn the_summary_lists_each_kind_present_with_its_tools() {
        let lines = routes().summary_lines().expect("registered kinds");
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("embedding: openai:e (tools embed;"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("rerank: local:r (tools rerank;"),
            "{lines:?}"
        );
    }

    #[test]
    fn embedding_batches_stay_within_the_input_count_and_the_token_bound() {
        let limits = EmbedLimits {
            max_inputs: 3,
            max_request_tokens: 40,
        };
        let texts: Vec<String> = (0..7).map(|_| "x".repeat(4)).collect();
        // Each text costs 4 bytes + 8 allowance = 12: three fit in 40 by bytes
        // and by count.
        assert_eq!(limits.batches(&texts), vec![0..3, 3..6, 6..7]);
        let wide = vec!["y".repeat(30), "z".repeat(30), "w".repeat(100)];
        // 38 fits alone; 38 + 38 does not; an over-size text goes alone.
        assert_eq!(limits.batches(&wide), vec![0..1, 1..2, 2..3]);
        assert!(limits.batches(&[]).is_empty());
        let openai = EmbedLimits::OPENAI.batches(&vec!["a".to_owned(); 5_000]);
        assert_eq!(openai, vec![0..2_048, 2_048..4_096, 4_096..5_000]);
    }

    #[test]
    fn cosine_similarity_is_one_for_parallel_vectors_and_none_for_a_length_mismatch() {
        let s = cosine_similarity(&[1.0, 2.0], &[2.0, 4.0]).expect("defined");
        assert!((s - 1.0).abs() < 1e-9);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), None);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), None);
    }

    #[test]
    fn ranking_by_similarity_is_best_first_and_refuses_a_mismatched_vector() {
        let ranked =
            rank_by_similarity(&[1.0, 0.0], &[vec![0.0, 1.0], vec![1.0, 0.1]]).expect("rank");
        assert_eq!(ranked[0].index, 1);
        assert_eq!(ranked[1].index, 0);
        assert!(rank_by_similarity(&[1.0, 0.0], &[vec![1.0]]).is_none());
    }
}
