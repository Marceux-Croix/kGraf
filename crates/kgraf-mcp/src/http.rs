//! The Streamable HTTP side of [`crate::RmcpCaller`]: header values read from the
//! environment, credentials scrubbed from everything the server sends back, and
//! transport failures turned into typed [`McpConnectError`]s.
//!
//! **What is reused and what is not (reuse existing code).** The transport is `rmcp`'s
//! `StreamableHttpClientTransport` (Apache-2.0, 3.4.1): POST of JSON-RPC,
//! `application/json` and `text/event-stream` replies, `Mcp-Session-Id`, the
//! optional GET stream with `Last-Event-ID` resumption, and DELETE when the session
//! ends. Under it runs [`crate::http_client::BoundedClient`], `rmcp`'s own `reqwest`
//! client adapted to bound every body. SSE framing is the `sse-stream` crate (MIT OR
//! Apache-2.0), which keeps `id:` and `retry:`. `kgraf-provider`'s `SseDecoder`
//! was considered and not used: it returns JSON payloads only, drops `id:` and
//! `retry:` (which resumption needs), and raises `ProviderError`. The code in this
//! file is new, because it is policy no dependency has: where header values come
//! from, and what a server may echo back.
//!
//! **Why scrubbing sits inside the HTTP client, not at the error boundary.**
//! `rmcp` puts a failed response's body into
//! `StreamableHttpError::UnexpectedServerResponse("HTTP {status}: {body}")`, and
//! its worker logs that error through `tracing::error!` before any caller sees it
//! (`transport/worker.rs`, "worker quit with fatal"). A server that echoes the
//! request's `Authorization` header in a 401 body would put the credential into
//! the trace. [`RedactingClient`] scrubs every error, JSON reply and SSE event on
//! its way into `rmcp`, so no later layer (`rmcp`'s traces, its `Debug` output,
//! this crate's errors, the model's tool result) holds the credential.
//!
//! **Replies are scrubbed as decoded JSON, not as text ** JSON may
//! escape any character (`\/` for `/`, `A` for `A`), and a credential with a
//! `"` or `\` in it is always escaped, so a text search of the raw body misses
//! credentials the model then reads decoded. Every reply is parsed, and each string
//! in it (object keys included) is searched after decoding. Numbers, booleans and
//! structure are never changed. A reply that is not JSON is scrubbed as text.
//!
//! **Only real credentials are scrubbed.** A header is scrubbed only
//! when it is a secret ([`HeaderFromEnv`] says which), and a secret shorter than
//! [`MIN_SECRET_CHARS`] is refused at connect: removing `2.0` from every reply
//! would corrupt `"jsonrpc": "2.0"` itself.
//!
//! **Known limit.** A credential is found only when it appears whole
//! inside one JSON string. A server that splits it across two strings, two SSE
//! events or two replies, or encodes it (base64, reversed), passes it through. The
//! scrubbing protects against a careless server echoing a header, not against a
//! hostile one that wants the credential out: such a server already received it.
//!
//! **Fail loud (fail loud).** A header variable that is not set fails the connect
//! with no request sent. A 404 on a request that carried a session id is
//! [`McpConnectError::SessionExpired`]; `rmcp`'s own recovery, which re-runs
//! `initialize` and retries the call on a new session, is turned off, because a
//! new session on the server's side can silently drop state the run relied on.
//! A server that answers `initialize` with a protocol version outside
//! [`SUPPORTED_HTTP_PROTOCOL_VERSIONS`] is refused. No failure falls back to
//! another transport.

use crate::client::McpConnectError;
use crate::config::{HeaderFromEnv, REDACTED};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::service::ClientInitializeError;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use serde_json::Value;
use sse_stream::Sse;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::env::VarError;
use std::future::Future;
use std::sync::Arc;

/// The MCP revisions whose Streamable HTTP transport this client speaks. The
/// Streamable HTTP transport first appears in 2025-03-26; 2025-06-18 is the floor
/// because it made `MCP-Protocol-Version` on every request mandatory, which the
/// transport sends. A server that negotiates anything else is refused at connect.
pub const SUPPORTED_HTTP_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-11-25"];

/// The longest server-supplied text kept in one error, in characters. A server's
/// error body is not bounded by anything else on this path.
pub const MAX_ERROR_DETAIL_CHARS: usize = 512;

/// The shortest secret header value accepted. Scrubbing a shorter one from replies
/// would remove ordinary text (`2.0`, `acme`) and corrupt them; a credential this
/// short is not one anyway.
pub const MIN_SECRET_CHARS: usize = 8;

/// The shortest credential after `Bearer` or `Basic` that is scrubbed by pattern,
/// and the shortest cut-off tail of a known secret that is scrubbed. Shorter
/// matches are too likely to be ordinary words.
const MIN_PATTERN_CHARS: usize = 8;

/// Replaces known secret values, and the credential after a `Bearer` or `Basic`
/// scheme, with [`REDACTED`]. Cheap to clone (the secrets are shared).
#[derive(Clone, Default)]
pub(crate) struct Redactor {
    /// Longest first, so a full header value (`Bearer abc`) is replaced before
    /// its own variable value (`abc`) could leave the prefix behind.
    secrets: Arc<[String]>,
}

impl Redactor {
    pub(crate) fn new(mut secrets: Vec<String>) -> Self {
        secrets.retain(|s| !s.is_empty());
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self {
            secrets: secrets.into(),
        }
    }

    fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Replace every known secret in one decoded string.
    pub(crate) fn scrub_data<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if !self.secrets.iter().any(|s| text.contains(s.as_str())) {
            return Cow::Borrowed(text);
        }
        let mut out = text.to_owned();
        for secret in self.secrets.iter() {
            out = out.replace(secret.as_str(), REDACTED);
        }
        Cow::Owned(out)
    }

    /// Scrub every string in `value`, object keys included. `true` if any changed.
    pub(crate) fn scrub_value(&self, value: &mut Value) -> bool {
        match value {
            Value::String(text) => match self.scrub_data(text) {
                Cow::Borrowed(_) => false,
                Cow::Owned(scrubbed) => {
                    *text = scrubbed;
                    true
                }
            },
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |changed, item| self.scrub_value(item) | changed),
            Value::Object(map) => {
                let mut changed = map
                    .values_mut()
                    .fold(false, |changed, item| self.scrub_value(item) | changed);
                // Only a key that holds a secret is moved; the rest stay put.
                let renames: Vec<(String, String)> = map
                    .keys()
                    .filter_map(|key| match self.scrub_data(key) {
                        Cow::Borrowed(_) => None,
                        Cow::Owned(scrubbed) => Some((key.clone(), scrubbed)),
                    })
                    .collect();
                for (old, new) in renames {
                    if let Some(item) = map.remove(&old) {
                        // Two keys that scrub to the same text must not merge,
                        // one silently replacing the other: a later
                        // one gets a numbered suffix.
                        let mut key = new.clone();
                        let mut n = 2;
                        while map.contains_key(&key) {
                            key = format!("{new}#{n}");
                            n += 1;
                        }
                        map.insert(key, item);
                        changed = true;
                    }
                }
                changed
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => false,
        }
    }

    /// Scrub one SSE event's data: as JSON when it parses (the normal case), as
    /// text otherwise. `None` when nothing changed.
    fn scrub_event_data(&self, data: &str) -> Option<String> {
        match serde_json::from_str::<Value>(data) {
            Ok(mut value) => {
                if self.scrub_value(&mut value) {
                    serde_json::to_string(&value).ok()
                } else {
                    None
                }
            }
            Err(_) => match self.scrub_data(data) {
                Cow::Borrowed(_) => None,
                Cow::Owned(scrubbed) => Some(scrubbed),
            },
        }
    }

    /// Scrub a text that becomes part of an error: known secrets, a secret cut
    /// off at the end, any `Bearer`/`Basic` credential (this also covers an OAuth
    /// token, which this crate does not hold), then a length cap.
    pub(crate) fn scrub_text(&self, text: &str) -> String {
        let mut out = self.scrub_data(text).into_owned();
        for secret in self.secrets.iter() {
            if let Some(cut) = secret_tail(&out, secret) {
                out.truncate(cut);
                out.push_str(REDACTED);
            }
        }
        cap(&scrub_auth_schemes(&out))
    }

    fn scrub_error<E>(&self, error: StreamableHttpError<E>) -> StreamableHttpError<E>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match error {
            StreamableHttpError::UnexpectedServerResponse(text) => {
                StreamableHttpError::UnexpectedServerResponse(Cow::Owned(self.scrub_text(&text)))
            }
            StreamableHttpError::UnexpectedContentType(Some(text)) => {
                StreamableHttpError::UnexpectedContentType(Some(self.scrub_text(&text)))
            }
            other => other,
        }
    }

    /// A JSON-RPC message with every known secret removed from its strings. The
    /// round trip through `Value` is paid only when this server has secrets.
    fn scrub_message<E>(
        &self,
        message: ServerJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, StreamableHttpError<E>>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        if self.is_empty() {
            return Ok(message);
        }
        let mut value = serde_json::to_value(&message)?;
        if !self.scrub_value(&mut value) {
            return Ok(message);
        }
        serde_json::from_value(value).map_err(|_| {
            StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                "a response carried a configured header credential and could not be \
                 rebuilt after the credential was removed",
            ))
        })
    }

    fn scrub_stream(
        &self,
        stream: BoxStream<'static, Result<Sse, SseError>>,
    ) -> BoxStream<'static, Result<Sse, SseError>> {
        if self.is_empty() {
            return stream;
        }
        let redactor = self.clone();
        stream
            .map(move |item| {
                item.map(|mut event| {
                    if let Some(scrubbed) = event
                        .data
                        .as_deref()
                        .and_then(|data| redactor.scrub_event_data(data))
                    {
                        event.data = Some(scrubbed);
                    }
                    event
                })
            })
            .boxed()
    }

    fn scrub_post<E>(
        &self,
        result: Result<StreamableHttpPostResponse, StreamableHttpError<E>>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<E>>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match result {
            Err(error) => Err(self.scrub_error(error)),
            Ok(StreamableHttpPostResponse::Json(message, session)) => Ok(
                StreamableHttpPostResponse::Json(self.scrub_message(message)?, session),
            ),
            Ok(StreamableHttpPostResponse::Sse(stream, session)) => Ok(
                StreamableHttpPostResponse::Sse(self.scrub_stream(stream), session),
            ),
            Ok(other) => Ok(other),
        }
    }
}

/// Where `text` ends in a leading part of `secret` at least [`MIN_PATTERN_CHARS`]
/// bytes long, the byte offset that part starts at. Covers a body cut off inside
/// a credential.
fn secret_tail(text: &str, secret: &str) -> Option<usize> {
    (MIN_PATTERN_CHARS..secret.len())
        .rev()
        .filter(|&len| secret.is_char_boundary(len))
        .find(|&len| text.ends_with(&secret[..len]))
        .map(|len| text.len() - len)
}

/// A token68 character (RFC 9110 section 11.2), the shape of a credential.
fn is_token68_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~+/".contains(c)
}

/// Replace a credential after an HTTP auth scheme word (`Bearer x`, `Basic x`),
/// matched ignoring case at a word start. The value must look like a credential:
/// at least [`MIN_PATTERN_CHARS`] token68 characters, optional `=` padding, then
/// the end of the text, a space or a delimiter. An auth parameter
/// (`Bearer realm="x"`) and an ordinary phrase (`Basic auth failed`) are left
/// alone.
fn scrub_auth_schemes(text: &str) -> String {
    const SCHEMES: [&str; 2] = ["bearer ", "basic "];
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let at_word_start = i == 0
            || !text[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let scheme = if at_word_start {
            SCHEMES.iter().find(|s| lower[i..].starts_with(*s))
        } else {
            None
        };
        let Some(scheme) = scheme else {
            if let Some(c) = text[i..].chars().next() {
                out.push(c);
                i += c.len_utf8();
            }
            continue;
        };
        out.push_str(&text[i..i + scheme.len()]);
        i += scheme.len();
        let rest = &text[i..];
        let body = rest
            .find(|c: char| !is_token68_char(c))
            .unwrap_or(rest.len());
        let padded = body + rest[body..].chars().take_while(|&c| c == '=').count();
        let next = rest[padded..].chars().next();
        let ends_cleanly = next.is_none_or(|c| c.is_whitespace() || ",;\"'}]".contains(c));
        // `name="value"` is an auth parameter, not a credential.
        let is_auth_param = padded == body + 1 && next == Some('"');
        if body >= MIN_PATTERN_CHARS && ends_cleanly && !is_auth_param {
            out.push_str(REDACTED);
            i += padded;
        }
    }
    out
}

fn cap(text: &str) -> String {
    let mut chars = text.chars();
    let kept: String = chars.by_ref().take(MAX_ERROR_DETAIL_CHARS).collect();
    if chars.next().is_some() {
        format!("{kept} [truncated]")
    } else {
        kept
    }
}

/// Header values read from the environment, with the redactor that knows the
/// secret ones.
pub(crate) struct ResolvedHeaders {
    pub(crate) map: HeaderMap,
    pub(crate) redactor: Redactor,
}

/// Read every configured header from `lookup` (the process environment in
/// production). Fails on the first header that cannot be sent; the error names
/// the header and the variable, never the value.
pub(crate) fn resolve_headers(
    server: &str,
    headers: &BTreeMap<String, HeaderFromEnv>,
    lookup: &(dyn Fn(&str) -> Result<String, VarError> + Sync),
) -> Result<ResolvedHeaders, McpConnectError> {
    let mut map = HeaderMap::new();
    let mut secrets = Vec::new();
    for (name, source) in headers {
        let fail = |reason: &'static str| McpConnectError::HeaderEnv {
            server: server.to_owned(),
            header: name.clone(),
            variable: source.env().to_owned(),
            reason,
        };
        // Validated at config load; a config built another way still fails here.
        let secret = source.is_secret(name).map_err(fail)?;
        let value = match lookup(source.env()) {
            Ok(value) => value,
            Err(VarError::NotPresent) => return Err(fail("is not set")),
            Err(VarError::NotUnicode(_)) => return Err(fail("is not valid Unicode")),
        };
        if value.is_empty() {
            return Err(fail("is empty"));
        }
        if secret && value.chars().count() < MIN_SECRET_CHARS {
            return Err(fail(
                "holds a secret shorter than 8 characters, too short to remove from replies \
                 without corrupting them; if the header is not a credential, mark it \
                 \"secret\": false",
            ));
        }
        let full = format!("{}{value}", source.prefix());
        let header_name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| McpConnectError::BadHeader {
                server: server.to_owned(),
                reason: format!("`{name}` is not a valid header name"),
            })?;
        let mut header_value = HeaderValue::from_str(&full)
            .map_err(|_| fail("holds a character an HTTP header value cannot carry"))?;
        if secret {
            // `http` prints a sensitive value as `Sensitive` in `Debug`, and HPACK
            // never indexes it, so neither a trace nor an HTTP/2 table keeps it.
            header_value.set_sensitive(true);
            // Also registered with kGraf's process-wide scrubber (decision
            // 0071), which is a *different* protection than `Redactor` below:
            // `Redactor` scrubs what this server echoes back over this one
            // connection; `kgraf_secrets` scrubs the same value out of every
            // worker transcript and the shell, for the life of the process, in
            // case a tool prints this header's value for some other reason
            // (for example a worker `curl`s the same endpoint by hand and this
            // env var's value shows up in its command line or output).
            // A refused value is said to the user by the scrubber; this `Redactor` still covers it for MCP replies below.
            kgraf_secrets::register_exact_secret("mcp-header-env", &value);
            secrets.push(value);
            secrets.push(full);
        }
        map.insert(header_name, header_value);
    }
    Ok(ResolvedHeaders {
        map,
        redactor: Redactor::new(secrets),
    })
}

/// A [`StreamableHttpClient`] that scrubs what the server returns before `rmcp`
/// sees it (see the module docs). Every call goes to `inner`, including the
/// `*_with_max_sse_event_size` variants, so the per-event size cap still applies
/// at the byte layer.
#[derive(Clone)]
pub(crate) struct RedactingClient<C> {
    inner: C,
    redactor: Redactor,
}

impl<C> RedactingClient<C> {
    pub(crate) fn new(inner: C, redactor: Redactor) -> Self {
        Self { inner, redactor }
    }
}

type SseResult<E> = Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<E>>;

impl<C> StreamableHttpClient for RedactingClient<C>
where
    C: StreamableHttpClient + Sync,
{
    type Error = C::Error;

    fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>>>
    + Send
    + '_ {
        let call = self
            .inner
            .post_message(uri, message, session_id, auth_header, custom_headers);
        async move { self.redactor.scrub_post(call.await) }
    }

    fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<Output = Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>>>
    + Send
    + '_ {
        let call = self.inner.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            max_sse_event_size,
        );
        async move { self.redactor.scrub_post(call.await) }
    }

    fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<(), StreamableHttpError<Self::Error>>> + Send + '_ {
        let call = self
            .inner
            .delete_session(uri, session_id, auth_header, custom_headers);
        async move { call.await.map_err(|e| self.redactor.scrub_error(e)) }
    }

    fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = SseResult<Self::Error>> + Send + '_ {
        let call =
            self.inner
                .get_stream(uri, session_id, last_event_id, auth_header, custom_headers);
        async move {
            call.await
                .map(|stream| self.redactor.scrub_stream(stream))
                .map_err(|e| self.redactor.scrub_error(e))
        }
    }

    fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<Output = SseResult<Self::Error>> + Send + '_ {
        let call = self.inner.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            max_sse_event_size,
        );
        async move {
            call.await
                .map(|stream| self.redactor.scrub_stream(stream))
                .map_err(|e| self.redactor.scrub_error(e))
        }
    }
}

/// The status code at the start of `rmcp`'s `"HTTP {status}: {body}"` text. `rmcp`
/// formats that text itself (`common/reqwest/streamable_http_client.rs`), and it
/// carries no status field, so the text is the only place the code is.
fn leading_http_status(text: &str) -> Option<u16> {
    let rest = text.strip_prefix("HTTP ")?;
    let digits = rest.get(..3)?;
    if digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

fn chain_text(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

/// A transport-level failure as a typed error. Both `reqwest::Client` and
/// `AuthClient<reqwest::Client>` report `reqwest::Error`, so one downcast covers
/// both connect paths.
fn classify_transport(
    server: &str,
    error: &(dyn std::error::Error + Send + Sync + 'static),
    redactor: &Redactor,
) -> McpConnectError {
    let server = server.to_owned();
    let Some(http) = error.downcast_ref::<StreamableHttpError<reqwest::Error>>() else {
        return McpConnectError::HttpTransport {
            server,
            detail: redactor.scrub_text(&chain_text(error)),
        };
    };
    match http {
        StreamableHttpError::SessionExpired => McpConnectError::SessionExpired { server },
        StreamableHttpError::AuthRequired(_) | StreamableHttpError::InsufficientScope(_) => {
            McpConnectError::AuthorizationRequired {
                server,
                challenge: redactor.scrub_text(http.auth_challenge().unwrap_or_default()),
            }
        }
        StreamableHttpError::UnexpectedServerResponse(text) => match leading_http_status(text) {
            Some(status) => McpConnectError::HttpStatus {
                server,
                status,
                detail: redactor.scrub_text(text),
            },
            None => McpConnectError::HttpTransport {
                server,
                detail: redactor.scrub_text(text),
            },
        },
        StreamableHttpError::Client(inner) if inner.status().is_some() => {
            McpConnectError::HttpStatus {
                server,
                status: inner.status().map(|s| s.as_u16()).unwrap_or_default(),
                detail: redactor.scrub_text(&chain_text(inner)),
            }
        }
        other => McpConnectError::HttpTransport {
            server,
            detail: redactor.scrub_text(&chain_text(other)),
        },
    }
}

/// A failed request on a connected HTTP peer, as a typed error.
pub(crate) fn classify_service_error(
    server: &str,
    error: &rmcp::ServiceError,
    redactor: &Redactor,
) -> McpConnectError {
    match error {
        rmcp::ServiceError::TransportSend(inner) => {
            classify_transport(server, inner.error.as_ref(), redactor)
        }
        rmcp::ServiceError::McpError(data) => McpConnectError::ServerError {
            server: server.to_owned(),
            detail: redactor.scrub_text(&data.message),
        },
        other => McpConnectError::HttpTransport {
            server: server.to_owned(),
            detail: redactor.scrub_text(&chain_text(other)),
        },
    }
}

/// A failed `initialize` over HTTP, as a typed error.
pub(crate) fn classify_init_error(
    server: &str,
    error: &ClientInitializeError,
    redactor: &Redactor,
) -> McpConnectError {
    if let Some(challenge) = error.auth_challenge() {
        return McpConnectError::AuthorizationRequired {
            server: server.to_owned(),
            challenge: redactor.scrub_text(challenge),
        };
    }
    match error {
        ClientInitializeError::TransportError { error, .. } => {
            classify_transport(server, error.error.as_ref(), redactor)
        }
        ClientInitializeError::JsonRpcError(data) => McpConnectError::ServerError {
            server: server.to_owned(),
            detail: redactor.scrub_text(&data.message),
        },
        other => McpConnectError::HttpTransport {
            server: server.to_owned(),
            detail: redactor.scrub_text(&chain_text(other)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lookup(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Result<String, VarError> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
                .ok_or(VarError::NotPresent)
        }
    }

    fn headers(pairs: &[(&str, HeaderFromEnv)]) -> BTreeMap<String, HeaderFromEnv> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn a_secret_header_is_read_from_its_variable_and_marked_sensitive() {
        let resolved = resolve_headers(
            "s",
            &headers(&[(
                "Authorization",
                HeaderFromEnv::new("TOKEN").with_prefix("Bearer "),
            )]),
            &lookup(&[("TOKEN", "abc12345")]),
        )
        .expect("resolves");
        let value = &resolved.map["authorization"];
        assert_eq!(value.to_str().expect("ascii"), "Bearer abc12345");
        assert!(value.is_sensitive());
        assert!(!format!("{:?}", resolved.map).contains("abc12345"));
        assert_eq!(
            resolved.redactor.scrub_data("x abc12345"),
            format!("x {REDACTED}")
        );
    }

    /// A public header is sent but never scrubbed, and a secret too short to
    /// scrub safely is refused before any request.
    #[test]
    fn a_public_header_is_not_scrubbed_and_a_short_secret_is_refused() {
        let resolved = resolve_headers(
            "s",
            &headers(&[("X-Api-Version", HeaderFromEnv::new("V").with_secret(false))]),
            &lookup(&[("V", "2.0")]),
        )
        .expect("a public header resolves");
        assert_eq!(resolved.map["x-api-version"], "2.0");
        assert!(!resolved.map["x-api-version"].is_sensitive());
        assert!(matches!(
            resolved.redactor.scrub_data(r#"{"jsonrpc":"2.0"}"#),
            Cow::Borrowed(_)
        ));

        let Err(error) = resolve_headers(
            "s",
            &headers(&[("X-Api-Key", HeaderFromEnv::new("K"))]),
            &lookup(&[("K", "acme")]),
        ) else {
            panic!("a four-character secret must be refused");
        };
        let text = error.to_string();
        assert!(text.contains("shorter than 8"), "{text}");
        assert!(!text.contains("acme"), "{text}");
    }

    #[test]
    fn an_unset_empty_or_unsendable_variable_fails_without_its_value() {
        let unset = resolve_headers(
            "s",
            &headers(&[("X-Key", HeaderFromEnv::new("MISSING"))]),
            &lookup(&[]),
        );
        assert!(matches!(
            unset,
            Err(McpConnectError::HeaderEnv { reason: "is not set", ref variable, .. })
                if variable == "MISSING"
        ));
        let empty = resolve_headers(
            "s",
            &headers(&[("X-Key", HeaderFromEnv::new("E"))]),
            &lookup(&[("E", "")]),
        );
        assert!(matches!(
            empty,
            Err(McpConnectError::HeaderEnv {
                reason: "is empty",
                ..
            })
        ));
        let Err(bad) = resolve_headers(
            "s",
            &headers(&[("X-Key", HeaderFromEnv::new("B"))]),
            &lookup(&[("B", "secret-value\nX-Forged: 1")]),
        ) else {
            panic!("a value with a line break must be refused");
        };
        assert!(!bad.to_string().contains("secret-value"), "{bad}");
    }

    /// Strings are scrubbed after decoding, keys too; numbers and
    /// structure are untouched even when they spell the secret.
    #[test]
    fn keys_that_scrub_to_the_same_text_stay_distinct() {
        let r = Redactor::new(vec!["alpha-secret-1".into(), "beta-secret-22".into()]);
        let mut value = json!({
            "alpha-secret-1": 1,
            "beta-secret-22": 2,
            REDACTED: 3,
        });
        assert!(r.scrub_value(&mut value));
        let map = value.as_object().expect("object");
        assert_eq!(map.len(), 3, "no entry was lost: {value}");
        let mut numbers: Vec<i64> = map.values().filter_map(|v| v.as_i64()).collect();
        numbers.sort_unstable();
        assert_eq!(numbers, [1, 2, 3]);
        assert!(map.keys().all(|k| k.starts_with(REDACTED)), "{value}");
    }

    #[test]
    fn scrub_value_changes_decoded_strings_only() {
        let r = Redactor::new(vec!["12345678".into(), r#"q"uo\te-99"#.into()]);
        let mut value = json!({
            "id": 12345678,
            "text": "a 12345678 b",
            "12345678": [r#"q"uo\te-99"#, true, null, 1.5],
        });
        assert!(r.scrub_value(&mut value));
        assert_eq!(
            value,
            json!({
                "id": 12345678,
                "text": format!("a {REDACTED} b"),
                REDACTED: [REDACTED, true, null, 1.5],
            })
        );
        let mut clean = json!({"id": 1, "text": "nothing here"});
        assert!(!r.scrub_value(&mut clean));
    }

    #[test]
    fn event_data_is_scrubbed_after_json_decoding() {
        let r = Redactor::new(vec!["tok+secret/xyz".into()]);
        let scrubbed = r
            .scrub_event_data(r#"{"text":"echo tok+secret\/xyz"}"#)
            .expect("the escaped secret is found");
        assert!(!scrubbed.contains("secret"), "{scrubbed}");
        assert_eq!(r.scrub_event_data(r#"{"text":"clean"}"#), None);
        assert_eq!(
            r.scrub_event_data("not json tok+secret/xyz").as_deref(),
            Some(format!("not json {REDACTED}").as_str())
        );
    }

    /// Scheme scrubbing needs a credential-shaped value; auth parameters and
    /// ordinary words stay; the cut-off tail rule needs 8 characters.
    #[test]
    fn scrub_text_removes_credentials_and_leaves_ordinary_text() {
        let r = Redactor::new(vec!["tok-SECRET-9".into(), "Bearer tok-SECRET-9".into()]);
        assert_eq!(
            r.scrub_text("got Bearer tok-SECRET-9 and tok-SECRET-9"),
            format!("got {REDACTED} and {REDACTED}")
        );
        assert_eq!(r.scrub_text("body: tok-SECR"), format!("body: {REDACTED}"));
        assert_eq!(
            r.scrub_text("body: tok-SE"),
            "body: tok-SE",
            "under 8 characters"
        );
        assert_eq!(
            r.scrub_text("authorization: bearer oauth-xyz-123456, other"),
            format!("authorization: bearer {REDACTED}, other")
        );
        assert_eq!(
            r.scrub_text(r#"{"auth":"Bearer abcdefgh123=="}"#),
            format!(r#"{{"auth":"Bearer {REDACTED}"}}"#)
        );
        for untouched in [
            "Basic auth failed for this user",
            r#"Bearer realm="mcp", resource_metadata="https://a.example/x""#,
            "superbasic text",
            "Bearer short",
        ] {
            assert_eq!(r.scrub_text(untouched), untouched);
        }
        let long = "x".repeat(MAX_ERROR_DETAIL_CHARS + 10);
        assert!(r.scrub_text(&long).ends_with("[truncated]"));
    }

    #[test]
    fn the_status_is_read_from_rmcp_text_only_at_its_start() {
        assert_eq!(leading_http_status("HTTP 401 Unauthorized: x"), Some(401));
        assert_eq!(leading_http_status("HTTP/1.1 500"), None);
        assert_eq!(leading_http_status("not HTTP 500"), None);
    }
}

/// The scrubbing cost measurement, ported from a probe. Ignored in the normal run; run it in release:
/// `cargo test -p kgraf-mcp --release --lib scrub_cost -- --ignored --nocapture`.
#[cfg(test)]
mod scrub_bench {
    use super::*;

    /// A `tools/call` result of about `bytes`, in text parts of about 200
    /// characters; with `secret`, one part echoes the credential.
    fn reply(bytes: usize, secret: bool) -> String {
        let part = "lorem ipsum dolor sit amet, consectetur adipiscing elit; sed do eiusmod \
                    tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, \
                    quis nostrud exercitation ullamco laboris nisi";
        let mut parts = Vec::new();
        let mut size = 0;
        while size < bytes {
            parts.push(serde_json::json!({"type": "text", "text": part}));
            size += part.len() + 30;
        }
        if secret {
            parts.push(serde_json::json!({"type": "text", "text": "echo tok-SECRET-123456"}));
        }
        serde_json::to_string(
            &serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": {"content": parts}}),
        )
        .expect("json")
    }

    fn time<F: FnMut()>(label: &str, n: u32, mut f: F) {
        for _ in 0..3 {
            f();
        }
        let started = std::time::Instant::now();
        for _ in 0..n {
            f();
        }
        println!("BENCH {label}: {:?} per reply", started.elapsed() / n);
    }

    #[test]
    #[ignore = "a measurement, not a check; run in release with --ignored --nocapture"]
    fn scrub_cost() {
        let r = Redactor::new(vec![
            "tok-SECRET-123456".into(),
            "Bearer tok-SECRET-123456".into(),
        ]);
        for (name, bytes, n) in [("16KiB", 16 * 1024, 2000u32), ("1MiB", 1024 * 1024, 30)] {
            for secret in [false, true] {
                let data = reply(bytes, secret);
                let tag = format!("{name} len={} secret_present={secret}", data.len());
                time(&format!("{tag} rmcp parse alone (baseline)"), n, || {
                    let _: ServerJsonRpcMessage = serde_json::from_str(&data).expect("parse");
                });
                time(&format!("{tag} raw-text scrub_data"), n, || {
                    let _ = r.scrub_data(&data);
                });
                time(&format!("{tag} SSE scrub_event_data"), n, || {
                    let _ = r.scrub_event_data(&data);
                });
                let message: ServerJsonRpcMessage = serde_json::from_str(&data).expect("parse");
                time(&format!("{tag} JSON scrub_message"), n, || {
                    let _ = r.scrub_message::<std::io::Error>(message.clone());
                });
                time(&format!("{tag} message clone (subtract)"), n, || {
                    let _ = message.clone();
                });
            }
        }
    }
}
