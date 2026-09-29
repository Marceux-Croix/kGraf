//! [`BoundedClient`]: `rmcp`'s `StreamableHttpClient` for `reqwest`, with every
//! response body read under a byte cap and the GET stream paced.
//!
//! Adapted from modelcontextprotocol/rust-sdk at
//! 9427a929959e665e0d12e9395f674026baf4bd48 (the `rmcp` 3.4.1 release),
//! `crates/rmcp/src/transport/common/reqwest/streamable_http_client.rs`,
//! `client_side_sse.rs` (the per-event SSE size limiter) and `http_header.rs`
//! (`extract_scope_from_header`). Apache-2.0; see `third_party/rmcp/`.
//!
//! **Why a port and not a wrapper ** `rmcp`'s own client
//! reads an `application/json` reply with `response.bytes()` and a non-2xx body
//! with `response.text()`, both unbounded: a 40 MiB reply was accepted, and a
//! server could send any amount. The body is read inside that implementation, so
//! no wrapper around it can bound the read. Changes from upstream:
//!
//! - a JSON reply is refused when its `Content-Length` exceeds
//!   [`MAX_HTTP_BODY_BYTES`], and otherwise read as a stream that is refused the
//!   moment it passes the cap;
//! - an error body is read only up to [`MAX_ERROR_BODY_BYTES`]; the rest is not
//!   read, and the connection is dropped;
//! - GET streams are paced at one per [`MIN_GET_STREAM_INTERVAL`] (a
//!   server that sends `retry: 0` and closes the stream got 1904 GETs in 3 s,
//!   because `rmcp` resets its retry count after every successful reconnect, so no
//!   retry policy can stop that loop), and budgeted at
//!   [`MAX_GET_STREAM_REOPENS`] reopens per [`GET_STREAM_BUDGET_WINDOW`] per
//!   session; every GET is attributed as an `open_stream` event;
//! - an oversized event on a reply stream becomes a JSON-RPC error for that
//!   request, so the call fails at once instead of waiting out its timeout;
//! - the `server/discover` fallback for 2026-07-28 servers is dropped: kGraf
//!   uses the `initialize` lifecycle only and never sends `server/discover`.

use crate::attribution::{McpAttributionSink, McpCallEvent, McpCallKind, McpCallOutcome};
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, future};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage, RequestId, ServerJsonRpcMessage};
use rmcp::transport::common::http_header::{
    EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_MCP_PROTOCOL_VERSION, HEADER_SESSION_ID,
    JSON_MIME_TYPE,
};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient, StreamableHttpError,
    StreamableHttpPostResponse,
};
use sse_stream::{Sse, SseStream};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The largest reply body accepted: one `application/json` reply, or one SSE
/// event. The same as `rmcp`'s own per-event SSE default.
///
/// **Why this large.** A `tools/call` result can carry binary content (an image,
/// an audio clip, a blob resource) as base64, which `crate::parse` decodes and
/// hashes into a `BinarySummary` rather than forward (the model never sees it). A
/// screenshot or a PDF page runs to several MiB once base64-encoded; a cap near
/// 1 MiB would refuse ordinary tool results. Text results are capped much lower
/// later, by `parse`'s own limits.
///
/// **Worst-case memory.** `rmcp` runs at most `max_concurrent_requests` (16,
/// its default, which this crate keeps) ordinary POSTs to one server at once, so
/// buffered JSON replies are bounded by 16 x 16 MiB = 256 MiB per server,
/// plus up to about two copies of one reply while it is scrubbed (when the
/// server has secret headers). An SSE reply holds one event at a time. A global
/// in-flight byte budget across servers was considered and not built: a worker
/// talks to few servers, and its calls to one server are rarely concurrent, so
/// the bound above is far from reached in practice; it becomes worth building if
/// many workers share hosted servers at once.
pub const MAX_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;

/// How much of an error (non-2xx) body is read. Enough for any error message a
/// person or a model could use; the rest is never read.
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// The shortest time between two GET stream requests to one server, whatever
/// `retry:` the server asks for.
pub const MIN_GET_STREAM_INTERVAL: Duration = Duration::from_secs(1);

/// Bytes of an unusable JSON body kept in an error (upstream's preview length).
const JSON_BODY_PREVIEW_BYTES: usize = 256;

type Error = StreamableHttpError<reqwest::Error>;
type SseResult = Result<BoxStream<'static, Result<Sse, SseError>>, Error>;

/// How many times one session may open a GET stream within
/// [`GET_STREAM_BUDGET_WINDOW`]: the first open plus this many reopens. Past it
/// the session's GET stream is refused for good, and the refusal is recorded
/// (pacing alone still allowed 3,600 reconnects an hour, because
/// `rmcp` resets its retry count after every clean reconnect).
pub const MAX_GET_STREAM_REOPENS: usize = 10;

/// The window [`MAX_GET_STREAM_REOPENS`] is counted over.
pub const GET_STREAM_BUDGET_WINDOW: Duration = Duration::from_secs(60);

/// The session's GET stream reopen budget: opens in the current window, whether
/// opens are being refused right now, and when the last request was sent.
///
/// **Two kinds of GET.** A GET without `Last-Event-ID` (re)opens the
/// session's standing stream; it draws on the budget. A GET with `Last-Event-ID`
/// resumes a stream, and SEP-1699 polling does exactly that, once per poll, for
/// as long as a slow tool runs: a 15-poll call failed after 10 under the old
/// rule, and the session lost resumption for good. A resume is therefore free
/// while a request was sent within the call timeout (so it can still be that
/// request's reply), and counts against the budget otherwise (a standing stream
/// that happens to carry event ids, reopened with no call in flight). Pacing
/// applies to both.
///
/// **Refusal expires with the window.** Opens older than
/// [`GET_STREAM_BUDGET_WINDOW`] drop out, and the next open after that is
/// allowed again; a burst no longer costs the session its stream for good.
#[derive(Default)]
struct GetBudget {
    opens: VecDeque<std::time::Instant>,
    refusing: bool,
    last_request: Option<std::time::Instant>,
}

/// What [`GetBudget::admit`] decided.
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Allowed,
    /// Refused; `first` is true for the first refusal of a run of refusals, the
    /// one that is recorded and logged.
    Refused {
        first: bool,
    },
}

impl GetBudget {
    fn admit(
        &mut self,
        now: std::time::Instant,
        resume: bool,
        call_timeout: Duration,
    ) -> Admission {
        let request_in_flight = self
            .last_request
            .is_some_and(|sent| now.duration_since(sent) <= call_timeout);
        if resume && request_in_flight {
            return Admission::Allowed;
        }
        while self
            .opens
            .front()
            .is_some_and(|t| now.duration_since(*t) > GET_STREAM_BUDGET_WINDOW)
        {
            self.opens.pop_front();
        }
        if self.opens.len() > MAX_GET_STREAM_REOPENS {
            let first = !self.refusing;
            self.refusing = true;
            return Admission::Refused { first };
        }
        self.refusing = false;
        self.opens.push_back(now);
        Admission::Allowed
    }
}

/// `rmcp`'s `reqwest` client with bounded bodies and a paced, budgeted GET
/// stream. Every GET is attributed through the server's sink.
#[derive(Clone)]
pub(crate) struct BoundedClient {
    http: reqwest::Client,
    server: Arc<str>,
    sink: Arc<dyn McpAttributionSink>,
    /// When the last GET stream request started. Shared by every clone, since
    /// the transport clones its client per request.
    last_get: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>,
    budget: Arc<Mutex<GetBudget>>,
    /// The server's per-call timeout: how long after a request its reply may
    /// still be resumed without drawing on the budget.
    call_timeout: Duration,
}

impl BoundedClient {
    pub(crate) fn new(
        http: reqwest::Client,
        server: &str,
        sink: Arc<dyn McpAttributionSink>,
        call_timeout: Duration,
    ) -> Self {
        Self {
            http,
            server: server.into(),
            sink,
            last_get: Arc::new(tokio::sync::Mutex::new(None)),
            budget: Arc::new(Mutex::new(GetBudget::default())),
            call_timeout,
        }
    }

    fn record_get(&self, duration: Duration, outcome: McpCallOutcome) {
        self.sink.record(McpCallEvent {
            server: self.server.to_string(),
            tool: None,
            worker: None,
            task: None,
            kind: McpCallKind::OpenStream,
            duration,
            outcome,
            tool_is_error: None,
            bytes_in: 0,
            bytes_out: 0,
        });
    }

    /// Admit one GET stream open against the session's budget, or refuse it.
    /// The refusal that exhausts the budget is recorded and logged once; later
    /// ones are refused without a request or a record.
    fn admit_get(&self, resume: bool) -> Result<(), Error> {
        let admission = self.budget.lock().unwrap_or_else(|p| p.into_inner()).admit(
            std::time::Instant::now(),
            resume,
            self.call_timeout,
        );
        let Admission::Refused { first } = admission else {
            return Ok(());
        };
        if first {
            self.record_get(Duration::ZERO, McpCallOutcome::ProtocolError);
            tracing::error!(
                target: "kgraf_mcp::http",
                server = %self.server,
                "GET stream reopen budget exhausted; reopens are refused until the window moves"
            );
        }
        Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
            format!(
                "MCP server `{}`: GET stream reopen budget exhausted ({MAX_GET_STREAM_REOPENS} \
                 reopens per {} s, MAX_GET_STREAM_REOPENS); reopens are refused until the \
                 window moves",
                self.server,
                GET_STREAM_BUDGET_WINDOW.as_secs()
            ),
        )))
    }

    /// Note that a request is being sent, so resumes of its reply are free.
    fn note_request(&self) {
        self.budget
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_request = Some(std::time::Instant::now());
    }

    /// Wait until [`MIN_GET_STREAM_INTERVAL`] has passed since the previous GET.
    /// The lock is held across the wait, so concurrent GETs queue.
    async fn pace_get(&self) {
        let mut last = self.last_get.lock().await;
        if let Some(previous) = *last {
            tokio::time::sleep_until(previous + MIN_GET_STREAM_INTERVAL).await;
        }
        *last = Some(tokio::time::Instant::now());
    }
}

fn too_large(what: &str, cap: usize) -> Error {
    StreamableHttpError::UnexpectedServerResponse(Cow::Owned(format!(
        "the server's {what} exceeds the {cap}-byte limit and was refused"
    )))
}

/// Read at most `cap` bytes of `response`. `true` when more was left unread.
async fn read_capped(response: reqwest::Response, cap: usize) -> Result<(Vec<u8>, bool), Error> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let room = cap - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

fn preview(body: &[u8]) -> String {
    if body.is_empty() {
        return "<empty>".to_owned();
    }
    let lossy = String::from_utf8_lossy(body);
    let mut end = lossy.len().min(JSON_BODY_PREVIEW_BYTES);
    while !lossy.is_char_boundary(end) {
        end -= 1;
    }
    lossy[..end].to_owned()
}

fn apply_custom_headers(
    mut builder: reqwest::RequestBuilder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, Error> {
    for (name, value) in custom_headers {
        // Upstream's `validate_custom_header`: these are the transport's own,
        // except the protocol version, which the transport itself passes here.
        let reserved = ["accept", HEADER_SESSION_ID, HEADER_LAST_EVENT_ID]
            .iter()
            .any(|r| name.as_str().eq_ignore_ascii_case(r))
            && !name
                .as_str()
                .eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION);
        if reserved {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        builder = builder.header(name, value);
    }
    Ok(builder)
}

fn www_authenticate(response: &reqwest::Response) -> Result<Option<String>, Error> {
    match response.headers().get(WWW_AUTHENTICATE) {
        None => Ok(None),
        Some(value) => value.to_str().map(|s| Some(s.to_owned())).map_err(|_| {
            StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                "invalid www-authenticate header value",
            ))
        }),
    }
}

/// Upstream's `extract_scope_from_header`.
fn scope_from_challenge(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let start = lower.find("scope=")? + "scope=".len();
    let value = &header[start..];
    if let Some(quoted) = value.strip_prefix('"') {
        return quoted.find('"').map(|end| quoted[..end].to_owned());
    }
    let end = value
        .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .unwrap_or(value.len());
    (end > 0).then(|| value[..end].to_owned())
}

/// A 401 or 403 with a challenge becomes `rmcp`'s typed auth error.
fn auth_error(status: StatusCode, response: &reqwest::Response) -> Result<Option<Error>, Error> {
    let Some(challenge) = www_authenticate(response)? else {
        return Ok(None);
    };
    Ok(match status {
        StatusCode::UNAUTHORIZED => Some(StreamableHttpError::AuthRequired(
            AuthRequiredError::new(challenge),
        )),
        StatusCode::FORBIDDEN => {
            let scope = scope_from_challenge(&challenge);
            Some(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(challenge, scope),
            ))
        }
        _ => None,
    })
}

/// Upstream's `bounded_sse_stream`: SSE parsing over a byte stream that fails as
/// soon as one event grows past `max_event_size`. Built with `scan` instead of
/// upstream's `pin_project` struct, to need no further dependency.
///
/// **An oversized event ends the stream, not the session.** `rmcp`
/// treats only its own, private oversized-event error as final; any other stream
/// error makes it resume with `Last-Event-ID`, and the call then waited out its
/// timeout (60 s) and reported "timed out". So for a reply stream (`reply_to`
/// is the request's id) an oversized event becomes one JSON-RPC error answering
/// that request, and the stream ends: the call fails at once and says why. A GET
/// stream (no request to answer) passes the error on; `rmcp` reopens it, and the
/// session's reconnect budget bounds that.
fn bounded_sse_stream<S>(
    stream: S,
    max_event_size: usize,
    reply_to: Option<RequestId>,
) -> BoxStream<'static, Result<Sse, SseError>>
where
    S: futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let oversized = Arc::new(AtomicBool::new(false));
    let seen_oversized = oversized.clone();
    let limited = stream.scan(
        (EventSizeLimiter::new(max_event_size), false),
        move |(limiter, failed), item| {
            if *failed {
                return future::ready(None);
            }
            let out = match item {
                Ok(chunk) => {
                    if limiter.observe(&chunk) {
                        Ok(chunk)
                    } else {
                        *failed = true;
                        oversized.store(true, Ordering::SeqCst);
                        Err(SseBodyError::TooLarge(max_event_size))
                    }
                }
                Err(error) => {
                    *failed = true;
                    Err(SseBodyError::Source(error))
                }
            };
            future::ready(Some(out))
        },
    );
    let Some(request_id) = reply_to else {
        return SseStream::from_bytes_stream(limited).boxed();
    };
    SseStream::from_bytes_stream(limited)
        .scan(false, move |done, item| {
            if *done {
                return future::ready(None);
            }
            let out = match item {
                Err(_) if seen_oversized.load(Ordering::SeqCst) => {
                    *done = true;
                    Ok(oversized_reply(&request_id, max_event_size))
                }
                other => other,
            };
            future::ready(Some(out))
        })
        .boxed()
}

/// The SSE event that answers `request_id` with a JSON-RPC error saying the
/// server's reply was over the cap.
fn oversized_reply(request_id: &RequestId, cap: usize) -> Sse {
    let data = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {
            "code": -32603,
            "message": format!(
                "the server's reply exceeded the {} MiB limit and was dropped",
                cap / (1024 * 1024)
            ),
        },
    });
    Sse {
        event: None,
        data: Some(data.to_string()),
        id: None,
        retry: None,
    }
}

#[derive(Debug, thiserror::Error)]
enum SseBodyError {
    #[error(transparent)]
    Source(reqwest::Error),
    #[error("SSE event exceeded the maximum size of {0} bytes")]
    TooLarge(usize),
}

/// Upstream's `SseEventSizeLimiter`: the bytes one event retains, counted as
/// the SSE parser would (comment lines excluded, a newline per joined field).
struct EventSizeLimiter {
    max_size: usize,
    retained_size: usize,
    line_size: usize,
    line_is_comment: bool,
    previous_was_cr: bool,
}

impl EventSizeLimiter {
    fn new(max_size: usize) -> Self {
        Self {
            max_size,
            retained_size: 0,
            line_size: 0,
            line_is_comment: false,
            previous_was_cr: false,
        }
    }

    /// `false` once the current event is over the limit.
    fn observe(&mut self, chunk: &[u8]) -> bool {
        for &byte in chunk {
            if self.previous_was_cr {
                self.previous_was_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    if !self.finish_line() {
                        return false;
                    }
                    self.previous_was_cr = true;
                }
                b'\n' => {
                    if !self.finish_line() {
                        return false;
                    }
                }
                _ => {
                    if self.line_size == 0 {
                        self.line_is_comment = byte == b':';
                    }
                    self.line_size = self.line_size.saturating_add(1);
                    if !self.within_limit() {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn finish_line(&mut self) -> bool {
        if self.line_size == 0 {
            self.retained_size = 0;
        } else if !self.line_is_comment {
            self.retained_size = self
                .retained_size
                .saturating_add(self.line_size)
                .saturating_add(1);
        }
        self.line_size = 0;
        self.line_is_comment = false;
        self.within_limit()
    }

    fn within_limit(&self) -> bool {
        self.retained_size.saturating_add(self.line_size) <= self.max_size
    }
}

fn is_one_way(message: &ClientJsonRpcMessage) -> bool {
    matches!(
        message,
        ClientJsonRpcMessage::Notification(_)
            | ClientJsonRpcMessage::Response(_)
            | ClientJsonRpcMessage::Error(_)
    )
}

impl BoundedClient {
    async fn post(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let mut request = self
            .http
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        request = apply_custom_headers(request, custom_headers)?;
        if matches!(message, ClientJsonRpcMessage::Request(_)) {
            self.note_request();
        }
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = request.json(&message).send().await?;
        let status = response.status();
        if let Some(error) = auth_error(status, &response)? {
            return Err(error);
        }
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned());
        let content_length = response.content_length();
        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if status.is_success() && content_length == Some(0) && is_one_way(&message) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let is_json = content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with(JSON_MIME_TYPE));
        if !status.is_success() {
            let (body, truncated) = read_capped(response, MAX_ERROR_BODY_BYTES).await?;
            // A JSON-RPC error in a non-2xx reply is still the answer to the
            // request, if all of it was read.
            if is_json
                && !truncated
                && let Ok(error @ JsonRpcMessage::Error(_)) =
                    serde_json::from_slice::<ServerJsonRpcMessage>(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(error, session_id));
            }
            let mut text = String::from_utf8_lossy(&body).into_owned();
            if truncated {
                text.push_str(" [the rest of the error body was not read]");
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {text}"),
            )));
        }
        match content_type.as_deref() {
            Some(ct) if ct.starts_with(EVENT_STREAM_MIME_TYPE) => {
                let reply_to = match &message {
                    ClientJsonRpcMessage::Request(request) => Some(request.id.clone()),
                    _ => None,
                };
                let events =
                    bounded_sse_stream(response.bytes_stream(), max_sse_event_size, reply_to);
                Ok(StreamableHttpPostResponse::Sse(events, session_id))
            }
            Some(_) if is_json => {
                if content_length.is_some_and(|n| n > MAX_HTTP_BODY_BYTES as u64) {
                    return Err(too_large("reply", MAX_HTTP_BODY_BYTES));
                }
                let (body, truncated) = read_capped(response, MAX_HTTP_BODY_BYTES).await?;
                if truncated {
                    return Err(too_large("reply", MAX_HTTP_BODY_BYTES));
                }
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(parsed) => Ok(StreamableHttpPostResponse::Json(parsed, session_id)),
                    Err(error) if is_one_way(&message) => {
                        tracing::warn!(
                            "could not parse JSON response as ServerJsonRpcMessage, treating as \
                             accepted: {error}"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                    Err(error) => Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                        format!(
                            "could not parse JSON response as ServerJsonRpcMessage: {error}: {}",
                            preview(&body)
                        ),
                    ))),
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn get(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> SseResult {
        self.admit_get(last_event_id.is_some())?;
        self.pace_get().await;
        let started = std::time::Instant::now();
        let result = self
            .open_get(
                uri,
                session_id,
                last_event_id,
                auth_token,
                custom_headers,
                max_sse_event_size,
            )
            .await;
        let outcome = match &result {
            Ok(_) => McpCallOutcome::Success,
            Err(_) => McpCallOutcome::ProtocolError,
        };
        self.record_get(started.elapsed(), outcome);
        result
    }

    async fn open_get(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> SseResult {
        let mut request = self
            .http
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        request = apply_custom_headers(request, custom_headers)?;
        let response = request.send().await?;
        let status = response.status();
        if status == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if let Some(error) = auth_error(status, &response)? {
            return Err(error);
        }
        let response = response.error_for_status()?;
        match response.headers().get(CONTENT_TYPE) {
            Some(ct)
                if ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    || ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {}
            other => {
                return Err(StreamableHttpError::UnexpectedContentType(
                    other.map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned()),
                ));
            }
        }
        Ok(bounded_sse_stream(
            response.bytes_stream(),
            max_sse_event_size,
            None,
        ))
    }

    async fn delete(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        let mut request = self.http.delete(uri.as_ref());
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        request = request.header(HEADER_SESSION_ID, session.as_ref());
        request = apply_custom_headers(request, custom_headers)?;
        let response = request.send().await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            tracing::debug!("this server doesn't support deleting session");
            return Ok(());
        }
        response.error_for_status()?;
        Ok(())
    }
}

impl StreamableHttpClient for BoundedClient {
    type Error = reqwest::Error;

    fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<StreamableHttpPostResponse, Error>> + Send + '_ {
        self.post(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            MAX_HTTP_BODY_BYTES,
        )
    }

    fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<Output = Result<StreamableHttpPostResponse, Error>> + Send + '_ {
        self.post(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            max_sse_event_size.min(MAX_HTTP_BODY_BYTES),
        )
    }

    fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<(), Error>> + Send + '_ {
        self.delete(uri, session_id, auth_header, custom_headers)
    }

    fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = SseResult> + Send + '_ {
        self.get(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            MAX_HTTP_BODY_BYTES,
        )
    }

    fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<Output = SseResult> + Send + '_ {
        self.get(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            max_sse_event_size.min(MAX_HTTP_BODY_BYTES),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain reopens draw on the budget; the first refusal is marked; the
    /// refusal ends when the window moves on.
    #[test]
    fn the_reopen_budget_refuses_past_its_limit_and_recovers_with_the_window() {
        let timeout = Duration::from_secs(60);
        let start = std::time::Instant::now();
        let mut budget = GetBudget::default();
        for n in 0..=MAX_GET_STREAM_REOPENS {
            let at = start + Duration::from_secs(n as u64);
            assert_eq!(
                budget.admit(at, false, timeout),
                Admission::Allowed,
                "open {n}"
            );
        }
        let full = start + Duration::from_secs(20);
        assert_eq!(
            budget.admit(full, false, timeout),
            Admission::Refused { first: true }
        );
        assert_eq!(
            budget.admit(full, false, timeout),
            Admission::Refused { first: false }
        );
        // Once the first open is older than the window, one more is allowed.
        let later = start + GET_STREAM_BUDGET_WINDOW + Duration::from_millis(500);
        assert_eq!(budget.admit(later, false, timeout), Admission::Allowed);
    }

    /// A resume while a request is within its timeout is free; the same
    /// resume with no request in flight draws on the budget.
    #[test]
    fn a_resume_is_free_only_while_a_request_is_in_flight() {
        let timeout = Duration::from_secs(30);
        let start = std::time::Instant::now();
        let mut budget = GetBudget::default();
        for n in 0..=MAX_GET_STREAM_REOPENS {
            let at = start + Duration::from_millis(n as u64);
            assert_eq!(budget.admit(at, false, timeout), Admission::Allowed);
        }
        let spent = start + Duration::from_secs(1);
        assert!(matches!(
            budget.admit(spent, true, timeout),
            Admission::Refused { .. }
        ));
        budget.last_request = Some(spent);
        for poll in 0..100u64 {
            let at = spent + Duration::from_millis(100 * poll);
            assert_eq!(
                budget.admit(at, true, timeout),
                Admission::Allowed,
                "poll {poll}"
            );
            assert!(matches!(
                budget.admit(at, false, timeout),
                Admission::Refused { .. }
            ));
        }
        let after_timeout = spent + timeout + Duration::from_secs(1);
        assert!(matches!(
            budget.admit(after_timeout, true, timeout),
            Admission::Refused { .. }
        ));
    }

    #[test]
    fn the_event_limiter_counts_one_event_and_resets_on_a_blank_line() {
        let mut limiter = EventSizeLimiter::new(20);
        assert!(limiter.observe(b"data: 0123456789\n\n"));
        assert!(
            limiter.observe(b"data: 0123456789\n\n"),
            "a new event starts at zero"
        );
        assert!(!limiter.observe(b"data: 0123456789abcdefgh"));
        let mut comments = EventSizeLimiter::new(8);
        assert!(
            comments.observe(b": ping\n: ping\n: ping\n"),
            "comments are not retained"
        );
    }

    #[test]
    fn a_scope_is_read_from_a_quoted_or_bare_challenge() {
        assert_eq!(
            scope_from_challenge(r#"Bearer error="insufficient_scope", scope="a b""#).as_deref(),
            Some("a b")
        );
        assert_eq!(
            scope_from_challenge("Bearer scope=read:data, x=y").as_deref(),
            Some("read:data")
        );
        assert_eq!(scope_from_challenge("Bearer realm=x"), None);
    }
}
