//! `POST /embeddings` and `POST /rerank`: the embedding and rerank kinds on
//! OpenAI-family endpoints and the local servers beside them (decision 0089).
//!
//! **Embeddings**, by [`EmbedDialect`]:
//! - [`EmbedDialect::OpenAi`]: OpenAI's published shape, which Google's
//!   OpenAI-compatible surface, llama.cpp's server and Azure OpenAI also serve
//!   (Together's reply has no `usage`, so no route claims it there):
//!   request `{model, input: [..]}` at `{base_url}/embeddings`, response
//!   `data[].{index, embedding}`, `model`, and `usage.{prompt_tokens,
//!   total_tokens}` (<https://platform.openai.com/docs/api-reference/embeddings/create>).
//!   An input over the model's limit is refused by the server (HTTP 400).
//! - [`EmbedDialect::OllamaNative`]: Ollama's `POST /api/embed` with
//!   `truncate: false`, response `{model, embeddings, prompt_eval_count}`
//!   (<https://github.com/ollama/ollama/blob/main/docs/api.md#generate-embeddings>).
//!   Ollama's OpenAI-compatible `/v1/embeddings` forwards no `truncate` field,
//!   so the server's default applies, which cuts an over-long input to the
//!   context length and says nothing. The native endpoint refuses it instead.
//! - [`EmbedDialect::TeiNative`]: Text Embeddings Inference's `POST /embed`
//!   with `truncate: false`, response a bare array of vectors, token count in
//!   the `x-compute-tokens` header
//!   (<https://huggingface.github.io/text-embeddings-inference/openapi.json>).
//!   TEI's OpenAI-compatible `/v1/embeddings` takes no `truncate` field and
//!   truncates whenever the server runs with `--auto-truncate`, its default.
//!   TEI serves one model per server and names none in the reply, so the
//!   model echo cannot be checked on this dialect.
//!
//! **Rerank**, by [`RerankDialect`]:
//! - [`RerankDialect::Common`]: the shape vLLM, Jina, llama.cpp and Together
//!   share: request `{model, query, documents, top_n}` at `{base_url}/rerank`,
//!   response `results[].{index, relevance_score}`, `model`, and
//!   `usage.total_tokens` (<https://docs.vllm.ai/en/latest/serving/openai_compatible_server.html>,
//!   <https://docs.together.ai/reference/rerank-1>).
//! - [`RerankDialect::TeiNative`]: TEI's `POST /rerank`, request `{query,
//!   texts, truncate: false}`, response `[{index, score}]`, token count in the
//!   `x-compute-tokens` header. No model echo, as for TEI embeddings.
//!
//! Cohere's `POST /v2/rerank` is not served: it bills in search units, not
//! tokens (`meta.billed_units.search_units`), which the token ledger cannot
//! price, and its reply names no model
//! (<https://docs.cohere.com/reference/rerank>). OpenAI serves no rerank
//! endpoint.
//!
//! **Azure OpenAI** (decision 0092) serves OpenAI's embeddings dialect through
//! [`crate::azure::AzureTransport::embeddings`]: its URL, `api-key` header and
//! deployment mapping, with the reply's `model` checked against the model the
//! deployment is mapped to.
//!
//! All of these are input-only: the whole usage figure is uncached input, and
//! no output tokens exist. Each refuses a model outside the configured set, an
//! empty text, and more documents than the route takes, before sending;
//! asserts the response model where the reply names one (unless the route's
//! vendor documents a reply model that differs from the request, see
//! [`EchoRule::NotCompared`]); and raises on a missing or zero usage figure
//! rather than settling zero (fail loud). Every reply's rate-limit headers are
//! recorded under the model's own key (OpenAI limits are per model; Azure's
//! per deployment), and a 429 is recorded for the machine's learned rate, as
//! the chat adapter does (decision 0067).
//!
//! The ledger closing follows the chat adapter: a refused or unsent request
//! closes as not billed; a response that arrived but cannot be used stays open
//! as unreconciled, since the call may have billed.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::provider::assert_model_echo;
use kgraf_provider::{
    CallToken, EmbedLimits, Embedder, Embeddings, ProviderConfig, ProviderError, Ranked, Ranking,
    Reranker,
};

/// The embeddings adapter's stable name.
pub const EMBEDDINGS_ADAPTER: &str = "openai-embeddings";
/// The rerank adapter's stable name.
pub const RERANK_ADAPTER: &str = "openai-rerank";

/// The response header in which Text Embeddings Inference reports the tokens
/// a request used.
pub const TEI_TOKENS_HEADER: &str = "x-compute-tokens";

/// Which request and reply shape an embedding route speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmbedDialect {
    /// `POST {base_url}/embeddings`, OpenAI's shape.
    #[default]
    OpenAi,
    /// `POST {server}/api/embed` with `truncate: false`.
    OllamaNative,
    /// `POST {server}/embed` with `truncate: false`.
    TeiNative,
}

/// Which request and reply shape a rerank route speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RerankDialect {
    /// `POST {base_url}/rerank`, the shape vLLM, Jina and Together share.
    #[default]
    Common,
    /// `POST {server}/rerank`, TEI's `{query, texts}` shape.
    TeiNative,
}

/// A decoded reply and what the transport said about it.
struct Reply {
    body: serde_json::Value,
    request_id: Option<String>,
    /// The `x-compute-tokens` header, where the server sent one.
    header_tokens: Option<u64>,
}

/// How a reply's `model` is checked against the model requested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EchoRule {
    /// The reply must name the model requested (or, on Azure, the model the
    /// deployment is mapped to). Any other name raises.
    #[default]
    Requested,
    /// The vendor documents a reply `model` that is not the requested name
    /// (a serving checkpoint's own name) and no rule relating the two, so the
    /// name is not compared: comparing it would raise on every call after it
    /// billed. Set only by a catalog claim that cites the vendor's page.
    NotCompared,
}

/// One JSON POST with the configured credential.
struct Poster {
    config: ProviderConfig,
    http: reqwest::Client,
    /// Azure OpenAI, when set: its URL, `api-key` header, deployment mapping
    /// and per-deployment limit key replace the defaults.
    azure: Option<crate::azure::AzureTransport>,
}

/// Rate-limit header capture (decision 0067).
///
/// OpenAI states its rate limits per model, so an embedding model's headers
/// describe that model, not the chat model on the same base URL: they are
/// recorded under `scoped_key(base_url, model)`, never under the base URL the
/// chat adapter records under. A 429 is recorded under both, since it is the
/// route that was refused, and the route's learned rate reads the base URL.
/// Azure limits are per deployment, and its headers are read with the Azure
/// rules under the deployment's own key.
fn observe_rate_limit_headers(poster: &Poster, model: &str, http: &reqwest::Response) {
    let headers = http.headers().clone();
    let get = move |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let refused = http.status().as_u16() == 429;
    match &poster.azure {
        Some(transport) => {
            let key = transport.limit_key(model);
            kgraf_provider::limits::record(&key, kgraf_provider::limits::parse_azure_style(&get));
            if refused {
                kgraf_provider::limits::record_429_now(&key);
            }
        }
        None => {
            let base = poster.config.base_url.trim_end_matches('/');
            let key = kgraf_provider::limits::scoped_key(base, model);
            kgraf_provider::limits::record(&key, kgraf_provider::limits::parse_openai_style(&get));
            if refused {
                kgraf_provider::limits::record_429_now(&key);
                kgraf_provider::limits::record_429_now(base);
            }
        }
    }
}

impl Poster {
    fn new(adapter: &'static str, config: ProviderConfig) -> Result<Self, ProviderError> {
        if config.protocol != Protocol::Inclusive {
            return Err(ProviderError::CapabilityNotHonored {
                adapter,
                capability: "inclusive usage accounting",
            });
        }
        let http = reqwest::Client::builder()
            .connect_timeout(config.timeout)
            .build()
            .map_err(|e| ProviderError::Transport {
                adapter,
                reason: e.to_string(),
            })?;
        Ok(Self {
            config,
            http,
            azure: None,
        })
    }

    /// The model a reply must name for a request to `requested`: the mapped
    /// underlying model on Azure, else the model itself.
    fn echo_target<'a>(
        &'a self,
        adapter: &'static str,
        requested: &'a str,
    ) -> Result<&'a str, ProviderError> {
        match &self.azure {
            Some(transport) => transport.underlying(adapter, requested),
            None => Ok(requested),
        }
    }

    /// `{base_url}/{path}`.
    fn at_base(&self, path: &str) -> String {
        format!("{}/{path}", self.config.base_url.trim_end_matches('/'))
    }

    /// `{server}/{path}`, where the server root is the base URL without its
    /// trailing `/v1`: the native endpoints of Ollama and TEI sit beside their
    /// OpenAI-compatible ones, not under them.
    fn at_server(&self, adapter: &'static str, path: &str) -> Result<String, ProviderError> {
        let base = self.config.base_url.trim_end_matches('/');
        let root = base
            .strip_suffix("/v1")
            .ok_or(ProviderError::CapabilityNotHonored {
                adapter,
                capability: "a native endpoint beside a base URL that does not end in /v1",
            })?;
        Ok(format!("{root}/{path}"))
    }

    /// Send `body` to `url` and return the parsed JSON reply, closing `token`
    /// on every failure path. On success the token is still open: the caller
    /// settles it once the reply has been decoded.
    async fn post(
        &self,
        adapter: &'static str,
        url: String,
        model: &str,
        body: &serde_json::Value,
        token: &CallToken<'_>,
    ) -> Result<Reply, ProviderError> {
        if let Err(error) = self
            .config
            .check_model(adapter, model)
            .and_then(|()| self.echo_target(adapter, model).map(|_| ()))
        {
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let mut builder = self.http.post(url).json(body);
        match &self.azure {
            Some(transport) => {
                if let Some(query) = transport.query() {
                    builder = builder.query(&[query]);
                }
                if let Some(key) = self.config.credential() {
                    match crate::azure::key_header(adapter, key) {
                        Ok(value) => builder = builder.header("api-key", value),
                        Err(error) => {
                            token.close_on_async(&error, None).await?;
                            return Err(error);
                        }
                    }
                }
            }
            None => {
                if let Some(credential) = self.config.credential() {
                    builder = builder.bearer_auth(credential);
                }
            }
        }
        let http = match tokio::time::timeout(self.config.timeout, builder.send()).await {
            Ok(Ok(http)) => http,
            Ok(Err(e)) => {
                let error = crate::send_error(adapter, e);
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
            Err(_) => {
                let error = ProviderError::Transport {
                    adapter,
                    reason: format!(
                        "no response within {:?} of sending the request",
                        self.config.timeout
                    ),
                };
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        observe_rate_limit_headers(self, model, &http);
        let status = http.status();
        let request_id = http
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let header_tokens = http
            .headers()
            .get(TEI_TOKENS_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if !status.is_success() {
            let text = crate::bounded_error_body(self.config.timeout, http).await;
            let error = ProviderError::Http {
                adapter,
                status: status.as_u16(),
                body: text,
            };
            token.close_on_async(&error, request_id.as_deref()).await?;
            return Err(error);
        }
        let text = match tokio::time::timeout(self.config.timeout, http.text()).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter,
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter,
                    reason: format!(
                        "response body did not finish within {:?}",
                        self.config.timeout
                    ),
                });
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Decode {
                    adapter,
                    reason: e.to_string(),
                });
            }
        };
        if let Some(error) = parsed.get("error").filter(|e| !e.is_null()) {
            token
                .leave_unreconciled_async(request_id.as_deref())
                .await?;
            return Err(ProviderError::Decode {
                adapter,
                reason: format!("HTTP 200 with an `error` object in the body: {error}"),
            });
        }
        Ok(Reply {
            body: parsed,
            request_id,
            header_tokens,
        })
    }
}

/// Input-only usage from a reply's `usage` object: `prompt_tokens`, else
/// `total_tokens`. Absent is [`ProviderError::UsageMissing`], and so is zero:
/// every request that reaches a server carries at least one non-empty text
/// (empty ones are refused before sending), which cannot take zero tokens, so
/// a zero is a count the server did not make, not a free call.
fn body_usage(
    adapter: &'static str,
    body: &serde_json::Value,
) -> Result<TokenUsage, ProviderError> {
    let usage = body
        .get("usage")
        .filter(|u| u.is_object())
        .ok_or(ProviderError::UsageMissing { adapter })?;
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("total_tokens"))
        .and_then(serde_json::Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or(ProviderError::UsageMissing { adapter })?;
    Ok(TokenUsage {
        uncached_input: input,
        raw: Some(usage.clone()),
        ..TokenUsage::default()
    })
}

/// Input-only usage from a named top-level count (Ollama's
/// `prompt_eval_count`). Absent or zero is [`ProviderError::UsageMissing`], as
/// for [`body_usage`]: Ollama's reference does not promise the count on a
/// reply to several inputs, and a zero for non-empty input would settle a real
/// call as free.
fn count_usage(
    adapter: &'static str,
    body: &serde_json::Value,
    field: &str,
) -> Result<TokenUsage, ProviderError> {
    let input = body
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or(ProviderError::UsageMissing { adapter })?;
    Ok(TokenUsage {
        uncached_input: input,
        raw: Some(serde_json::json!({ field: input })),
        ..TokenUsage::default()
    })
}

/// Input-only usage from TEI's `x-compute-tokens` header. Absent or zero is
/// [`ProviderError::UsageMissing`], as for [`body_usage`].
fn header_usage(adapter: &'static str, reply: &Reply) -> Result<TokenUsage, ProviderError> {
    let input = reply
        .header_tokens
        .filter(|n| *n > 0)
        .ok_or(ProviderError::UsageMissing { adapter })?;
    Ok(TokenUsage {
        uncached_input: input,
        raw: Some(serde_json::json!({ TEI_TOKENS_HEADER: input })),
        ..TokenUsage::default()
    })
}

fn decode(adapter: &'static str, reason: impl Into<String>) -> ProviderError {
    ProviderError::Decode {
        adapter,
        reason: reason.into(),
    }
}

/// Settle on a decoded reply. A reply that cannot be used, or carries no
/// usage, may still have billed: its row stays open as unreconciled, with the
/// usage the reply carried when it carried one.
async fn finish<T>(
    token: &CallToken<'_>,
    request_id: Option<&str>,
    decoded: Result<T, ProviderError>,
    usage: Result<TokenUsage, ProviderError>,
) -> Result<(T, TokenUsage), ProviderError> {
    match (decoded, usage) {
        (Ok(value), Ok(usage)) => {
            token.settle_async(&usage, request_id).await?;
            Ok((value, usage))
        }
        (Ok(_), Err(missing)) => {
            token
                .leave_unreconciled_with_async(None, request_id)
                .await?;
            Err(missing)
        }
        (Err(error), usage) => {
            token
                .leave_unreconciled_with_async(usage.ok().as_ref(), request_id)
                .await?;
            Err(error)
        }
    }
}

/// The reply's `model`, checked against the one requested.
fn echo(adapter: &'static str, model: &str, body: &serde_json::Value) -> Result<(), ProviderError> {
    let returned = body
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| decode(adapter, "reply has no `model` field"))?;
    assert_model_echo(model, returned)
}

/// One vector from a JSON array of numbers.
fn vector(position: usize, value: &serde_json::Value) -> Result<Vec<f32>, ProviderError> {
    value
        .as_array()
        .ok_or_else(|| {
            decode(
                EMBEDDINGS_ADAPTER,
                format!(
                    "row {position} has no embedding array (a base64 reply is not requested and \
                     not read)"
                ),
            )
        })?
        .iter()
        .map(|x| x.as_f64().map(|x| x as f32))
        .collect::<Option<Vec<f32>>>()
        .ok_or_else(|| {
            decode(
                EMBEDDINGS_ADAPTER,
                format!("row {position} holds a non-number"),
            )
        })
}

/// Every vector nonempty and of one length: vectors of mixed lengths come
/// from mixed models or a broken server, and none of them can be compared.
fn check_widths(vectors: &[Vec<f32>]) -> Result<(), ProviderError> {
    let Some(first) = vectors.first() else {
        return Ok(());
    };
    if first.is_empty() {
        return Err(decode(EMBEDDINGS_ADAPTER, "row 0 is an empty vector"));
    }
    match vectors.iter().position(|v| v.len() != first.len()) {
        Some(row) => Err(decode(
            EMBEDDINGS_ADAPTER,
            format!(
                "row {row} has {} dimensions where row 0 has {}",
                vectors[row].len(),
                first.len()
            ),
        )),
        None => Ok(()),
    }
}

/// An embedding route of any [`EmbedDialect`].
pub struct OpenAiEmbeddings {
    poster: Poster,
    dialect: EmbedDialect,
    limits: EmbedLimits,
}

impl OpenAiEmbeddings {
    /// OpenAI's dialect, with OpenAI's request bounds.
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            poster: Poster::new(EMBEDDINGS_ADAPTER, config)?,
            dialect: EmbedDialect::OpenAi,
            limits: EmbedLimits::OPENAI,
        })
    }

    /// Speak `dialect` instead. A native dialect needs a base URL ending in
    /// `/v1`, whose parent is the server root; any other is refused here,
    /// before a call.
    pub fn with_dialect(mut self, dialect: EmbedDialect) -> Result<Self, ProviderError> {
        if dialect != EmbedDialect::OpenAi {
            self.poster.at_server(EMBEDDINGS_ADAPTER, "")?;
        }
        self.dialect = dialect;
        Ok(self)
    }

    /// Bound each request by `limits` (see [`Embedder::limits`]).
    pub fn with_limits(mut self, limits: EmbedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Serve this adapter's body through Azure OpenAI (decision 0092): the
    /// transport's URL, `api-key` header and deployment mapping replace the
    /// defaults, and the reply must name the model the deployment is mapped
    /// to. Every configured model must be a mapped deployment, and only
    /// OpenAI's dialect is served there; anything else is refused here.
    pub fn with_azure(
        mut self,
        transport: crate::azure::AzureTransport,
    ) -> Result<Self, ProviderError> {
        if self.dialect != EmbedDialect::OpenAi {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: transport.name(),
                capability: "a native embedding dialect on Azure OpenAI",
            });
        }
        transport.check_models(transport.name(), &self.poster.config.models)?;
        self.poster.azure = Some(transport);
        Ok(self)
    }

    /// The request body in OpenAI's dialect.
    pub fn to_wire(model: &str, texts: &[String]) -> serde_json::Value {
        serde_json::json!({ "model": model, "input": texts })
    }

    /// The vectors of an OpenAI-shaped reply, in input order. Every input
    /// index must appear exactly once, and every vector must be nonempty and
    /// of one length.
    pub fn parse(
        model: &str,
        expected: usize,
        body: &serde_json::Value,
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        echo(EMBEDDINGS_ADAPTER, model, body)?;
        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| decode(EMBEDDINGS_ADAPTER, "reply has no `data` array"))?;
        if data.len() != expected {
            return Err(decode(
                EMBEDDINGS_ADAPTER,
                format!("{} vectors for {expected} texts", data.len()),
            ));
        }
        let mut out: Vec<Option<Vec<f32>>> = vec![None; expected];
        for (position, row) in data.iter().enumerate() {
            // `index` is in the published shape; where a server omits it, the
            // position stands in, since the published order is input order.
            let index = row
                .get("index")
                .and_then(|i| i.as_u64())
                .map_or(position, |i| i as usize);
            let vector = vector(
                position,
                row.get("embedding").unwrap_or(&serde_json::Value::Null),
            )?;
            let slot = out.get_mut(index).ok_or_else(|| {
                decode(
                    EMBEDDINGS_ADAPTER,
                    format!("index {index} is outside the input"),
                )
            })?;
            if slot.replace(vector).is_some() {
                return Err(decode(
                    EMBEDDINGS_ADAPTER,
                    format!("index {index} appears twice"),
                ));
            }
        }
        let vectors = out
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| decode(EMBEDDINGS_ADAPTER, "an input has no vector"))?;
        check_widths(&vectors)?;
        Ok(vectors)
    }

    /// The vectors of a reply that is (or holds) a bare array of vectors in
    /// input order: Ollama's `embeddings`, TEI's whole body.
    pub fn parse_rows(
        expected: usize,
        rows: &serde_json::Value,
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        let rows = rows
            .as_array()
            .ok_or_else(|| decode(EMBEDDINGS_ADAPTER, "reply has no array of vectors"))?;
        if rows.len() != expected {
            return Err(decode(
                EMBEDDINGS_ADAPTER,
                format!("{} vectors for {expected} texts", rows.len()),
            ));
        }
        let vectors = rows
            .iter()
            .enumerate()
            .map(|(position, row)| vector(position, row))
            .collect::<Result<Vec<_>, _>>()?;
        check_widths(&vectors)?;
        Ok(vectors)
    }
}

#[async_trait::async_trait]
impl Embedder for OpenAiEmbeddings {
    fn adapter(&self) -> &'static str {
        self.poster
            .azure
            .as_ref()
            .map_or(EMBEDDINGS_ADAPTER, |t| t.name())
    }

    fn protocol(&self) -> Protocol {
        self.poster.config.protocol
    }

    fn limits(&self) -> EmbedLimits {
        self.limits
    }

    async fn embed(
        &self,
        model: &str,
        texts: &[String],
        token: &CallToken,
    ) -> Result<Embeddings, ProviderError> {
        let refusal = if texts.is_empty() {
            Some("no texts to embed".to_owned())
        } else if let Some(i) = texts.iter().position(|t| t.is_empty()) {
            // OpenAI's reference: the input "cannot be an empty string".
            Some(format!("text {i} is empty; an empty text has no embedding"))
        } else if texts.len() > self.limits.max_inputs {
            Some(format!(
                "{} texts in one request where this route takes at most {}; split them",
                texts.len(),
                self.limits.max_inputs
            ))
        } else {
            None
        };
        if let Some(reason) = refusal {
            // Refused before sending: the row closes as not billed.
            let error = ProviderError::Unsent {
                adapter: self.adapter(),
                reason,
            };
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let (url, body) = match (&self.poster.azure, self.dialect) {
            (Some(transport), _) => {
                let mut body = Self::to_wire(model, texts);
                if !transport.body_names_model()
                    && let Some(object) = body.as_object_mut()
                {
                    // The deployment is in the URL on the dated surface.
                    object.remove("model");
                }
                (transport.url(model), body)
            }
            (None, EmbedDialect::OpenAi) => (
                self.poster.at_base("embeddings"),
                Self::to_wire(model, texts),
            ),
            (None, EmbedDialect::OllamaNative) => (
                self.poster.at_server(EMBEDDINGS_ADAPTER, "api/embed")?,
                serde_json::json!({ "model": model, "input": texts, "truncate": false }),
            ),
            (None, EmbedDialect::TeiNative) => (
                self.poster.at_server(EMBEDDINGS_ADAPTER, "embed")?,
                serde_json::json!({ "inputs": texts, "truncate": false }),
            ),
        };
        let reply = self
            .poster
            .post(self.adapter(), url, model, &body, token)
            .await?;
        // Checked before sending (`Poster::post`); an error here still goes
        // through `finish`, since the reply arrived and may have billed.
        let target = self.poster.echo_target(self.adapter(), model);
        let (decoded, usage) = match self.dialect {
            EmbedDialect::OpenAi => (
                target.and_then(|target| Self::parse(target, texts.len(), &reply.body)),
                body_usage(EMBEDDINGS_ADAPTER, &reply.body),
            ),
            EmbedDialect::OllamaNative => (
                echo(EMBEDDINGS_ADAPTER, model, &reply.body).and_then(|()| {
                    Self::parse_rows(
                        texts.len(),
                        reply
                            .body
                            .get("embeddings")
                            .unwrap_or(&serde_json::Value::Null),
                    )
                }),
                count_usage(EMBEDDINGS_ADAPTER, &reply.body, "prompt_eval_count"),
            ),
            EmbedDialect::TeiNative => (
                Self::parse_rows(texts.len(), &reply.body),
                header_usage(EMBEDDINGS_ADAPTER, &reply),
            ),
        };
        let (vectors, usage) = finish(token, reply.request_id.as_deref(), decoded, usage).await?;
        Ok(Embeddings { vectors, usage })
    }
}

/// A rerank route of any [`RerankDialect`].
pub struct OpenAiRerank {
    poster: Poster,
    dialect: RerankDialect,
    echo: EchoRule,
    max_documents: usize,
}

impl OpenAiRerank {
    /// The common dialect.
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            poster: Poster::new(RERANK_ADAPTER, config)?,
            dialect: RerankDialect::Common,
            echo: EchoRule::Requested,
            max_documents: kgraf_provider::DEFAULT_RERANK_MAX_DOCUMENTS,
        })
    }

    /// Check the reply's `model` by `rule` (see [`EchoRule`]).
    pub fn with_echo(mut self, rule: EchoRule) -> Self {
        self.echo = rule;
        self
    }

    /// Refuse a request of more than `max` documents before it is sent: a
    /// server with a lower cap (TEI's default `--max-client-batch-size` is
    /// 32) answers a larger one with an error.
    pub fn with_max_documents(mut self, max: usize) -> Self {
        self.max_documents = max.max(1);
        self
    }

    /// Speak `dialect` instead; a native dialect needs a base URL ending in
    /// `/v1`, refused here otherwise.
    pub fn with_dialect(mut self, dialect: RerankDialect) -> Result<Self, ProviderError> {
        if dialect != RerankDialect::Common {
            self.poster.at_server(RERANK_ADAPTER, "")?;
        }
        self.dialect = dialect;
        Ok(self)
    }

    /// The request body in the common dialect. `top_n` is sent only when set,
    /// since servers differ on what an absent value means and none of them
    /// rejects its absence.
    pub fn to_wire(
        model: &str,
        query: &str,
        documents: &[String],
        top_n: Option<usize>,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": model,
            "query": query,
            "documents": documents,
        });
        if let Some(n) = top_n {
            body["top_n"] = serde_json::json!(n);
        }
        body
    }

    /// The ranking of a common-dialect reply, best first.
    pub fn parse(
        model: &str,
        documents: usize,
        top_n: Option<usize>,
        body: &serde_json::Value,
    ) -> Result<Vec<Ranked>, ProviderError> {
        echo(RERANK_ADAPTER, model, body)?;
        let rows = body
            .get("results")
            .ok_or_else(|| decode(RERANK_ADAPTER, "reply has no `results` array"))?;
        Self::rank_rows(documents, top_n, rows, "relevance_score")
    }

    /// The ranking of a TEI reply (a bare `[{index, score}]` array), best
    /// first.
    pub fn parse_tei(
        documents: usize,
        top_n: Option<usize>,
        body: &serde_json::Value,
    ) -> Result<Vec<Ranked>, ProviderError> {
        Self::rank_rows(documents, top_n, body, "score")
    }

    /// Rows `{index, <score>}`, checked and sorted best first. Every index
    /// must fall inside the documents and appear once. A reply longer than the
    /// `top_n` asked for (a server that ignores `top_n`, or has none, as TEI)
    /// is cut to the best `top_n` with a notice: the call was billed and its
    /// best rows are what was asked for, so refusing them would waste it.
    fn rank_rows(
        documents: usize,
        top_n: Option<usize>,
        rows: &serde_json::Value,
        score_field: &str,
    ) -> Result<Vec<Ranked>, ProviderError> {
        let rows = rows
            .as_array()
            .ok_or_else(|| decode(RERANK_ADAPTER, "reply has no array of results"))?;
        let mut seen = vec![false; documents];
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let index = row
                .get("index")
                .and_then(|i| i.as_u64())
                .ok_or_else(|| decode(RERANK_ADAPTER, "a result has no `index`"))?
                as usize;
            let score = row
                .get(score_field)
                .and_then(|s| s.as_f64())
                .filter(|s| s.is_finite())
                .ok_or_else(|| {
                    decode(
                        RERANK_ADAPTER,
                        format!("result {index} has no finite `{score_field}`"),
                    )
                })?;
            match seen.get_mut(index) {
                Some(slot) if !*slot => *slot = true,
                Some(_) => {
                    return Err(decode(
                        RERANK_ADAPTER,
                        format!("index {index} appears twice"),
                    ));
                }
                None => {
                    return Err(decode(
                        RERANK_ADAPTER,
                        format!("index {index} is outside the {documents} documents"),
                    ));
                }
            }
            out.push(Ranked { index, score });
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
        let limit = top_n.unwrap_or(documents).min(documents);
        if out.len() > limit {
            eprintln!(
                "kgraf: the rerank reply held {} results where {limit} were asked for; kept the \
                 best {limit}",
                out.len()
            );
            out.truncate(limit);
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Reranker for OpenAiRerank {
    fn adapter(&self) -> &'static str {
        RERANK_ADAPTER
    }

    fn protocol(&self) -> Protocol {
        self.poster.config.protocol
    }

    fn max_documents(&self) -> usize {
        self.max_documents
    }

    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: Option<usize>,
        token: &CallToken,
    ) -> Result<Ranking, ProviderError> {
        let refusal = if documents.is_empty() || top_n == Some(0) {
            Some("no documents to rank".to_owned())
        } else if documents.len() > self.max_documents {
            Some(format!(
                "{} documents in one request where this route takes at most {}",
                documents.len(),
                self.max_documents
            ))
        } else {
            None
        };
        if let Some(reason) = refusal {
            // Refused before sending: the row closes as not billed.
            let error = ProviderError::Unsent {
                adapter: RERANK_ADAPTER,
                reason,
            };
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let (url, body) = match self.dialect {
            RerankDialect::Common => (
                self.poster.at_base("rerank"),
                Self::to_wire(model, query, documents, top_n),
            ),
            RerankDialect::TeiNative => (
                self.poster.at_server(RERANK_ADAPTER, "rerank")?,
                serde_json::json!({ "query": query, "texts": documents, "truncate": false }),
            ),
        };
        let reply = self
            .poster
            .post(RERANK_ADAPTER, url, model, &body, token)
            .await?;
        let (decoded, usage) = match self.dialect {
            RerankDialect::Common => (
                match self.echo {
                    EchoRule::Requested => Self::parse(model, documents.len(), top_n, &reply.body),
                    EchoRule::NotCompared => reply
                        .body
                        .get("results")
                        .ok_or_else(|| decode(RERANK_ADAPTER, "reply has no `results` array"))
                        .and_then(|rows| {
                            Self::rank_rows(documents.len(), top_n, rows, "relevance_score")
                        }),
                },
                body_usage(RERANK_ADAPTER, &reply.body),
            ),
            RerankDialect::TeiNative => (
                Self::parse_tei(documents.len(), top_n, &reply.body),
                header_usage(RERANK_ADAPTER, &reply),
            ),
        };
        let (results, usage) = finish(token, reply.request_id.as_deref(), decoded, usage).await?;
        Ok(Ranking { results, usage })
    }
}
