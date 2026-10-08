//! The Gemini `generateContent` adapter.
//!
//! One adapter serves two hosts that speak the same body:
//!
//! - the Gemini Developer API, `POST {base}/models/{model}:generateContent`,
//!   authenticated with an API key in the `x-goog-api-key` header (the key never
//!   appears in a URL, so it cannot leak through a request-error string);
//! - Vertex AI, `POST .../publishers/google/models/{model}:generateContent`,
//!   authenticated with an OAuth bearer token.
//!
//! Streaming is `:streamGenerateContent?alt=sse` on both. Wire references:
//!
//! - <https://ai.google.dev/api/generate-content>
//! - <https://ai.google.dev/api/caching#FunctionDeclaration>
//! - <https://ai.google.dev/api/caching#FunctionResponse>
//! - <https://ai.google.dev/gemini-api/docs/thinking>
//! - <https://ai.google.dev/gemini-api/docs/function-calling>
//! - <https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/inference>
//!
//! Protocol specifics handled deliberately:
//!
//! - **Usage is inclusive.** `promptTokenCount` contains
//!   `cachedContentTokenCount`, so the cached share is carved out of the input.
//! - **Thinking tokens sit outside `candidatesTokenCount`.** They are
//!   `thoughtsTokenCount`, billed at the output rate, and recorded as reasoning
//!   output. `totalTokenCount` is cross-checked: a total above the counted parts
//!   is billed as more reasoning, a total below them is refused.
//! - **A function call has no stop reason of its own.** The candidate ends with
//!   `STOP`; the adapter reports `ToolUse` when the turn carries a call.
//! - **A function response names the function, not a call id.** The adapter
//!   resolves the name from the tool call earlier in the history.
//! - **Thought signatures** returned with a function call or a text part travel
//!   with the history (`Content::ToolUse::signature`, `Content::Text::signature`),
//!   so a resumed run or a new process sends them back unchanged. A token is valid
//!   only for the model that made it, so it carries the adapter and model as its
//!   producer and is sent only when they match. A call with none, or with another
//!   producer's, gets the documented skip value, `skip_thought_signature_validator`,
//!   and only on a model family that validates signatures and only on the first
//!   call of a step, where the service checks. Reference:
//!   <https://cloud.google.com/vertex-ai/generative-ai/docs/thought-signatures>.
//!   A signature that trails the text, even on an empty text part, joins the last
//!   text block, and a signed part is never merged with an unsigned one. Thought
//!   summaries are not kept with their signatures: the history has no place for
//!   one, and only a function call's token is validated.
//! - **Two function-declaration dialects.** The Developer API takes full JSON
//!   Schema in `parametersJsonSchema`; Vertex's function-calling guide documents
//!   only `parameters`, an OpenAPI subset, so the schema is converted there and a
//!   construct the subset cannot express is refused, naming the tool and the node.
//!   Vertex documents no `id` on a function call or response either, so none is
//!   sent. Function names start with a letter or underscore and use letters,
//!   digits, underscore, dot, colon and dash, up to 128 characters on the
//!   Developer API and 64 on Vertex.
//! - **Every finish reason is named.** An unknown one raises after the call is
//!   settled.
//!
//! Unverified against a live endpoint: the request and response shapes are
//! written from the published references above and tested against recorded
//! bodies.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::{Capabilities, Reasoning, StreamEvent, Streaming};
use kgraf_provider::message::ReasoningEffort;
use kgraf_provider::{
    CallToken, Content, ProviderConfig, ProviderError, Request, Response, Role, Signature,
    SseDecoder, StopReason, StreamGuard,
};
use kgraf_provider_anthropic::BearerSource;

/// The adapter name for the Gemini Developer API.
pub const GEMINI_ADAPTER: &str = "gemini-generate-content";
/// The adapter name for Gemini on Vertex AI.
pub const VERTEX_GEMINI_ADAPTER: &str = "vertex-generate-content";

/// Prefix of a tool-call id this adapter made up because the response carried
/// none. Such an id is never sent back to the model.
const SYNTHETIC_ID_PREFIX: &str = "gemini-call-";

/// The value Google documents for a function call no Gemini model produced, so
/// it has no signature to return.
/// <https://cloud.google.com/vertex-ai/generative-ai/docs/thought-signatures>
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// The longest function name each endpoint accepts.
/// <https://ai.google.dev/api/generate-content#FunctionDeclaration> and
/// <https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/function-calling>
const MAX_FUNCTION_NAME_GEMINI: usize = 128;
const MAX_FUNCTION_NAME_VERTEX: usize = 64;

/// The smallest thinking budget the model accepts.
/// <https://cloud.google.com/vertex-ai/generative-ai/docs/thinking> lists 512
/// for Gemini 2.5 Flash-Lite, 128 for Gemini 2.5 Pro and 1 for Gemini 2.5
/// Flash. A model the table does not name gets 128, the larger of the two
/// documented floors that are not Flash-Lite's, so a budget is never below any
/// documented minimum but Flash-Lite's.
fn min_thinking_budget(model: &str) -> u64 {
    if bare_model(model).starts_with("gemini-2.5-flash-lite") {
        512
    } else {
        128
    }
}

/// How the reasoning effort maps onto the model's thinking control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingStyle {
    /// `thinkingConfig.thinkingBudget`, a token count (Gemini 2.5 family).
    Budget,
    /// `thinkingConfig.thinkingLevel`, a named level (Gemini 3 family).
    Level,
}

impl ThinkingStyle {
    /// The style for a model name: the 3 family takes a level, the 2.5 family a
    /// budget. Any other family is `None`: its control is not known, so a
    /// request for reasoning effort on it is refused rather than guessed.
    pub fn for_model(model: &str) -> Option<Self> {
        let model = bare_model(model);
        if model.starts_with("gemini-3") {
            Some(ThinkingStyle::Level)
        } else if model.starts_with("gemini-2.5") {
            Some(ThinkingStyle::Budget)
        } else {
            None
        }
    }

    fn config(
        self,
        adapter: &'static str,
        model: &str,
        effort: ReasoningEffort,
        max_output_tokens: u32,
        overridden: bool,
    ) -> Result<serde_json::Value, ProviderError> {
        // Some models in these families cannot turn thinking off at all, and
        // no off form has been exchanged with the running service, so none is
        // sent on a guess.
        let off_refused = ProviderError::CapabilityNotHonored {
            adapter,
            capability: "reasoning off (no off form is verified on this protocol)",
        };
        match self {
            ThinkingStyle::Budget => {
                let fixed: u64 = match effort {
                    ReasoningEffort::Off => return Err(off_refused),
                    ReasoningEffort::Low => 1024,
                    ReasoningEffort::Medium => 8192,
                    ReasoningEffort::High => 24576,
                };
                // Thinking tokens count against `maxOutputTokens`; a budget at the
                // ceiling leaves no room for the answer and ends in MAX_TOKENS
                // with no text. Half the ceiling is reserved for the answer.
                let budget = fixed.min(u64::from(max_output_tokens) / 2);
                if budget < min_thinking_budget(model) {
                    return Err(ProviderError::CapabilityNotHonored {
                        adapter,
                        capability: "reasoning effort with an output limit too small for the \
                                     model's minimum thinking budget (twice the minimum: 256 \
                                     tokens, 1024 on Gemini 2.5 Flash-Lite)",
                    });
                }
                Ok(serde_json::json!({
                    "thinkingBudget": budget,
                    "includeThoughts": true,
                }))
            }
            ThinkingStyle::Level => {
                let level = match effort {
                    ReasoningEffort::Off => return Err(off_refused),
                    ReasoningEffort::Low => "low",
                    ReasoningEffort::Medium => "medium",
                    ReasoningEffort::High => "high",
                };
                if !overridden && !supported_levels(model).contains(&level) {
                    return Err(ProviderError::CapabilityNotHonored {
                        adapter,
                        capability: "this reasoning effort on this model (its documented \
                                     thinking levels do not include it)",
                    });
                }
                Ok(serde_json::json!({
                    "thinkingLevel": level,
                    "includeThoughts": true,
                }))
            }
        }
    }
}

/// The thinking levels a Gemini 3 model documents. A family not in the table
/// gets only the two every listed model has.
/// <https://cloud.google.com/vertex-ai/generative-ai/docs/thinking> and
/// <https://ai.google.dev/gemini-api/docs/thinking>
fn supported_levels(model: &str) -> &'static [&'static str] {
    let model = bare_model(model);
    // A prefix that another prefix extends comes first: `gemini-3.5-flash-lite`
    // before `gemini-3.5-flash`, `gemini-3.1-flash-lite-image` before
    // `gemini-3.1-flash-lite`.
    const TABLE: &[(&str, &[&str])] = &[
        ("gemini-3-pro-image", &["high"]),
        ("gemini-3-pro", &["low", "high"]),
        ("gemini-3-flash", &["minimal", "low", "medium", "high"]),
        ("gemini-3.1-pro", &["low", "medium", "high"]),
        ("gemini-3.1-flash-lite-image", &["minimal", "high"]),
        (
            "gemini-3.1-flash-lite",
            &["minimal", "low", "medium", "high"],
        ),
        ("gemini-3.1-flash-image", &["minimal", "high"]),
        (
            "gemini-3.5-flash-lite",
            &["minimal", "low", "medium", "high"],
        ),
        ("gemini-3.5-flash", &["minimal", "low", "medium", "high"]),
        ("gemini-3.6-flash", &["minimal", "low", "medium", "high"]),
        ("gemini-3.7-flash", &["low", "medium", "high"]),
        ("gemini-3.8-flash", &["low", "medium", "high"]),
    ];
    TABLE
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map_or(&["low", "high"], |(_, levels)| levels)
}

enum Auth {
    ApiKey,
    Bearer(Arc<dyn BearerSource>),
}

/// A Gemini `generateContent` client.
pub struct GeminiAdapter {
    name: &'static str,
    config: ProviderConfig,
    http: reqwest::Client,
    auth: Auth,
    thinking: Option<ThinkingStyle>,
    nonce: u64,
    counter: AtomicU64,
}

impl GeminiAdapter {
    /// The Gemini Developer API, authenticated with `config`'s API key.
    ///
    /// `config.base_url` is the root that `/models/{model}:generateContent` is
    /// appended to, for example `https://generativelanguage.googleapis.com/v1beta`.
    pub fn api_key(config: ProviderConfig) -> Result<Self, ProviderError> {
        if config.credential().is_none() {
            return Err(ProviderError::MissingCredential {
                adapter: GEMINI_ADAPTER,
            });
        }
        Self::build(GEMINI_ADAPTER, config, Auth::ApiKey)
    }

    /// Gemini on Vertex AI, authenticated with bearer tokens from `auth`.
    ///
    /// `config.base_url` is the publisher root, for example
    /// `https://us-central1-aiplatform.googleapis.com/v1/projects/P/locations/L/publishers/google`.
    pub fn vertex(
        config: ProviderConfig,
        auth: Arc<dyn BearerSource>,
    ) -> Result<Self, ProviderError> {
        Self::build(VERTEX_GEMINI_ADAPTER, config, Auth::Bearer(auth))
    }

    fn build(
        name: &'static str,
        config: ProviderConfig,
        auth: Auth,
    ) -> Result<Self, ProviderError> {
        // The cached share is inside the prompt count on this wire. Under the
        // other convention it would be counted twice.
        if config.protocol != Protocol::Inclusive {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: name,
                capability: "additive usage",
            });
        }
        // Only the connect phase has a client-level bound; see the Anthropic
        // adapter for why a whole-request timeout would cut a healthy stream.
        let http = reqwest::Client::builder()
            .connect_timeout(config.timeout)
            .build()
            .map_err(|e| ProviderError::Transport {
                adapter: name,
                reason: e.to_string(),
            })?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Ok(Self {
            name,
            config,
            http,
            auth,
            thinking: None,
            nonce,
            counter: AtomicU64::new(0),
        })
    }

    /// Override the thinking control chosen from the model name.
    pub fn with_thinking_style(mut self, style: ThinkingStyle) -> Self {
        self.thinking = Some(style);
        self
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    fn decode(&self, reason: impl Into<String>) -> ProviderError {
        ProviderError::Decode {
            adapter: self.name,
            reason: reason.into(),
        }
    }

    /// Refuse a request this adapter cannot serve as asked, before any connection.
    fn check_request(&self, request: &Request) -> Result<(), ProviderError> {
        self.config.check_model(self.name, &request.model)?;
        // The model is a URL path segment.
        let model = bare_model(&request.model);
        let safe = !model.is_empty()
            && model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !safe {
            return Err(ProviderError::ModelUnsupported {
                adapter: self.name,
                model: request.model.clone(),
                available: "model ids use letters, digits, hyphen, underscore and dot".to_owned(),
            });
        }
        // An alias that follows the newest release answers under a concrete id,
        // which the echo check would refuse after the call was billed, on
        // every call.
        if model.ends_with("-latest") {
            return Err(ProviderError::ModelUnsupported {
                adapter: self.name,
                model: request.model.clone(),
                available: "a `-latest` alias answers under a concrete model id, which would \
                            fail the model echo check after the call was billed; configure the \
                            concrete id the alias currently resolves to (the `modelVersion` of \
                            a response names it)"
                    .to_owned(),
            });
        }
        let limit = if self.is_vertex() {
            MAX_FUNCTION_NAME_VERTEX
        } else {
            MAX_FUNCTION_NAME_GEMINI
        };
        // Names in the history are checked too: the service validates them the
        // same way, and a name another route accepted may not pass here.
        let history_names = request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|block| match block {
                Content::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            });
        for name in request
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .chain(history_names)
        {
            if let Some(why) = function_name_problem(name, limit) {
                return Err(self.refused(format!("the function name `{name}` {why}")));
            }
        }
        Ok(())
    }

    /// A request refused before any connection: nothing was sent or billed.
    /// The reason names what to change.
    fn refused(&self, reason: String) -> ProviderError {
        ProviderError::Unsent {
            adapter: self.name,
            reason,
        }
    }

    /// The tag a thought signature carries: the adapter and the model asked
    /// for. A token is valid only for the model that made it.
    fn producer(&self, model: &str) -> String {
        format!("{}:{}", self.name, bare_model(model))
    }

    fn is_vertex(&self) -> bool {
        self.name == VERTEX_GEMINI_ADAPTER
    }

    fn url(&self, model: &str, stream: bool) -> String {
        let model = bare_model(model);
        if stream {
            format!(
                "{}/models/{model}:streamGenerateContent?alt=sse",
                self.config.base_url
            )
        } else {
            format!("{}/models/{model}:generateContent", self.config.base_url)
        }
    }

    fn next_id(&self) -> String {
        format!(
            "{SYNTHETIC_ID_PREFIX}{:x}-{}",
            self.nonce,
            self.counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Translate a kGraf request into the wire shape.
    pub fn to_wire(&self, request: &Request) -> Result<serde_json::Value, ProviderError> {
        // A function response carries the function's name; the history carries
        // the name on the call the id belongs to.
        let mut names: HashMap<&str, &str> = HashMap::new();
        for message in &request.messages {
            for block in &message.content {
                if let Content::ToolUse { id, name, .. } = block {
                    names.insert(id.as_str(), name.as_str());
                }
            }
        }

        let producer = self.producer(&request.model);
        let mut contents = Vec::new();
        for message in &request.messages {
            let mut parts = Vec::new();
            let mut call_seen = false;
            for block in &message.content {
                match block {
                    Content::Text {
                        text, signature, ..
                    } => {
                        // A token is sent only back to the model that made it.
                        let token = signature
                            .as_ref()
                            .filter(|s| s.is_from(&producer))
                            .map(|s| s.value.as_str());
                        // The service refuses an empty text part, except one that
                        // carries a signature.
                        if !text.is_empty() || token.is_some() {
                            let mut part = serde_json::json!({ "text": text });
                            if let Some(token) = token {
                                part["thoughtSignature"] = serde_json::json!(token);
                            }
                            parts.push(part);
                        }
                    }
                    Content::ToolUse {
                        id,
                        name,
                        input,
                        signature,
                    } => {
                        let mut call = serde_json::json!({
                            "name": name,
                            "args": kgraf_provider::tool_arguments_object(input),
                        });
                        // Vertex documents no `id` on a function call.
                        if !self.is_vertex() && !id.starts_with(SYNTHETIC_ID_PREFIX) {
                            call["id"] = serde_json::json!(id);
                        }
                        let mut part = serde_json::json!({ "functionCall": call });
                        match signature.as_ref().filter(|s| s.is_from(&producer)) {
                            Some(signature) => {
                                part["thoughtSignature"] = serde_json::json!(signature.value);
                            }
                            // The service checks the first call of a step. A call
                            // with no signature, or one made by another route or
                            // model (whose token this model would reject), gets the
                            // skip value.
                            None if !call_seen && validates_signatures(&request.model) => {
                                part["thoughtSignature"] = serde_json::json!(SKIP_SIGNATURE);
                            }
                            None => {}
                        }
                        call_seen = true;
                        parts.push(part);
                    }
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let Some(name) = names.get(tool_use_id.as_str()) else {
                            // Refused before sending: the function name is unknown, and
                            // no request has been made, so nothing is billed.
                            return Err(ProviderError::CapabilityNotHonored {
                                adapter: self.name,
                                capability: "a tool result with no matching tool call",
                            });
                        };
                        let mut response = serde_json::json!({
                            "name": name,
                            // `output` and `error` are the keys the service documents.
                            "response": if *is_error {
                                serde_json::json!({ "error": content })
                            } else {
                                serde_json::json!({ "output": content })
                            },
                        });
                        if !self.is_vertex() && !tool_use_id.starts_with(SYNTHETIC_ID_PREFIX) {
                            response["id"] = serde_json::json!(tool_use_id);
                        }
                        parts.push(serde_json::json!({ "functionResponse": response }));
                    }
                    // Thought summaries are not resent.
                    Content::Reasoning { .. } => {}
                }
            }
            if parts.is_empty() {
                continue;
            }
            contents.push(serde_json::json!({
                "role": match message.role { Role::User => "user", Role::Assistant => "model" },
                "parts": parts,
            }));
        }

        let mut generation = serde_json::json!({
            "maxOutputTokens": request.max_output_tokens,
        });
        if let Some(temperature) = request.temperature {
            generation["temperature"] =
                serde_json::json!((f64::from(temperature) * 1e6).round() / 1e6);
        }
        if let Some(effort) = request.reasoning_effort {
            let style = self
                .thinking
                .or_else(|| ThinkingStyle::for_model(&request.model))
                .ok_or(ProviderError::CapabilityNotHonored {
                    adapter: self.name,
                    capability: "reasoning effort on a model family with no known thinking control",
                })?;
            generation["thinkingConfig"] = style.config(
                self.name,
                &request.model,
                effort,
                request.max_output_tokens,
                self.thinking.is_some(),
            )?;
        }

        let mut body = serde_json::json!({
            "contents": contents,
            "generationConfig": generation,
        });
        if !request.system.is_empty() {
            let mut parts = Vec::new();
            for block in &request.system {
                match block {
                    Content::Text { text, .. } if !text.is_empty() => {
                        parts.push(serde_json::json!({ "text": text }));
                    }
                    Content::Text { .. } => {}
                    _ => {
                        return Err(ProviderError::CapabilityNotHonored {
                            adapter: self.name,
                            capability: "non-text system content",
                        });
                    }
                }
            }
            if !parts.is_empty() {
                body["systemInstruction"] = serde_json::json!({ "parts": parts });
            }
        }
        if !request.tools.is_empty() {
            let mut declarations = Vec::new();
            for tool in &request.tools {
                let mut declaration = serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                });
                if self.is_vertex() {
                    // Vertex documents only `parameters`, an OpenAPI subset. A tool
                    // with no parameters sends none.
                    if !schema_is_empty_object(&tool.input_schema) {
                        declaration["parameters"] =
                            vertex_schema(&tool.input_schema).map_err(|why| {
                                self.refused(format!(
                                    "the schema of tool `{}` cannot be sent to Vertex: {why}",
                                    tool.name
                                ))
                            })?;
                    }
                } else {
                    // Full JSON Schema, not the OpenAPI subset of `parameters`.
                    declaration["parametersJsonSchema"] = tool.input_schema.clone();
                }
                declarations.push(declaration);
            }
            body["tools"] = serde_json::json!([{ "functionDeclarations": declarations }]);
        }
        Ok(body)
    }

    async fn post(
        &self,
        url: &str,
        body: &serde_json::Value,
        bound: std::time::Duration,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut builder = self
            .http
            .post(url)
            .header("content-type", "application/json");
        builder = match &self.auth {
            Auth::ApiKey => match self.config.credential() {
                Some(key) => builder.header("x-goog-api-key", key),
                None => {
                    return Err(ProviderError::MissingCredential { adapter: self.name });
                }
            },
            Auth::Bearer(source) => {
                let builder = builder.bearer_auth(source.bearer().await?);
                match source.quota_project() {
                    Some(project) => builder.header("x-goog-user-project", project),
                    None => builder,
                }
            }
        };
        let response = match tokio::time::timeout(bound, builder.json(body).send()).await {
            Ok(result) => result.map_err(|e| self.send_error(e))?,
            Err(_) => {
                return Err(ProviderError::Transport {
                    adapter: self.name,
                    reason: format!("no response within {:?} of sending the request", bound),
                });
            }
        };
        let status = response.status();
        if status.as_u16() == 429 {
            kgraf_provider::limits::record_429_now(&self.config.base_url);
        }
        if !status.is_success() {
            let text = match tokio::time::timeout(self.config.timeout, response.text()).await {
                Ok(Ok(text)) => text,
                Ok(Err(_)) | Err(_) => String::new(),
            };
            return Err(ProviderError::Http {
                adapter: self.name,
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(response)
    }

    fn send_error(&self, e: reqwest::Error) -> ProviderError {
        // Only a connection that never opened is known not to have reached the
        // provider.
        if e.is_connect() || e.is_builder() {
            ProviderError::Unsent {
                adapter: self.name,
                reason: e.to_string(),
            }
        } else {
            ProviderError::Transport {
                adapter: self.name,
                reason: e.to_string(),
            }
        }
    }

    /// Normalize a `usageMetadata` object.
    ///
    /// Public so the tests can assert the arithmetic directly.
    pub fn normalize_usage(&self, raw: &serde_json::Value) -> Result<TokenUsage, ProviderError> {
        let field = |name: &str| raw.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        // A successful call always reports a prompt count. The other counts are
        // omitted when zero (a reply made only of thoughts has no candidate
        // count), so only the prompt count proves usage arrived.
        if raw
            .get("promptTokenCount")
            .and_then(|v| v.as_u64())
            .is_none()
        {
            return Err(ProviderError::UsageMissing { adapter: self.name });
        }
        let input = field("promptTokenCount") + field("toolUsePromptTokenCount");
        let cached = field("cachedContentTokenCount");
        let candidates = field("candidatesTokenCount");
        let mut thoughts = field("thoughtsTokenCount");
        if let Some(total) = raw.get("totalTokenCount").and_then(|v| v.as_u64()) {
            let parts = input + candidates + thoughts;
            if total < parts {
                return Err(self.decode(format!(
                    "usage is inconsistent: prompt {input} plus candidates {candidates} plus \
                     thoughts {thoughts} is {parts} but `totalTokenCount` is {total}, below \
                     the counted fields"
                )));
            }
            // Billed tokens the parts leave out are reasoning, at the output rate.
            thoughts += total - parts;
        }
        TokenUsage::normalize(
            Protocol::Inclusive,
            input,
            cached,
            0,
            candidates,
            thoughts,
            Some(raw.clone()),
        )
        .map_err(|source| ProviderError::Usage {
            adapter: self.name,
            source,
        })
    }

    /// The counted fields as reported, with no consistency check and the raw
    /// object kept: the floor a refused call keeps on its open row.
    fn lower_bound(&self, raw: &serde_json::Value) -> Option<TokenUsage> {
        let field = |name: &str| raw.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        let prompt = raw.get("promptTokenCount").and_then(|v| v.as_u64())?;
        let input = prompt + field("toolUsePromptTokenCount");
        let cached = field("cachedContentTokenCount");
        Some(TokenUsage {
            uncached_input: input.saturating_sub(cached),
            cache_read: cached,
            cache_write: 0,
            text_output: field("candidatesTokenCount"),
            reasoning_output: field("thoughtsTokenCount"),
            unsplit_reasoning_chars: None,
            raw: Some(raw.clone()),
        })
    }

    /// Assert the response model is the requested one.
    ///
    /// Vertex resolves an alias to a versioned id (`gemini-2.0-flash` answers as
    /// `gemini-2.0-flash-001`), so the requested id followed by a numeric version
    /// suffix is the same model. Any other difference is a different model.
    fn assert_echo(&self, requested: &str, returned: &str) -> Result<(), ProviderError> {
        let requested = bare_model(requested);
        // A returned id may carry the resource path.
        let returned = returned.rsplit('/').next().unwrap_or(returned);
        if is_versioned_form(requested, returned) {
            return Ok(());
        }
        kgraf_provider::provider::assert_model_echo(requested, returned)
    }

    /// Fold one response part into the accumulated content.
    ///
    /// `producer` tags any thought signature the part carries.
    ///
    /// Returns the fragment to hand a stream consumer, if the part had one.
    fn absorb_part(
        &self,
        acc: &mut Accumulator,
        part: &serde_json::Value,
        producer: &str,
    ) -> Result<Option<Content>, ProviderError> {
        let thought = part
            .get("thought")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let signature = part
            .get("thoughtSignature")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| Signature::new(s, producer));
        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            // A thought summary is not kept with its signature: the history has
            // no place for a signed summary, and the service does not check that
            // one is returned (only a function call's token is validated).
            if thought {
                if text.is_empty() {
                    return Ok(None);
                }
                let fragment = Content::Reasoning {
                    text: text.to_owned(),
                };
                match acc.content.last_mut() {
                    Some(Content::Reasoning { text: last }) => last.push_str(text),
                    _ => acc.content.push(fragment.clone()),
                }
                return Ok(Some(fragment));
            }
            // A signature that trails the text belongs to the last text block,
            // whether it rides on an empty part or on the final chunk of a stream.
            if text.is_empty() {
                if let Some(signature) = signature {
                    attach_trailing_signature(acc, signature);
                }
                return Ok(None);
            }
            let fragment = Content::text(text);
            // A signed part is its own block: merging it with an unsigned
            // neighbor would move the token onto text it does not cover.
            match acc.content.last_mut() {
                Some(Content::Text {
                    text: last,
                    signature: None,
                    ..
                }) if signature.is_none() => last.push_str(text),
                _ => acc.content.push(Content::Text {
                    text: text.to_owned(),
                    cache_breakpoint: false,
                    signature,
                }),
            }
            return Ok(Some(fragment));
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.is_empty())
                .ok_or_else(|| self.decode("a functionCall part has no name"))?;
            let id = match call.get("id").and_then(|i| i.as_str()) {
                Some(id) if !id.is_empty() => id.to_owned(),
                _ => self.next_id(),
            };
            let args = call
                .get("args")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            acc.tool_calls += 1;
            let block = Content::ToolUse {
                id,
                name: name.to_owned(),
                input: args,
                signature,
            };
            acc.content.push(block.clone());
            return Ok(Some(block));
        }
        // A part with only a signature carries no content of its own; a token
        // that is not a thought summary's trails the text like an empty text part.
        if part
            .as_object()
            .is_some_and(|o| o.keys().all(|k| k == "thoughtSignature" || k == "thought"))
        {
            if let (false, Some(signature)) = (thought, signature) {
                attach_trailing_signature(acc, signature);
            }
            return Ok(None);
        }
        // Inline data, code execution and similar parts are not requested, so one
        // arriving is a protocol surprise; dropping it would lose output silently.
        let kinds = part
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        Err(self.decode(format!("unmodeled response part with keys: {kinds}")))
    }

    fn parse_response(
        &self,
        requested: &str,
        body: &serde_json::Value,
    ) -> Result<Parsed, ProviderError> {
        if let Some(error) = body.get("error") {
            return Err(self.decode(format!("the response is an error object: {error}")));
        }
        let returned = body
            .get("modelVersion")
            .and_then(|m| m.as_str())
            .ok_or_else(|| {
                self.decode("response has no `modelVersion`, so the model echo cannot be checked")
            })?;
        self.assert_echo(requested, returned)?;
        let usage = body
            .get("usageMetadata")
            .ok_or(ProviderError::UsageMissing { adapter: self.name })?;
        let usage = self.normalize_usage(usage)?;
        let request_id = body
            .get("responseId")
            .and_then(|i| i.as_str())
            .map(str::to_owned);

        let producer = self.producer(requested);
        let mut acc = Accumulator::default();
        let candidate = body
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first());
        let mut raw_finish: Option<String> = None;
        let finish = match candidate {
            Some(candidate) => {
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        self.absorb_part(&mut acc, part, &producer)?;
                    }
                }
                let reason = candidate
                    .get("finishReason")
                    .and_then(|r| r.as_str())
                    .ok_or_else(|| self.decode("the candidate has no `finishReason`"))?;
                raw_finish = Some(reason.to_owned());
                classify_finish(reason, acc.tool_calls)
            }
            None => match body
                .get("promptFeedback")
                .and_then(|f| f.get("blockReason"))
                .and_then(|r| r.as_str())
            {
                Some(reason) => Finish::Unusable(format!("prompt blocked: {reason}")),
                None => return Err(self.decode("the response has no candidates")),
            },
        };
        let stop_reason = match &finish {
            Finish::Stop(reason) => *reason,
            _ => StopReason::Other,
        };
        Ok(Parsed {
            response: Response {
                finish_reason: raw_finish,
                model: returned.to_owned(),
                content: acc.content,
                stop_reason,
                usage,
                request_id,
            },
            finish,
        })
    }
}

/// Give a signature that trails the text to the last text block, unless that
/// block already has one, in which case the token is a block of its own.
fn attach_trailing_signature(acc: &mut Accumulator, signature: Signature) {
    match acc.content.last_mut() {
        Some(Content::Text {
            signature: last @ None,
            ..
        }) => *last = Some(signature),
        _ => acc.content.push(Content::Text {
            text: String::new(),
            cache_breakpoint: false,
            signature: Some(signature),
        }),
    }
}

/// Whether the service validates thought signatures on this model: the Gemini 3
/// family does, and answers a history without them with a 400.
fn validates_signatures(model: &str) -> bool {
    bare_model(model).starts_with("gemini-3")
}

/// Whether `returned` is `requested` plus a numeric version suffix such as
/// `-001` or `-05-20`, and nothing else.
/// <https://cloud.google.com/vertex-ai/generative-ai/docs/learn/model-versions>
fn is_versioned_form(requested: &str, returned: &str) -> bool {
    returned
        .strip_prefix(requested)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .split('-')
                    .all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// A schema that says "an object" and nothing more: no parameters.
fn schema_is_empty_object(schema: &serde_json::Value) -> bool {
    let Some(map) = schema.as_object() else {
        return false;
    };
    map.get("type").and_then(|t| t.as_str()) == Some("object")
        && map
            .get("properties")
            .is_none_or(|p| p.as_object().is_some_and(|p| p.is_empty()))
        && map
            .get("required")
            .is_none_or(|r| r.as_array().is_some_and(|r| r.is_empty()))
}

/// Convert a JSON Schema into the OpenAPI subset Vertex documents for
/// `FunctionDeclaration.parameters`.
///
/// The fields are those of the `Schema` resource
/// (<https://cloud.google.com/vertex-ai/docs/reference/rest/v1/Schema>):
/// `type`, `description`, `enum`, `items`, `properties`, `required`,
/// `nullable`, `anyOf`, `format`, `title`, `default`, `example`, the numeric,
/// length, pattern and item-count bounds. Types are the upper-case enum the
/// reference names. Three rules keep a schema the service would answer with a
/// 400 from being sent:
///
/// - a node with no `type` gets one only where it is implied (`enum` of
///   strings, `properties`, `items`, `anyOf`); otherwise it is refused;
/// - `enum` is a list of strings, so any other value is refused;
/// - an `OBJECT` with no properties is refused (the service rejects it), which
///   is what a free-form object is.
///
/// A keyword the subset cannot express is an error naming it and where it is,
/// never a silent drop: `additionalProperties: true|false` and pure annotations
/// are the only omissions, because the model is never shown them as constraints.
/// `uniqueItems` has no field in the subset either. `uniqueItems: true` is kept
/// as a sentence in the node's `description` ("The items must be distinct."), so
/// the model is still shown the rule; `uniqueItems: false` states no rule and is
/// left out. A schema is never a guarantee the model obeys, so a tool that needs
/// distinct items checks them itself when it is called.
pub fn vertex_schema(schema: &serde_json::Value) -> Result<serde_json::Value, String> {
    convert_schema(schema, "$")
}

fn is_null_schema(schema: &serde_json::Value) -> bool {
    schema
        .as_object()
        .is_some_and(|m| m.len() == 1 && m.get("type").and_then(|t| t.as_str()) == Some("null"))
}

fn convert_schema(schema: &serde_json::Value, path: &str) -> Result<serde_json::Value, String> {
    let Some(map) = schema.as_object() else {
        return Err(format!("{path}: a schema must be an object"));
    };
    let mut out = serde_json::Map::new();
    let mut distinct_items = false;
    for (key, value) in map {
        match key.as_str() {
            "uniqueItems" => {
                distinct_items = value
                    .as_bool()
                    .ok_or_else(|| format!("{path}: `uniqueItems` is not a boolean"))?;
            }
            "type" => match value {
                serde_json::Value::String(t) => {
                    out.insert("type".into(), t.to_ascii_uppercase().into());
                }
                serde_json::Value::Array(types) => {
                    let mut named = Vec::new();
                    for t in types {
                        let t = t
                            .as_str()
                            .ok_or_else(|| format!("{path}: `type` holds a non-string"))?;
                        if t == "null" {
                            out.insert("nullable".into(), true.into());
                        } else {
                            named.push(t.to_ascii_uppercase());
                        }
                    }
                    match named.as_slice() {
                        [one] => {
                            out.insert("type".into(), one.clone().into());
                        }
                        _ => {
                            return Err(format!(
                                "{path}: `type` lists several non-null types, which the subset \
                                 cannot express"
                            ));
                        }
                    }
                }
                _ => return Err(format!("{path}: `type` is neither a string nor a list")),
            },
            "description" | "required" | "nullable" | "format" | "title" | "default"
            | "example" | "minItems" | "maxItems" | "minimum" | "maximum" | "minLength"
            | "maxLength" | "pattern" => {
                out.insert(key.clone(), value.clone());
            }
            "enum" => {
                out.insert("enum".into(), string_enum(value, path)?);
            }
            "const" => {
                out.insert(
                    "enum".into(),
                    string_enum(&serde_json::json!([value]), path)?,
                );
            }
            "items" => {
                out.insert(
                    "items".into(),
                    convert_schema(value, &format!("{path}.items"))?,
                );
            }
            "properties" => {
                let props = value
                    .as_object()
                    .ok_or_else(|| format!("{path}: `properties` must be an object"))?;
                let mut converted = serde_json::Map::new();
                for (name, prop) in props {
                    converted.insert(
                        name.clone(),
                        convert_schema(prop, &format!("{path}.properties.{name}"))?,
                    );
                }
                out.insert("properties".into(), converted.into());
            }
            "anyOf" => {
                let arms = value
                    .as_array()
                    .ok_or_else(|| format!("{path}: `anyOf` must be a list"))?;
                // `anyOf: [X, {"type": "null"}]` is how JSON Schema says "X or
                // null": the subset has `nullable` for that.
                let nulls = arms.iter().filter(|a| is_null_schema(a)).count();
                let rest: Vec<(usize, &serde_json::Value)> = arms
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| !is_null_schema(a))
                    .collect();
                if nulls > 0 {
                    out.insert("nullable".into(), true.into());
                }
                match rest.as_slice() {
                    [(i, only)] if nulls > 0 => {
                        let inner = convert_schema(only, &format!("{path}.anyOf[{i}]"))?;
                        if let serde_json::Value::Object(inner) = inner {
                            for (k, v) in inner {
                                out.entry(k).or_insert(v);
                            }
                        }
                    }
                    _ => {
                        let mut converted = Vec::new();
                        for (i, arm) in rest {
                            converted.push(convert_schema(arm, &format!("{path}.anyOf[{i}]"))?);
                        }
                        out.insert("anyOf".into(), converted.into());
                    }
                }
            }
            "additionalProperties" if value.is_boolean() => {}
            "$schema" | "$id" | "$comment" | "examples" => {}
            other => {
                return Err(format!(
                    "{path}: the keyword `{other}` cannot be expressed in the Vertex schema subset"
                ));
            }
        }
    }
    if distinct_items {
        const RULE: &str = "The items must be distinct.";
        let description = match out.get("description").and_then(|d| d.as_str()) {
            Some(text) if !text.trim().is_empty() => {
                format!("{}. {RULE}", text.trim().trim_end_matches('.'))
            }
            _ => RULE.to_owned(),
        };
        out.insert("description".into(), description.into());
    }
    // The service rejects a node with no type. Where the type is implied, say
    // it; anywhere else the schema is refused, naming the node.
    if !out.contains_key("type") && !out.contains_key("anyOf") {
        let implied = if out.contains_key("enum") {
            "STRING"
        } else if out.contains_key("properties") {
            "OBJECT"
        } else if out.contains_key("items") {
            "ARRAY"
        } else {
            return Err(format!(
                "{path}: the node has no `type`, which the service requires, and none is implied"
            ));
        };
        out.insert("type".into(), implied.into());
    }
    let is_object = out.get("type").and_then(|t| t.as_str()) == Some("OBJECT");
    let has_properties = out
        .get("properties")
        .and_then(|p| p.as_object())
        .is_some_and(|p| !p.is_empty());
    if is_object && !has_properties && path != "$" {
        return Err(format!(
            "{path}: an object with no properties (a free-form object) cannot be expressed; \
             the service rejects an OBJECT without properties"
        ));
    }
    Ok(out.into())
}

/// An `enum` value list as the subset takes it: a non-empty list of strings.
fn string_enum(value: &serde_json::Value, path: &str) -> Result<serde_json::Value, String> {
    let values = value
        .as_array()
        .ok_or_else(|| format!("{path}: `enum` must be a list"))?;
    if values.is_empty() {
        return Err(format!("{path}: `enum` lists no values"));
    }
    if let Some(bad) = values.iter().find(|v| !v.is_string()) {
        return Err(format!(
            "{path}: `enum` values must be strings on Vertex, and {bad} is not"
        ));
    }
    Ok(value.clone())
}

/// Why `name` is not an acceptable function name, if it is not.
///
/// Both endpoints document: start with a letter or an underscore; then letters,
/// digits, underscores, dots, colons and dashes; at most `limit` characters.
/// <https://cloud.google.com/vertex-ai/docs/reference/rest/v1/Tool#FunctionDeclaration>
fn function_name_problem(name: &str, limit: usize) -> Option<&'static str> {
    let mut chars = name.chars();
    match chars.next() {
        None => return Some("is empty"),
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        Some(_) => return Some("must start with a letter or an underscore"),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')) {
        return Some("may hold only letters, digits, underscores, dots, colons and dashes");
    }
    if name.len() > limit {
        return Some("is longer than the endpoint accepts");
    }
    None
}

/// The model id without a `models/` prefix.
fn bare_model(model: &str) -> &str {
    model.strip_prefix("models/").unwrap_or(model)
}

/// Fold a `usageMetadata` report into the ones before it: a field in `new`
/// replaces the same field, and fields only the earlier report had stay.
///
/// The exception is `totalTokenCount`: it is the sum of the other fields at the
/// moment of the report, so a total from an earlier report is stale once a
/// newer report omits it, and would be checked against counts it no longer
/// matches.
fn merge_usage(previous: Option<serde_json::Value>, new: &serde_json::Value) -> serde_json::Value {
    match (previous, new) {
        (Some(serde_json::Value::Object(mut merged)), serde_json::Value::Object(new)) => {
            if !new.contains_key("totalTokenCount") {
                merged.remove("totalTokenCount");
            }
            for (key, value) in new {
                merged.insert(key.clone(), value.clone());
            }
            serde_json::Value::Object(merged)
        }
        _ => new.clone(),
    }
}

/// Content and call count built up from response parts.
#[derive(Default)]
struct Accumulator {
    content: Vec<Content>,
    tool_calls: usize,
}

/// A response and how its candidate ended.
struct Parsed {
    response: Response,
    finish: Finish,
}

/// How a candidate ended.
#[derive(Debug, PartialEq, Eq)]
enum Finish {
    Stop(StopReason),
    /// The turn is not an answer: a block, a refusal, or a malformed call.
    Unusable(String),
    /// A reason this build does not model.
    Unknown(String),
}

/// Map a `finishReason` to how the turn ended.
///
/// The names are the `FinishReason` enum of the API reference. A name not
/// listed is [`Finish::Unknown`], never mapped to a normal end.
fn classify_finish(reason: &str, tool_calls: usize) -> Finish {
    match reason {
        "STOP" if tool_calls > 0 => Finish::Stop(StopReason::ToolUse),
        "STOP" => Finish::Stop(StopReason::EndTurn),
        "MAX_TOKENS" => Finish::Stop(StopReason::MaxTokens),
        "SAFETY"
        | "RECITATION"
        | "LANGUAGE"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "TOO_MANY_TOOL_CALLS"
        | "MISSING_THOUGHT_SIGNATURE"
        | "MALFORMED_RESPONSE"
        | "ESCALATION"
        | "PUP_LIMITED_DISABLED"
        | "NO_IMAGE"
        | "IMAGE_OTHER"
        | "OTHER" => Finish::Unusable(reason.to_owned()),
        other => Finish::Unknown(other.to_owned()),
    }
}

impl GeminiAdapter {
    /// Return the error a finish outcome calls for, after the call is settled.
    fn finish_error(&self, finish: Finish) -> Result<StopReason, ProviderError> {
        match finish {
            Finish::Stop(reason) => Ok(reason),
            Finish::Unusable(stop_reason) => Err(ProviderError::TurnNotUsable {
                adapter: self.name,
                stop_reason,
            }),
            Finish::Unknown(reason) => Err(self.decode(format!(
                "unknown finishReason `{reason}`; the call was billed and recorded"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl kgraf_provider::Provider for GeminiAdapter {
    fn adapter(&self) -> &'static str {
        self.name
    }

    fn protocol(&self) -> Protocol {
        self.config.protocol
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let prepared = self
            .check_request(request)
            .and_then(|()| self.to_wire(request));
        let body = match prepared {
            Ok(body) => body,
            Err(error) => {
                // Refused before sending, so nothing billed.
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        // Buffered: no byte arrives until the answer exists, so the bound
        // scales with the output cap (`ProviderConfig::buffered_timeout`).
        let bound = self.config.buffered_timeout(request.max_output_tokens);
        let http = match self
            .post(&self.url(&request.model, false), &body, bound)
            .await
        {
            Ok(http) => http,
            Err(error) => {
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        let text = match tokio::time::timeout(bound, http.text()).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                token.leave_unreconciled_async(None).await?;
                return Err(ProviderError::Transport {
                    adapter: self.name,
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                token.leave_unreconciled_async(None).await?;
                return Err(ProviderError::Transport {
                    adapter: self.name,
                    reason: format!("response body did not finish within {:?}", bound),
                });
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                token.leave_unreconciled_async(None).await?;
                return Err(self.decode(e.to_string()));
            }
        };
        match self.parse_response(&request.model, &parsed) {
            Ok(Parsed { response, finish }) => {
                token
                    .settle_async(&response.usage, response.request_id.as_deref())
                    .await?;
                self.finish_error(finish)?;
                Ok(response)
            }
            Err(error) => {
                // A refused body still billed; keep the floor it carries.
                let carried = parsed
                    .get("usageMetadata")
                    .and_then(|u| self.normalize_usage(u).ok().or_else(|| self.lower_bound(u)));
                let request_id = parsed.get("responseId").and_then(|i| i.as_str());
                token
                    .leave_unreconciled_with_async(carried.as_ref(), request_id)
                    .await?;
                Err(error)
            }
        }
    }

    fn capabilities(&self) -> Capabilities<'_> {
        Capabilities::none()
            .with_streaming(self)
            .with_reasoning(self)
    }
}

impl Reasoning for GeminiAdapter {
    fn reports_reasoning_tokens(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl Streaming for GeminiAdapter {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        let prepared = self
            .check_request(request)
            .and_then(|()| self.to_wire(request));
        let body = match prepared {
            Ok(body) => body,
            Err(error) => {
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        let http = match self
            .post(&self.url(&request.model, true), &body, self.config.timeout)
            .await
        {
            Ok(http) => http,
            Err(error) => {
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };

        // Every exit from here settles the row with what is known; disarmed only
        // on the single success path.
        let guard = StreamGuard::new(token);
        let mut decoder = SseDecoder::new(self.name);
        let mut byte_stream = http.bytes_stream();
        let mut pending: VecDeque<serde_json::Value> = VecDeque::new();

        let producer = self.producer(&request.model);
        let mut acc = Accumulator::default();
        let mut usage_raw: Option<serde_json::Value> = None;
        let mut finish_reason: Option<String> = None;
        let mut prompt_block: Option<String> = None;
        let mut model: Option<String> = None;
        let mut request_id: Option<String> = None;
        let mut started = false;

        loop {
            let event = match pending.pop_front() {
                Some(event) => event,
                None => match tokio::time::timeout(self.config.timeout, byte_stream.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        pending.extend(decoder.push(&chunk)?);
                        continue;
                    }
                    Ok(Some(Err(e))) => {
                        return Err(ProviderError::Transport {
                            adapter: self.name,
                            reason: e.to_string(),
                        });
                    }
                    Ok(None) => {
                        pending.extend(decoder.finish()?);
                        match pending.pop_front() {
                            Some(event) => event,
                            None => break,
                        }
                    }
                    Err(_) => {
                        return Err(ProviderError::Transport {
                            adapter: self.name,
                            reason: format!(
                                "no data received for {:?}; the stream stalled",
                                self.config.timeout
                            ),
                        });
                    }
                },
            };

            if let Some(error) = event.get("error") {
                return Err(self.decode(format!("provider sent a stream error event: {error}")));
            }
            if let Some(returned) = event.get("modelVersion").and_then(|m| m.as_str()) {
                // As early as the protocol allows, before the output is consumed.
                self.assert_echo(&request.model, returned)?;
                model = Some(returned.to_owned());
            }
            if let Some(id) = event.get("responseId").and_then(|i| i.as_str()) {
                request_id = Some(id.to_owned());
                guard.record_request_id(Some(id));
            }
            if !started {
                started = true;
                sink(StreamEvent::Started {
                    model: model.clone().unwrap_or_else(|| request.model.clone()),
                    request_id: request_id.clone(),
                });
            }
            if let Some(raw) = event.get("usageMetadata") {
                // Cumulative: each report restates the totals so far, so a field
                // present later replaces the earlier one and nothing is summed. A
                // later report may omit fields an earlier one carried (the prompt
                // count, once it is known), so reports are merged, not replaced.
                let merged = merge_usage(usage_raw.take(), raw);
                if let Some(floor) = self.lower_bound(&merged) {
                    guard.record_usage(&floor);
                    sink(StreamEvent::Usage { usage: floor });
                }
                usage_raw = Some(merged);
            }
            if let Some(reason) = event
                .get("promptFeedback")
                .and_then(|f| f.get("blockReason"))
                .and_then(|r| r.as_str())
            {
                prompt_block = Some(reason.to_owned());
            }
            if let Some(candidate) = event
                .get("candidates")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
            {
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        if let Some(content) = self.absorb_part(&mut acc, part, &producer)? {
                            sink(StreamEvent::Delta { content });
                        }
                    }
                }
                if let Some(reason) = candidate.get("finishReason").and_then(|r| r.as_str()) {
                    finish_reason = Some(reason.to_owned());
                }
            }
        }

        let Some(raw) = usage_raw else {
            return Err(ProviderError::UsageMissing { adapter: self.name });
        };
        let raw_finish = finish_reason.clone();
        let finish = match (finish_reason, prompt_block) {
            (Some(reason), _) => classify_finish(&reason, acc.tool_calls),
            (None, Some(reason)) => Finish::Unusable(format!("prompt blocked: {reason}")),
            // The connection closed before the candidate ended. What was seen is
            // a lower bound, not the call's figure.
            (None, None) => return Err(ProviderError::StreamTruncated { adapter: self.name }),
        };
        let Some(model) = model else {
            return Err(self.decode(
                "the stream carried no `modelVersion`, so the model echo cannot be checked",
            ));
        };
        let usage = self.normalize_usage(&raw)?;

        guard.disarm();
        token.settle_async(&usage, request_id.as_deref()).await?;
        let stop_reason = self.finish_error(finish)?;
        sink(StreamEvent::Finished { stop_reason });
        Ok(Response {
            finish_reason: raw_finish,
            model,
            content: acc.content,
            stop_reason,
            usage,
            request_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_json_schema_becomes_the_openapi_subset() {
        let converted = vertex_schema(&json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "where"},
                "mode": {"type": ["string", "null"], "enum": ["a", "b"]},
                "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "kind": {"const": "fixed"},
            },
            "required": ["path"],
        }))
        .expect("convertible");
        assert_eq!(converted["type"], "OBJECT");
        assert!(converted.get("additionalProperties").is_none());
        assert_eq!(converted["properties"]["path"]["type"], "STRING");
        assert_eq!(converted["properties"]["mode"]["type"], "STRING");
        assert_eq!(converted["properties"]["mode"]["nullable"], true);
        assert_eq!(converted["properties"]["tags"]["items"]["type"], "STRING");
        assert_eq!(converted["properties"]["tags"]["minItems"], 1);
        assert_eq!(converted["properties"]["kind"]["enum"], json!(["fixed"]));
        assert_eq!(converted["required"], json!(["path"]));
    }

    /// Vertex rejects a schema node with no `type`; a bare `enum` of strings is
    /// a string enum, so the converter states the type.
    #[test]
    fn a_bare_string_enum_gets_the_string_type() {
        let converted = vertex_schema(&json!({
            "type": "object",
            "properties": {
                "evidence": {
                    "type": "object",
                    "properties": {
                        "kind": {"enum": ["not_consulted", "recompute", "quote", "none"]},
                        "item": {"type": "string"}
                    },
                    "required": ["kind"]
                }
            }
        }))
        .expect("convertible");
        let kind = &converted["properties"]["evidence"]["properties"]["kind"];
        assert_eq!(kind["type"], "STRING");
        assert_eq!(
            kind["enum"],
            json!(["not_consulted", "recompute", "quote", "none"])
        );
    }

    #[test]
    fn a_construct_the_subset_cannot_hold_names_the_keyword_and_where() {
        for (schema, needle) in [
            (json!({"$ref": "#/defs/x"}), "$ref"),
            (
                json!({"type": "object", "properties": {"a": {"allOf": []}}}),
                "allOf",
            ),
            (
                json!({"type": ["string", "integer"]}),
                "several non-null types",
            ),
            (
                json!({"additionalProperties": {"type": "string"}}),
                "additionalProperties",
            ),
        ] {
            let error = vertex_schema(&schema).expect_err("must refuse");
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn only_a_numeric_version_suffix_is_the_same_model() {
        assert!(is_versioned_form(
            "gemini-2.0-flash",
            "gemini-2.0-flash-001"
        ));
        assert!(is_versioned_form(
            "gemini-2.5-flash",
            "gemini-2.5-flash-05-20"
        ));
        assert!(!is_versioned_form("gemini-2.0-flash", "gemini-2.0-flash"));
        assert!(!is_versioned_form(
            "gemini-2.0-flash",
            "gemini-2.0-flash-lite"
        ));
        assert!(!is_versioned_form(
            "gemini-2.0-flash",
            "gemini-2.0-flash--1"
        ));
        assert!(!is_versioned_form("gemini-2.0", "gemini-2.0-flash-001"));
    }

    #[test]
    fn a_free_form_object_and_an_empty_enum_are_refused_naming_the_node() {
        let free = vertex_schema(&json!({
            "type": "object",
            "properties": {"payload": {"type": "object"}}
        }))
        .expect_err("free-form object");
        assert!(free.contains("$.properties.payload"), "{free}");
        let empty_props = vertex_schema(&json!({
            "type": "object",
            "properties": {"a": {"type": "array", "items": {"type": "object", "properties": {}}}}
        }))
        .expect_err("no properties");
        assert!(
            empty_props.contains("$.properties.a.items"),
            "{empty_props}"
        );
        let empty_enum = vertex_schema(&json!({
            "type": "object",
            "properties": {"route": {"type": "string", "enum": []}}
        }))
        .expect_err("empty enum");
        assert!(empty_enum.contains("$.properties.route"), "{empty_enum}");
    }

    #[test]
    fn unique_items_becomes_a_sentence_the_model_reads_and_is_never_sent_as_a_keyword() {
        let converted = vertex_schema(&json!({
            "type": "object",
            "properties": {
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "uniqueItems": true,
                    "description": "choice only"
                },
                "bare": {"type": "array", "items": {"type": "string"}, "uniqueItems": true},
                "loose": {"type": "array", "items": {"type": "string"}, "uniqueItems": false}
            }
        }))
        .expect("converts");
        let props = &converted["properties"];
        assert_eq!(
            props["options"]["description"],
            "choice only. The items must be distinct."
        );
        assert_eq!(props["bare"]["description"], "The items must be distinct.");
        assert!(props["loose"].get("description").is_none(), "{converted}");
        assert!(
            !converted.to_string().contains("uniqueItems"),
            "the subset has no such field: {converted}"
        );
        let error = vertex_schema(&json!({
            "type": "object",
            "properties": {"l": {"type": "array", "items": {"type": "string"}, "uniqueItems": 1}}
        }))
        .expect_err("not a boolean");
        assert!(error.contains("$.properties.l"), "{error}");
    }

    #[test]
    fn a_non_string_enum_or_const_is_refused_naming_the_node() {
        for schema in [
            json!({"type": "object", "properties": {"n": {"type": "integer", "enum": [1, 2]}}}),
            json!({"type": "object", "properties": {"n": {"const": 3}}}),
            json!({"type": "object", "properties": {"n": {"enum": ["a", null]}}}),
        ] {
            let error = vertex_schema(&schema).expect_err("must refuse");
            assert!(error.contains("$.properties.n"), "{error}");
            assert!(error.contains("strings"), "{error}");
        }
    }

    #[test]
    fn a_node_with_no_type_and_none_implied_is_refused() {
        let error = vertex_schema(&json!({
            "type": "object",
            "properties": {"any": {"description": "whatever"}}
        }))
        .expect_err("must refuse");
        assert!(error.contains("$.properties.any"), "{error}");
        // A type is implied by `properties`, `items` and `anyOf`.
        let ok = vertex_schema(&json!({
            "type": "object",
            "properties": {
                "o": {"properties": {"x": {"type": "string"}}},
                "l": {"items": {"type": "string"}},
                "u": {"anyOf": [{"type": "string"}, {"type": "integer"}]}
            }
        }))
        .expect("implied");
        assert_eq!(ok["properties"]["o"]["type"], "OBJECT");
        assert_eq!(ok["properties"]["l"]["type"], "ARRAY");
        assert!(ok["properties"]["u"].get("type").is_none());
    }

    #[test]
    fn an_optional_any_of_folds_into_nullable() {
        let converted = vertex_schema(&json!({
            "type": "object",
            "properties": {
                "a": {"description": "d", "anyOf": [{"type": "string"}, {"type": "null"}]},
                "b": {"anyOf": [{"type": "string"}, {"type": "integer"}, {"type": "null"}]}
            }
        }))
        .expect("convertible");
        let a = &converted["properties"]["a"];
        assert_eq!(a["type"], "STRING");
        assert_eq!(a["nullable"], true);
        assert_eq!(a["description"], "d");
        assert!(a.get("anyOf").is_none());
        let b = &converted["properties"]["b"];
        assert_eq!(b["nullable"], true);
        assert_eq!(b["anyOf"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn the_documented_constraint_keywords_are_kept() {
        let converted = vertex_schema(&json!({
            "type": "object",
            "properties": {
                "when": {"type": "string", "format": "date-time", "title": "T",
                         "default": "now", "example": "2026-01-01T00:00:00Z",
                         "pattern": "^2"},
                "xs": {"type": "array", "items": {"type": "integer"}, "minItems": 1, "maxItems": 3}
            }
        }))
        .expect("convertible");
        let when = &converted["properties"]["when"];
        for key in ["format", "title", "default", "example", "pattern"] {
            assert!(when.get(key).is_some(), "{key} was dropped");
        }
        assert_eq!(converted["properties"]["xs"]["minItems"], 1);
        assert_eq!(converted["properties"]["xs"]["maxItems"], 3);
    }

    #[test]
    fn a_usage_total_from_an_earlier_report_does_not_outlive_a_report_without_one() {
        let first =
            json!({"promptTokenCount": 10, "candidatesTokenCount": 1, "totalTokenCount": 11});
        let merged = merge_usage(Some(first.clone()), &json!({"candidatesTokenCount": 5}));
        assert!(merged.get("totalTokenCount").is_none(), "{merged}");
        let kept = merge_usage(
            Some(first),
            &json!({"candidatesTokenCount": 5, "totalTokenCount": 15}),
        );
        assert_eq!(kept["totalTokenCount"], 15);
    }

    #[test]
    fn the_level_table_covers_the_documented_models() {
        for (model, levels) in [
            (
                "gemini-3.1-flash-lite",
                &["minimal", "low", "medium", "high"][..],
            ),
            ("gemini-3.1-flash-lite-image", &["minimal", "high"][..]),
            ("gemini-3-pro-image-preview", &["high"][..]),
            ("gemini-3.1-flash-image", &["minimal", "high"][..]),
            (
                "gemini-3.5-flash-lite",
                &["minimal", "low", "medium", "high"][..],
            ),
            ("gemini-3-pro-preview", &["low", "high"][..]),
            ("gemini-3.1-pro-preview", &["low", "medium", "high"][..]),
            ("gemini-3.9-unlisted", &["low", "high"][..]),
        ] {
            assert_eq!(supported_levels(model), levels, "{model}");
        }
    }

    #[test]
    fn the_minimum_thinking_budget_follows_the_model() {
        assert_eq!(min_thinking_budget("gemini-2.5-flash-lite"), 512);
        assert_eq!(
            min_thinking_budget("models/gemini-2.5-flash-lite-preview"),
            512
        );
        assert_eq!(min_thinking_budget("gemini-2.5-flash"), 128);
        assert_eq!(min_thinking_budget("gemini-2.5-pro"), 128);
    }

    #[test]
    fn a_function_name_follows_the_documented_charset() {
        for ok in ["get_weather", "_x", "a.b", "srv:tool-1", "A9"] {
            assert_eq!(function_name_problem(ok, 64), None, "{ok}");
        }
        for bad in ["", "1abc", "-a", ".a", "a b", "a/b", "caf\u{e9}", "a$"] {
            assert!(function_name_problem(bad, 64).is_some(), "{bad:?}");
        }
        assert!(function_name_problem(&"n".repeat(65), 64).is_some());
        assert!(function_name_problem(&"n".repeat(64), 64).is_none());
    }

    #[test]
    fn a_later_usage_report_fills_and_replaces_without_dropping() {
        let first = json!({"promptTokenCount": 10, "candidatesTokenCount": 1});
        let merged = merge_usage(Some(first), &json!({"candidatesTokenCount": 2}));
        assert_eq!(
            merged,
            json!({"promptTokenCount": 10, "candidatesTokenCount": 2})
        );
    }
}
