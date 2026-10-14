//! The AWS Bedrock Converse adapter.
//!
//! Speaks the Converse and ConverseStream operations of the Bedrock runtime
//! (`POST /model/{modelId}/converse` and `/converse-stream`), signed with AWS
//! Signature Version 4. Like the other adapters it serves the protocol, not the
//! company: the model behind a model id may be any vendor's, and the adapter
//! neither knows nor cares which.
//!
//! # Dependencies
//!
//! `aws-sigv4` and `aws-smithy-eventstream` (Apache-2.0, AWS's smithy-rs) provide the
//! two parts that are easy to get subtly wrong, the canonical-request signature and
//! the binary event-stream framing with its CRC checks. `aws-config` (the full
//! credential chain) and `aws-sdk-bedrockruntime` are not used: they reach the
//! instance metadata service and STS/SSO endpoints the user did not configure, and
//! bring a far larger dependency tree than this adapter needs. Credentials come from
//! a Bedrock API key, the standard environment variables, or a static-key profile
//! ([`credentials`]).
//!
//! # Protocol specifics
//!
//! - **Usage is additive.** `inputTokens` excludes cache reads and writes (the
//!   prompt-caching page states it: "the `inputTokens` field represents only the
//!   non-cached input tokens"), so the true input is the sum of the three counts.
//! - **The response names no model.** Converse echoes neither the model id nor a
//!   substitute, so the model-echo assertion has nothing to compare. The model is
//!   checked against the configured set before the request goes out, and the
//!   response carries the requested id.
//! - **Reasoning tokens are not reported apart** from output, so `reasoning_output`
//!   stays zero and reasoning text is returned as reasoning content.
//! - **The stream ends with usage.** `metadata`, which carries `usage`, comes after
//!   `messageStop`; a stream that ends before it has no usage and is unreconciled.
//! - **Cache points** (`cachePoint` blocks) are emitted only where the configuration
//!   says the endpoint honors them. The service accepts them for some models only.
//!   The catalog route leaves the flag off and it is not user-settable; a caller that
//!   builds its own [`ProviderConfig`] can turn it on for a model that supports them.
//! - **Blank text is never sent.** The service rejects a blank text block, so blank
//!   text is skipped and an empty tool result is sent as a short placeholder.
//! - **`totalTokens` is a check, not a gate.** The counts come from their own fields.
//!   A total that matches neither documented sum is recorded on the usage's raw blob
//!   and the answer is kept, because the call was billed.
//! - **Authentication** is a Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`, sent as a
//!   bearer token) or SigV4 with access keys. When both are present the API key is
//!   used: that is kGraf's rule, chosen so one explicit variable decides. A `424` (`ModelErrorException`) may
//!   follow model work and the reference does not say it is unbilled, so its row
//!   stays unreconciled.
//! - **Reasoning effort** has no model-independent field in Converse (it rides in
//!   model-specific `additionalModelRequestFields`), so a request carrying one is
//!   refused rather than sent with the effort dropped.
//!
//! - **Service tier and cache TTL.** A `serviceTier` other than `default`, or cache
//!   writes with a TTL other than `5m`, bill at rates the price table does not hold.
//!   The row keeps the counts and the tier but is left unreconciled, so it shows as
//!   unpriced; the answer is returned.
//! - **Redirects** are not followed; a redirect is an error.
//!
//! Every credential (the bearer API key, the secret access key and the session
//! token) is resolved per call, registered with the process-wide scrubber, and never
//! written to an error or a log.

pub mod credentials;
pub mod eventstream;
mod sign;
mod wire;

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::SystemTime;

use futures_util::StreamExt;
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::{CacheControl, Capabilities, StreamEvent, Streaming};
use kgraf_provider::{
    CallToken, Content, ProviderConfig, ProviderError, Request, Response, StopReason, StreamGuard,
};
use serde_json::Value;

pub use credentials::{AwsAuth, AwsConfigError, credential_probe};
use eventstream::{EventStreamDecoder, Frame};
use wire::Stop;

/// The adapter's stable name.
pub const ADAPTER: &str = "bedrock-converse";

/// Where the region goes in a default endpoint.
pub const REGION_PLACEHOLDER: &str = "{region}";

/// The most bytes of tool input one block may accumulate from stream fragments.
const MAX_TOOL_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// A variable lookup, `std::env::var` in production.
type LookupFn = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where an adapter gets its signing credentials and region.
enum AuthSource {
    /// Read variables through this lookup (the process environment in production)
    /// and the shared config files on every call.
    Lookup(LookupFn),
    /// A fixed value, for tests and for callers that resolved their own.
    Fixed(AwsAuth),
}

/// The Bedrock Converse adapter.
pub struct BedrockAdapter {
    config: ProviderConfig,
    http: reqwest::Client,
    auth: AuthSource,
    /// The last secret access key registered with the scrubber, so a per-call
    /// resolution registers again only when the value changed.
    registered: Mutex<Option<String>>,
}

impl BedrockAdapter {
    /// An adapter that resolves credentials and region from the environment and the
    /// shared config files on every call.
    ///
    /// `config.base_url` may contain `{region}`, replaced by the resolved region.
    /// `config.api_key` is not used: the signing credentials are not a single string.
    /// The configured protocol must be additive, which is what Converse reports.
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Self::build(
            config,
            AuthSource::Lookup(Box::new(credentials::process_env)),
        )
    }

    /// An adapter that resolves credentials and region through `lookup` instead of
    /// the process environment.
    pub fn with_lookup(
        config: ProviderConfig,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self, ProviderError> {
        Self::build(config, AuthSource::Lookup(Box::new(lookup)))
    }

    /// An adapter signing with fixed credentials and region.
    pub fn with_auth(config: ProviderConfig, auth: AwsAuth) -> Result<Self, ProviderError> {
        Self::build(config, AuthSource::Fixed(auth))
    }

    fn build(config: ProviderConfig, auth: AuthSource) -> Result<Self, ProviderError> {
        if config.protocol != Protocol::Additive {
            // Converse reports input excluding the cache counts. Reading it as
            // inclusive would subtract the cache from the wrong total (fail loud).
            return Err(ProviderError::CapabilityNotHonored {
                adapter: ADAPTER,
                capability: "inclusive usage (Converse reports input excluding cache)",
            });
        }
        // Connect timeout only; the whole-call and per-chunk bounds are applied at
        // the call sites so a healthy long stream is not cut off.
        // Redirects are never followed: the request carries a signature or a bearer
        // token and a body, and a followed redirect could replay them at another host.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.timeout)
            .build()
            .map_err(|e| ProviderError::Transport {
                adapter: ADAPTER,
                reason: e.to_string(),
            })?;
        let adapter = Self {
            config,
            http,
            auth,
            registered: Mutex::new(None),
        };
        // Register the secrets now, so an error or transcript produced before the
        // first call cannot carry them. A lookup that cannot resolve yet is retried
        // on every call.
        let _ = adapter.resolve();
        Ok(adapter)
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// Resolve the credentials and region for one call, registering the secrets.
    fn resolve(&self) -> Result<AwsAuth, ProviderError> {
        let auth = match &self.auth {
            AuthSource::Fixed(auth) => auth.clone(),
            AuthSource::Lookup(lookup) => {
                credentials::resolve(lookup.as_ref()).map_err(|e| ProviderError::Unsent {
                    adapter: ADAPTER,
                    reason: e.to_string(),
                })?
            }
        };
        // The region becomes part of the host name, so a fixed value is checked too.
        credentials::validate_region(&auth.region).map_err(|e| ProviderError::Unsent {
            adapter: ADAPTER,
            reason: e.to_string(),
        })?;
        let mut last = self
            .registered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The session token can change while the secret key stays, so the key of the
        // "already registered" check covers both.
        let fingerprint = format!(
            "{}\0{}",
            auth.primary_secret(),
            auth.session_token.as_deref().unwrap_or("")
        );
        if last.as_deref() != Some(fingerprint.as_str()) {
            credentials::register_secrets(&auth);
            *last = Some(fingerprint);
        }
        Ok(auth)
    }

    /// Refuse a request this adapter cannot serve as asked, before any connection.
    fn check_request(&self, request: &Request) -> Result<(), ProviderError> {
        self.config.check_model(ADAPTER, &request.model)?;
        // Converse has a `toolChoice` form, but none is built or exchanged here.
        kgraf_provider::check_tool_choice(ADAPTER, request, false)?;
        if request.reasoning_effort.is_some() {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: ADAPTER,
                capability: "reasoning effort",
            });
        }
        // JSON has no NaN or infinity; serialization would turn one into `null`, and
        // the service would then run with its default temperature.
        if request.temperature.is_some_and(|t| !t.is_finite()) {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: ADAPTER,
                capability: "a non-finite temperature",
            });
        }
        Ok(())
    }

    /// The URL for one operation. The model id is percent-encoded whole: an id such
    /// as `anthropic.claude-x-v1:0`, or an ARN, contains `:` and `/`.
    fn url(&self, auth: &AwsAuth, model: &str, operation: &str) -> String {
        let base = self
            .config
            .base_url
            .replace(REGION_PLACEHOLDER, &auth.region);
        format!("{base}/model/{}/{operation}", encode_path_segment(model))
    }

    /// Send one request. On failure the error comes with the `x-amzn-requestid` of
    /// the response that carried it, when there was one, so the open row can be
    /// reconciled against the provider's records.
    async fn post(
        &self,
        model: &str,
        operation: &str,
        body: &Value,
        bound: std::time::Duration,
    ) -> Result<reqwest::Response, (ProviderError, Option<String>)> {
        let no_id = |error: ProviderError| (error, None);
        let auth = self.resolve().map_err(no_id)?;
        let url = self.url(&auth, model, operation);
        let bytes = serde_json::to_vec(body).map_err(|e| {
            no_id(ProviderError::Decode {
                adapter: ADAPTER,
                reason: format!("cannot serialize the request: {e}"),
            })
        })?;
        let signed = match &auth.bearer_token {
            // A Bedrock API key replaces the signature with a bearer header.
            Some(token) => vec![("authorization".to_owned(), format!("Bearer {token}"))],
            None => sign::sign_request(
                "POST",
                &url,
                &[("content-type", "application/json")],
                &bytes,
                &auth,
                sign::SERVICE,
                SystemTime::now(),
            )
            .map_err(no_id)?,
        };
        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json");
        for (name, value) in signed {
            builder = builder.header(name, value);
        }
        let response = match tokio::time::timeout(bound, builder.body(bytes).send()).await {
            Ok(result) => result.map_err(|e| no_id(send_error(e)))?,
            Err(_) => {
                return Err(no_id(ProviderError::Transport {
                    adapter: ADAPTER,
                    reason: format!("no response within {:?} of sending the request", bound),
                }));
            }
        };
        let status = response.status();
        if status.as_u16() == 429 {
            kgraf_provider::limits::record_429_now(&self.config.base_url);
        }
        if !status.is_success() {
            let request_id = request_id_of(&response);
            let text = read_error_body(response, self.config.timeout.min(ERROR_BODY_TIMEOUT)).await;
            return Err((
                ProviderError::Http {
                    adapter: ADAPTER,
                    status: status.as_u16(),
                    body: scrub_error_body(&text),
                    // AWS signals throttling in the body, not a retry header.
                    retry_after_ms: None,
                },
                request_id,
            ));
        }
        Ok(response)
    }
}

/// The most bytes of an error body that are read.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// The longest an error body may take to arrive. An error page is small; a service
/// that stalls on it must not hold the call for the full call timeout.
const ERROR_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Read an error body with a time limit and a size limit. A service that stalls or
/// sends an endless body must not hold the call's row open: whatever arrived in time
/// is used, with a marker saying the body is incomplete, and the status alone
/// carries the error if nothing did.
async fn read_error_body(mut response: reqwest::Response, limit: std::time::Duration) -> String {
    // The buffer lives outside the timed future, so a timeout keeps the bytes read
    // so far instead of dropping them with the future.
    let mut bytes: Vec<u8> = Vec::new();
    let outcome = tokio::time::timeout(limit, async {
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let room = MAX_ERROR_BODY_BYTES - bytes.len();
                    bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    if bytes.len() >= MAX_ERROR_BODY_BYTES {
                        return None;
                    }
                }
                Ok(None) => return None,
                Err(e) => return Some(format!("the error body read failed: {e}")),
            }
        }
    })
    .await;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    match outcome {
        Ok(None) => {}
        Ok(Some(problem)) => {
            text.push_str(&format!(" ({problem})"));
        }
        Err(_) => text.push_str(" (the error body did not finish in time)"),
    }
    text
}

/// Close a call's row after a failed send. A `424` (`ModelErrorException`) is the
/// service reporting an error while processing the model; the reference does not say
/// the request is unbilled, so the row stays open. Other errors follow the shared
/// billing rule.
async fn close_after_failed_post(
    token: &CallToken<'_>,
    error: &ProviderError,
    request_id: Option<&str>,
) -> Result<(), ProviderError> {
    if matches!(error, ProviderError::Http { status: 424, .. }) {
        token.leave_unreconciled_async(request_id).await?;
        return Ok(());
    }
    token.close_on_async(error, request_id).await?;
    Ok(())
}

/// The error text with every registered secret removed, and bounded: an error page
/// can be large and can echo request headers.
fn scrub_error_body(text: &str) -> String {
    let scrubbed = kgraf_secrets::scrub_persist(text);
    scrubbed.chars().take(4000).collect()
}

/// Percent-encode one path segment, keeping only the unreserved characters.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Classify a failure to send. Only a connection that never opened is known not to
/// have reached the service.
fn send_error(e: reqwest::Error) -> ProviderError {
    if e.is_connect() || e.is_builder() {
        ProviderError::Unsent {
            adapter: ADAPTER,
            reason: e.to_string(),
        }
    } else {
        ProviderError::Transport {
            adapter: ADAPTER,
            reason: e.to_string(),
        }
    }
}

fn request_id_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("x-amzn-requestid")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[async_trait::async_trait]
impl kgraf_provider::Provider for BedrockAdapter {
    fn adapter(&self) -> &'static str {
        ADAPTER
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        if let Err(error) = self.check_request(request) {
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let body = wire::to_wire(request, self.config.honors_cache_control);
        // Buffered: no byte arrives until the answer exists, so the bound
        // scales with the output cap (`ProviderConfig::buffered_timeout`).
        let bound = self.config.buffered_timeout(request.max_output_tokens);
        let http = match self.post(&request.model, "converse", &body, bound).await {
            Ok(http) => http,
            Err((error, failed_id)) => {
                close_after_failed_post(token, &error, failed_id.as_deref()).await?;
                return Err(error);
            }
        };
        let request_id = request_id_of(&http);
        // A whole-call bound is right here: one buffered response.
        let text = match tokio::time::timeout(bound, http.text()).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter: ADAPTER,
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter: ADAPTER,
                    reason: format!("response body did not finish within {:?}", bound),
                });
            }
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Decode {
                    adapter: ADAPTER,
                    reason: e.to_string(),
                });
            }
        };

        // Usage first, and the call settled on it: from here the call is billed
        // whatever the rest of the body turns out to be.
        let raw_usage = parsed.get("usage");
        let tier = parsed.get("serviceTier");
        let usage = match raw_usage.map(wire::normalize_usage) {
            Some(Ok(usage)) => usage,
            Some(Err(error)) => {
                let floor = raw_usage.and_then(wire::lower_bound_usage);
                token
                    .leave_unreconciled_with_async(floor.as_ref(), request_id.as_deref())
                    .await?;
                return Err(error);
            }
            None => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::UsageMissing { adapter: ADAPTER });
            }
        };
        let mut usage = usage;
        wire::record_tier(&mut usage, tier);
        // A tier or cache TTL the price table cannot see: the row keeps the counts
        // but is not settled, so it is never priced at the standard rate.
        let refusal = raw_usage.and_then(|raw| wire::pricing_refusal(raw, tier));
        match &refusal {
            Some(reason) => {
                tracing::warn!(adapter = ADAPTER, %reason, "usage left unpriced");
                token
                    .leave_unreconciled_with_async(Some(&usage), request_id.as_deref())
                    .await?;
            }
            None => token.settle_async(&usage, request_id.as_deref()).await?,
        }

        let message = parsed
            .get("output")
            .and_then(|o| o.get("message"))
            .ok_or_else(|| ProviderError::Decode {
                adapter: ADAPTER,
                reason: "the response has no `output.message`".to_owned(),
            })?;
        let content = wire::parse_content(message)?;
        let raw_finish = parsed
            .get("stopReason")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Decode {
                adapter: ADAPTER,
                reason: "the response has no `stopReason`".to_owned(),
            })?;
        let stop = wire::map_stop(raw_finish)?;
        match stop {
            Stop::Unusable(stop_reason) => Err(ProviderError::TurnNotUsable {
                adapter: ADAPTER,
                stop_reason,
            }),
            Stop::Usable(stop_reason) => Ok(Response {
                finish_reason: Some(raw_finish.to_owned()),
                model: request.model.clone(),
                content,
                stop_reason,
                usage,
                request_id,
            }),
        }
    }

    fn capabilities(&self) -> Capabilities<'_> {
        let mut capabilities = Capabilities::none().with_streaming(self);
        if self.config.honors_cache_control {
            capabilities = capabilities.with_cache_control(self);
        }
        capabilities
    }
}

impl CacheControl for BedrockAdapter {
    fn max_breakpoints(&self) -> usize {
        wire::MAX_CACHE_POINTS
    }

    fn minimum_cacheable_tokens(&self) -> Option<u32> {
        // The minimum differs by model and the published table changes; unknown is
        // a different claim from a number that goes stale.
        None
    }
}

/// A content block accumulating across delta events.
enum Block {
    Text(String),
    Reasoning(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
}

impl Block {
    fn finish(self) -> Result<Content, ProviderError> {
        Ok(match self {
            Block::Text(text) => Content::text(text),
            Block::Reasoning(text) => Content::Reasoning { text },
            Block::ToolUse { id, name, json } => {
                // A tool with no arguments streams no fragments at all.
                let input = if json.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&json).map_err(|e| ProviderError::Decode {
                        adapter: ADAPTER,
                        reason: format!(
                            "toolUse `{name}` (id `{id}`) accumulated invalid JSON input: {e}"
                        ),
                    })?
                };
                Content::ToolUse {
                    id,
                    name,
                    input,
                    signature: None,
                }
            }
        })
    }
}

fn decode(reason: impl Into<String>) -> ProviderError {
    ProviderError::Decode {
        adapter: ADAPTER,
        reason: reason.into(),
    }
}

fn block_index(event: &Value) -> Result<u64, ProviderError> {
    event
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .ok_or_else(|| decode("a content block event has no `contentBlockIndex`"))
}

#[async_trait::async_trait]
impl Streaming for BedrockAdapter {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        if let Err(error) = self.check_request(request) {
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let body = wire::to_wire(request, self.config.honors_cache_control);
        let http = match self
            .post(
                &request.model,
                "converse-stream",
                &body,
                self.config.timeout,
            )
            .await
        {
            Ok(http) => http,
            Err((error, failed_id)) => {
                close_after_failed_post(token, &error, failed_id.as_deref()).await?;
                return Err(error);
            }
        };
        let request_id = request_id_of(&http);

        // Settles the row with whatever is known on every exit but the one success
        // path, including the caller dropping this future.
        let guard = StreamGuard::new(token);
        guard.record_request_id(request_id.as_deref());

        let mut usage: Option<TokenUsage> = None;
        let mut unpriced: Option<String> = None;
        let mut stop: Option<String> = None;
        let mut blocks: HashMap<u64, Block> = HashMap::new();
        let mut content: BTreeMap<u64, Content> = BTreeMap::new();
        let mut broken_tool_call: Option<ProviderError> = None;

        let mut decoder = EventStreamDecoder::new();
        let mut byte_stream = http.bytes_stream();
        let mut pending: std::collections::VecDeque<Frame> = std::collections::VecDeque::new();
        // A decode failure is raised only after the frames decoded before it have
        // been handled, so a `metadata` event ahead of a corrupt frame is not lost.
        let mut deferred: Option<ProviderError> = None;
        loop {
            let frame = match pending.pop_front() {
                Some(frame) => frame,
                None => {
                    if let Some(error) = deferred.take() {
                        return Err(error);
                    }
                    // Only a gap this long between chunks counts as stalled.
                    match tokio::time::timeout(self.config.timeout, byte_stream.next()).await {
                        Ok(Some(Ok(chunk))) => {
                            let (frames, error) = decoder.push_partial(&chunk);
                            pending.extend(frames);
                            deferred = error;
                            continue;
                        }
                        Ok(Some(Err(e))) => {
                            return Err(ProviderError::Transport {
                                adapter: ADAPTER,
                                reason: e.to_string(),
                            });
                        }
                        Ok(None) => break,
                        Err(_) => {
                            return Err(ProviderError::Transport {
                                adapter: ADAPTER,
                                reason: format!(
                                    "no data received for {:?}; the stream stalled",
                                    self.config.timeout
                                ),
                            });
                        }
                    }
                }
            };
            let (name, payload) = match frame {
                Frame::Event { name, payload } => (name, payload),
                Frame::Exception { kind, payload } => {
                    if kind == "throttlingException" {
                        kgraf_provider::limits::record_429_now(&self.config.base_url);
                    }
                    let message = serde_json::from_slice::<Value>(&payload)
                        .ok()
                        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
                        .unwrap_or_default();
                    return Err(decode(format!(
                        "the service sent a stream exception ({kind}): {}",
                        scrub_error_body(&message)
                    )));
                }
                Frame::Error { code, message } => {
                    return Err(decode(format!(
                        "the service sent a stream error ({code}): {}",
                        scrub_error_body(&message)
                    )));
                }
            };
            let event: Value = serde_json::from_slice(&payload)
                .map_err(|e| decode(format!("the `{name}` event payload is not JSON: {e}")))?;
            match name.as_str() {
                "messageStart" => sink(StreamEvent::Started {
                    model: request.model.clone(),
                    request_id: request_id.clone(),
                }),
                "contentBlockStart" => {
                    let index = block_index(&event)?;
                    if blocks.contains_key(&index) || content.contains_key(&index) {
                        return Err(decode(format!(
                            "contentBlockStart received twice for content block {index}"
                        )));
                    }
                    let start = event.get("start").cloned().unwrap_or_default();
                    match start.get("toolUse") {
                        Some(tool) => {
                            let text = |field: &str| {
                                tool.get(field)
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .ok_or_else(|| {
                                        decode(format!("a toolUse start has no `{field}`"))
                                    })
                            };
                            blocks.insert(
                                index,
                                Block::ToolUse {
                                    id: text("toolUseId")?,
                                    name: text("name")?,
                                    json: String::new(),
                                },
                            );
                        }
                        None => {
                            return Err(decode(format!(
                                "contentBlockStart for block {index} is not a tool use, which \
                                 is the only start this adapter models"
                            )));
                        }
                    }
                }
                "contentBlockDelta" => {
                    let index = block_index(&event)?;
                    let delta = event.get("delta").cloned().unwrap_or_default();
                    if let Some(fragment) = delta.get("text").and_then(Value::as_str) {
                        match blocks
                            .entry(index)
                            .or_insert_with(|| Block::Text(String::new()))
                        {
                            Block::Text(buffer) => buffer.push_str(fragment),
                            _ => {
                                return Err(decode(format!(
                                    "a text delta for content block {index}, which is not a \
                                     text block"
                                )));
                            }
                        }
                        sink(StreamEvent::Delta {
                            content: Content::text(fragment),
                        });
                    } else if let Some(tool) = delta.get("toolUse") {
                        let fragment = tool.get("input").and_then(Value::as_str).unwrap_or("");
                        match blocks.get_mut(&index) {
                            Some(Block::ToolUse { id, name, json }) => {
                                if json.len() + fragment.len() > MAX_TOOL_INPUT_BYTES {
                                    return Err(decode(format!(
                                        "toolUse `{name}` (id `{id}`) accumulated more than \
                                         {MAX_TOOL_INPUT_BYTES} bytes of input without closing"
                                    )));
                                }
                                json.push_str(fragment);
                            }
                            _ => {
                                return Err(decode(format!(
                                    "a toolUse delta for content block {index}, which is not \
                                     an open toolUse block"
                                )));
                            }
                        }
                    } else if let Some(reasoning) = delta.get("reasoningContent") {
                        // A signature is a tamper check over text kGraf does not
                        // replay, so it is not kept.
                        let fragment =
                            if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                                Some(text.to_owned())
                            } else {
                                reasoning
                                    .get("redactedContent")
                                    .and_then(Value::as_str)
                                    .map(|data| format!("[redacted_thinking:{data}]"))
                            };
                        if let Some(fragment) = fragment {
                            match blocks
                                .entry(index)
                                .or_insert_with(|| Block::Reasoning(String::new()))
                            {
                                Block::Reasoning(buffer) => buffer.push_str(&fragment),
                                _ => {
                                    return Err(decode(format!(
                                        "a reasoning delta for content block {index}, which is \
                                         not a reasoning block"
                                    )));
                                }
                            }
                            sink(StreamEvent::Delta {
                                content: Content::Reasoning { text: fragment },
                            });
                        } else if reasoning.get("signature").is_none() {
                            return Err(decode(
                                "a reasoningContent delta has no text, signature or redacted \
                                 content",
                            ));
                        }
                    } else {
                        let kinds: Vec<&str> = delta
                            .as_object()
                            .map(|o| o.keys().map(String::as_str).collect())
                            .unwrap_or_default();
                        return Err(decode(format!(
                            "a content block delta this adapter does not model: {kinds:?}"
                        )));
                    }
                }
                "contentBlockStop" => {
                    let index = block_index(&event)?;
                    if let Some(block) = blocks.remove(&index) {
                        match block.finish() {
                            Ok(finished) => {
                                if matches!(finished, Content::ToolUse { .. }) {
                                    // A tool call could not be sunk per fragment.
                                    sink(StreamEvent::Delta {
                                        content: finished.clone(),
                                    });
                                }
                                content.insert(index, finished);
                            }
                            Err(error) => {
                                broken_tool_call.get_or_insert(error);
                            }
                        }
                    }
                }
                "messageStop" => {
                    let reason = event
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .ok_or_else(|| decode("a messageStop event has no `stopReason`"))?;
                    if let Ok(Stop::Usable(stop_reason)) = wire::map_stop(reason) {
                        sink(StreamEvent::Finished { stop_reason });
                    }
                    stop = Some(reason.to_owned());
                }
                "metadata" => {
                    let raw = event
                        .get("usage")
                        .ok_or(ProviderError::UsageMissing { adapter: ADAPTER })?;
                    let tier = event.get("serviceTier");
                    match wire::normalize_usage(raw) {
                        Ok(mut normalized) => {
                            wire::record_tier(&mut normalized, tier);
                            unpriced = wire::pricing_refusal(raw, tier);
                            guard.record_usage(&normalized);
                            sink(StreamEvent::Usage {
                                usage: normalized.clone(),
                            });
                            usage = Some(normalized);
                        }
                        Err(error) => {
                            if let Some(floor) = wire::lower_bound_usage(raw) {
                                guard.record_usage(&floor);
                            }
                            return Err(error);
                        }
                    }
                }
                // An event type added later carries no content this adapter models;
                // content arrives only through the block events above.
                _ => {}
            }
        }

        let Some(usage) = usage else {
            // The stream ended before `metadata`, the only event that carries usage.
            // The call may have billed; the row stays open with its request id.
            return Err(ProviderError::UsageMissing { adapter: ADAPTER });
        };
        let Some(stop) = stop else {
            return Err(ProviderError::StreamTruncated { adapter: ADAPTER });
        };
        let raw_finish = stop.clone();
        let stop = wire::map_stop(&stop);
        let max_tokens = matches!(stop, Ok(Stop::Usable(StopReason::MaxTokens)));
        if !max_tokens && let Some(error) = broken_tool_call.take() {
            return Err(error);
        }
        if !blocks.is_empty() {
            return Err(decode(format!(
                "{} content block(s) never received a contentBlockStop before the stream ended",
                blocks.len()
            )));
        }

        guard.disarm();
        match &unpriced {
            Some(reason) => {
                tracing::warn!(adapter = ADAPTER, %reason, "usage left unpriced");
                token
                    .leave_unreconciled_with_async(Some(&usage), request_id.as_deref())
                    .await?;
            }
            None => token.settle_async(&usage, request_id.as_deref()).await?,
        }
        match stop? {
            Stop::Unusable(stop_reason) => Err(ProviderError::TurnNotUsable {
                adapter: ADAPTER,
                stop_reason,
            }),
            Stop::Usable(stop_reason) => Ok(Response {
                finish_reason: Some(raw_finish),
                model: request.model.clone(),
                content: content.into_values().collect(),
                stop_reason,
                usage,
                request_id,
            }),
        }
    }
}

#[cfg(test)]
mod tests;
