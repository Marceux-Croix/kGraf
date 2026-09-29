//! [`McpCaller`] implemented on `rmcp` (Apache-2.0, `modelcontextprotocol/rust-sdk`,
//! license verified at crates.io 2026-09-26), for stdio and Streamable HTTP servers.
//!
//! **Why `rmcp` alone, and not codex-rs's `rmcp-client` too (0055 reuse survey).**
//! codex-rs's `rmcp-client` was evaluated. Its process-transport and OAuth code is
//! not separable from a dozen codex-internal crates it depends on directly
//! (`codex-api`, `codex-config`, `codex-secrets`, `codex-keyring-store`,
//! `codex-network-proxy`, `codex-otel`, three `codex-utils-*` crates), none of which
//! exist in kGraf and none of which would be "trimmed to what kGraf calls"
//! (`third_party/README.md`) without rewriting most of the file. It also carries
//! enterprise SSO and EMA identity-exchange machinery that decision 0051 places out
//! of scope for a single-user local app. Meanwhile the official `rmcp` crate we
//! already depend on for the base protocol has its **own** `transport::auth` module
//! (RFC 8414/9728 discovery, PKCE, SEP-991 dynamic client registration, reactive 401
//! challenge, refresh) that does the same job with none of that coupling. So the
//! OAuth seam in [`crate::oauth`] is built on `rmcp::transport::auth`, not vendored
//! from codex-rs. This is a reuse decision, not an omission: nothing here was
//! written from scratch that `rmcp` already provides.
//!
//! **One caller type, two transports.** [`RmcpCaller`] wraps whichever
//! `RunningService` the server's [`McpTransport`] needs. The two are not the same
//! Rust type (`RunningService<RoleClient, ()>` for stdio,
//! `RunningService<RoleClient, ClientConfig>` for Streamable HTTP), so the two
//! variants of the private `Peer` enum exist to give callers one type to hold,
//! matching how [`replace_server_tools`](crate::replace_server_tools) takes one
//! `Arc<dyn McpCaller>` regardless of transport.
//!
//! **Attribution is required, not optional.** Every constructor takes an
//! `Arc<dyn McpAttributionSink>`; there is no code path that builds an `RmcpCaller`
//! without one (decision 0051; see [`crate::attribution`]).
//!
//! **A stdio server's environment is not inherited wholesale.** The child starts
//! with a cleared environment plus a small allowlist
//! ([`INHERITED_ENV_ALLOWLIST`]) and the server's own configured `env`, so a
//! provider API key or other secret sitting in this process's environment for an
//! unrelated reason is not silently handed to every stdio MCP server .
//!
//! **A stdio server runs in its own process group** (a Unix process group via
//! `process-wrap`'s `ProcessGroup::leader`, a Windows Job Object via `JobObject`),
//! so killing it -- on `Drop` (already `rmcp`'s own behavior) or via
//! [`RmcpCaller::close`] -- reaps grandchildren an `npx`/`uvx` launcher spawned
//! instead of orphaning them. Its stderr is piped and read
//! by a bounded task that logs through `tracing`, never written raw to this
//! process's own terminal (`rmcp`'s default is `Stdio::inherit()`, which does that).

use crate::attribution::{McpAttributionSink, McpCallEvent, McpCallKind, McpCallOutcome};
use crate::config::{McpServerConfig, McpTransport};
use crate::http::{
    RedactingClient, Redactor, SUPPORTED_HTTP_PROTOCOL_VERSIONS, classify_init_error,
    classify_service_error, resolve_headers,
};
use crate::http_client::{BoundedClient, MAX_HTTP_BODY_BYTES, MIN_GET_STREAM_INTERVAL};
use crate::parse::parse_call_tool_result;
use crate::register::{DiscoveredTool, McpCaller};
use kgraf_tools::{CallPermit, ToolContext, ToolError, ToolOutput, UntrustedText};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, JsonObject,
};
use rmcp::service::RunningService;
use rmcp::transport::common::client_side_sse::ExponentialBackoff;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

/// Environment variables inherited from this process into every stdio MCP server's
/// environment, on top of its own configured `env` (a stdio
/// child no longer inherits this process's whole environment). Chosen as the
/// minimum a typical `node`/`npx`/`python`/`uvx`-launched server needs to run at
/// all: locate its interpreter and shared libraries, find its home directory for
/// caches and config, and produce readable (non-mojibake) output.
pub const INHERITED_ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "TMPDIR"];

/// Windows equivalent of [`INHERITED_ENV_ALLOWLIST`]'s `PATH`-adjacent entries:
/// without `SystemRoot`, Windows cannot resolve many DLLs and the child fails to
/// start at all.
#[cfg(windows)]
pub const INHERITED_ENV_ALLOWLIST_WINDOWS: &[&str] = &["SystemRoot"];

/// The longest one stderr line from a stdio server this crate will hold before
/// truncating it, and the most lines it will read before it stops logging further
/// ones (bounded memory for output this crate does not
/// control the size of). The pipe keeps draining past the line cap so the child
/// never blocks on a full stderr pipe; lines past the cap are counted, not logged.
const MAX_STDERR_LINE_CHARS: usize = 4_096;
const MAX_STDERR_LINES_LOGGED: usize = 200;

/// How often, at most, a failed GET stream is reopened before it is given up, and
/// the longest wait between two attempts. `rmcp`'s default retries forever
///. The GET stream is optional in MCP; requests and their replies
/// keep working without it.
const GET_STREAM_MAX_RETRIES: usize = 5;
const GET_STREAM_MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// kGraf's own MCP client identity, sent to every server at `initialize`.
fn client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("kgraf", env!("CARGO_PKG_VERSION")),
    )
}

/// A `reqwest` client builder with the one policy every Streamable HTTP client this
/// crate builds must share: **never follow a redirect**. A
/// server redirecting a request -- to `http://`, to a different host, to a URL
/// carrying attacker-chosen query parameters -- must not silently change where this
/// process's bearer token or OAuth-authorized requests go. `rmcp`'s own default
/// OAuth HTTP client already refuses redirects for the same reason; this makes the
/// non-OAuth path match it, and gives any caller building the base client that
/// later becomes an authorized one (via
/// [`crate::oauth::OAuthConnector::into_authorized_client`]) the same policy.
///
/// **No proxy from the environment**. `reqwest` otherwise routes
/// through `HTTP_PROXY`/`HTTPS_PROXY`, and a proxy sees every header, including
/// `Authorization`; for a plain-http loopback server it sees them in clear. A
/// proxy is used only when the server's configuration names one
/// ([`McpServerConfig::proxy`]).
pub fn base_http_client_builder(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(timeout)
}

/// The base client for one server: [`base_http_client_builder`], its headers,
/// and its configured proxy, if any.
fn http_client_for(
    config: &McpServerConfig,
    headers: reqwest::header::HeaderMap,
) -> Result<reqwest::Client, McpConnectError> {
    let setup = |reason: String| McpConnectError::Setup {
        server: config.name().to_owned(),
        reason,
    };
    let mut builder = base_http_client_builder(Duration::from_millis(config.timeout_ms()))
        .default_headers(headers);
    if let Some(proxy) = config.proxy() {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|e| setup(format!("unusable proxy: {}", e.without_url())))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| setup(e.without_url().to_string()))
}

/// A connected MCP server could not be reached, or refused an unauthenticated
/// connection.
#[derive(Debug, thiserror::Error)]
pub enum McpConnectError {
    /// The server's configuration names a transport this constructor does not
    /// speak (a stdio config passed to [`RmcpCaller::connect_streamable_http`], or
    /// the reverse). A programmer error at the call site (fail loud rather
    /// than silently trying the other transport).
    #[error("MCP server `{server}` is configured for {configured}, not {attempted}")]
    WrongTransport {
        server: String,
        configured: &'static str,
        attempted: &'static str,
    },

    /// A header could not be turned into an HTTP header for the request.
    #[error("MCP server `{server}` has a header that cannot be sent: {reason}")]
    BadHeader { server: String, reason: String },

    /// Building the process or the HTTP client failed before any bytes moved.
    #[error("cannot start a connection to MCP server `{server}`: {reason}")]
    Setup { server: String, reason: String },

    /// The connection or the initial handshake failed. Boxed: `ClientInitializeError`
    /// is large (it carries a full JSON-RPC error payload in one variant), and
    /// clippy's `result_large_err` is right that an error type this size should not
    /// be the `Err` of every fallible call in this module.
    #[error("cannot connect to MCP server `{server}`: {source}")]
    Connect {
        server: String,
        #[source]
        source: Box<rmcp::service::ClientInitializeError>,
    },

    /// A connected server's protocol call (list or call tool) failed.
    #[error("MCP server `{server}` failed: {source}")]
    Service {
        server: String,
        #[source]
        source: rmcp::ServiceError,
    },

    /// The server answered a 401 challenge: it requires OAuth. Complete the flow in
    /// [`crate::oauth`] and reconnect with
    /// [`RmcpCaller::connect_streamable_http_authorized`].
    #[error("MCP server `{server}` requires authorization: {challenge}")]
    AuthorizationRequired { server: String, challenge: String },

    /// A configured header's environment variable cannot be used. Raised before
    /// any request is sent. The message names the variable, never its value.
    #[error(
        "MCP server `{server}` header `{header}` reads environment variable `{variable}`, which {reason}"
    )]
    HeaderEnv {
        server: String,
        header: String,
        variable: String,
        reason: &'static str,
    },

    /// The server answered with an HTTP error status. `detail` is the server's
    /// text with configured credentials removed and its length capped.
    #[error("MCP server `{server}` answered HTTP {status}: {detail}")]
    HttpStatus {
        server: String,
        status: u16,
        detail: String,
    },

    /// The server answered 404 to a request that carried its `Mcp-Session-Id`:
    /// the session is gone. kGraf does not start a new session mid-run,
    /// because the server may have dropped state the run relied on; reconnect.
    #[error(
        "MCP server `{server}` ended the session (HTTP 404 on a request with its Mcp-Session-Id); \
         reconnect to start a new one"
    )]
    SessionExpired { server: String },

    /// The server negotiated a protocol version this client does not speak over
    /// Streamable HTTP ([`crate::SUPPORTED_HTTP_PROTOCOL_VERSIONS`]).
    #[error(
        "MCP server `{server}` negotiated protocol version `{offered}`; this client accepts {accepted}"
    )]
    ProtocolVersion {
        server: String,
        offered: String,
        accepted: String,
    },

    /// The server answered with a JSON-RPC error. Credentials removed.
    #[error("MCP server `{server}` returned an error: {detail}")]
    ServerError { server: String, detail: String },

    /// Any other Streamable HTTP failure (connection refused, a broken stream, an
    /// unexpected content type). Credentials removed.
    #[error("the HTTP transport to MCP server `{server}` failed: {detail}")]
    HttpTransport { server: String, detail: String },
}

/// One live connection to an MCP server, over whichever transport it configured.
pub struct RmcpCaller {
    server: String,
    timeout: Duration,
    peer: Peer,
    sink: Arc<dyn McpAttributionSink>,
    /// Knows this server's header values, to keep them out of error text. Empty
    /// for stdio servers and for OAuth-authorized HTTP servers.
    redactor: Redactor,
}

enum Peer {
    Stdio(RunningService<RoleClient, ()>),
    Http(RunningService<RoleClient, ClientConfig>),
}

impl RmcpCaller {
    /// Connect to a stdio server: spawn its command and speak MCP over its stdin
    /// and stdout. `sink` is required (see [`crate::attribution`]); every
    /// call this connection makes is recorded through it.
    pub async fn connect_stdio(
        config: &McpServerConfig,
        sink: Arc<dyn McpAttributionSink>,
    ) -> Result<Self, McpConnectError> {
        let McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } = config.transport()
        else {
            return Err(McpConnectError::WrongTransport {
                server: config.name().to_owned(),
                configured: "streamable_http",
                attempted: "stdio",
            });
        };
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        // Do not inherit this process's whole environment.
        cmd.env_clear();
        for key in INHERITED_ENV_ALLOWLIST {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        #[cfg(windows)]
        for key in INHERITED_ENV_ALLOWLIST_WINDOWS {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        cmd.envs(env);
        // A stdio server's `env` map (unlike an HTTP header's `HeaderFromEnv`)
        // is a literal value in the worker's own configuration, not read from
        // this process's environment at connect time, so a credential a stdio
        // server needs can sit right in `env`'s values (see this module's and
        // `kgraf-host::worker::assemble`'s docs on that). Registered with
        // kGraf's process-wide scrubber (decision 0071) so it is also removed
        // from every worker transcript and the shell, not only kept out of
        // this crate's own logging.
        //
        // **Not every value, only a credential-shaped one** (a security fix): registering *every* entry of length 8+ meant an ordinary
        // configuration value -- `NODE_ENV=production`, an inherited `HOME` or
        // `PATH` -- was treated as a secret and stripped everywhere it
        // happened to appear as a substring, which is a correctness bug, not
        // a safety margin. A value is offered only when its *key* looks like
        // it names a credential. The registry then refuses a placeholder
        // (`API_KEY=changeme`) or a repeat, and says so to the user; its
        // length-scaled floor accepts a real short key that this crate's
        // earlier flat 3.5-bit floor refused.
        for (key, value) in env {
            if is_credential_shaped_key(key) {
                kgraf_secrets::register_exact_secret("mcp-stdio-env", value);
            }
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }

        // Run the server in its own process group / Job
        // Object, so killing it (Drop, or `close`) reaps grandchildren too, and
        // pipe its stderr instead of the default `Stdio::inherit()`.
        let mut wrapped = process_wrap::tokio::CommandWrap::from(cmd);
        #[cfg(unix)]
        wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrapped.wrap(process_wrap::tokio::JobObject);

        let (child, stderr) = TokioChildProcess::builder(wrapped)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| McpConnectError::Setup {
                server: config.name().to_owned(),
                reason: e.to_string(),
            })?;
        if let Some(stderr) = stderr {
            spawn_bounded_stderr_reader(config.name().to_owned(), stderr);
        }

        let timeout = Duration::from_millis(config.timeout_ms());
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(timeout, ().serve(child)).await;
        let event_outcome = match &outcome {
            Ok(Ok(_)) => McpCallOutcome::Success,
            Ok(Err(_)) => McpCallOutcome::ProtocolError,
            Err(_) => McpCallOutcome::TimedOut,
        };
        sink.record(McpCallEvent {
            server: config.name().to_owned(),
            tool: None,
            worker: None,
            task: None,
            kind: McpCallKind::Connect,
            duration: started.elapsed(),
            outcome: event_outcome,
            tool_is_error: None,
            bytes_in: 0,
            bytes_out: 0,
        });
        let peer = outcome
            .map_err(|_| McpConnectError::Setup {
                server: config.name().to_owned(),
                reason: "timed out starting the server process".to_owned(),
            })?
            .map_err(|source| McpConnectError::Connect {
                server: config.name().to_owned(),
                source: Box::new(source),
            })?;
        Ok(Self {
            server: config.name().to_owned(),
            timeout,
            peer: Peer::Stdio(peer),
            sink,
            redactor: Redactor::default(),
        })
    }

    /// Connect to a Streamable HTTP server with the headers in its configuration
    /// (a static bearer token, an API key, ...), each read from this process's
    /// environment now. A 401 is [`McpConnectError::AuthorizationRequired`], and
    /// nothing more happens: the configured credential was refused, and starting
    /// an OAuth flow the user did not configure would be a guess. A
    /// server configured with `"oauth": true` is refused here; connect it with
    /// [`Self::connect_streamable_http_authorized`].
    pub async fn connect_streamable_http(
        config: &McpServerConfig,
        sink: Arc<dyn McpAttributionSink>,
    ) -> Result<Self, McpConnectError> {
        Self::connect_streamable_http_with_env(config, &|name| std::env::var(name), sink).await
    }

    /// [`Self::connect_streamable_http`] with header variables read from `env`
    /// instead of the process environment. Tests use it so that they need not
    /// mutate the environment of a multi-threaded process.
    pub async fn connect_streamable_http_with_env(
        config: &McpServerConfig,
        env: &(dyn Fn(&str) -> Result<String, std::env::VarError> + Sync),
        sink: Arc<dyn McpAttributionSink>,
    ) -> Result<Self, McpConnectError> {
        let McpTransport::StreamableHttp { headers, .. } = config.transport() else {
            return Err(wrong_transport_for_http(config));
        };
        if config.uses_oauth() {
            return Err(McpConnectError::Setup {
                server: config.name().to_owned(),
                reason: "the server is configured for OAuth; connect it with \
                         connect_streamable_http_authorized"
                    .to_owned(),
            });
        }
        // Resolved before any byte is sent: a missing variable is no egress.
        let resolved = resolve_headers(config.name(), headers, env)?;
        let http_client = BoundedClient::new(
            http_client_for(config, resolved.map)?,
            config.name(),
            sink.clone(),
            Duration::from_millis(config.timeout_ms()),
        );
        let redactor = resolved.redactor;
        let transport =
            build_transport(config, RedactingClient::new(http_client, redactor.clone()))?;
        Self::connect_http_transport(config, transport, redactor, sink).await
    }

    /// Connect to a server configured with `"oauth": true`, authorized by `oauth`
    /// (resumed from the keychain, or completed in a browser). The configured
    /// headers are sent too, read from this process's environment (they were
    /// dropped on this path before).
    pub async fn connect_streamable_http_authorized(
        config: &McpServerConfig,
        oauth: crate::oauth::OAuthConnector,
        sink: Arc<dyn McpAttributionSink>,
    ) -> Result<Self, McpConnectError> {
        Self::connect_streamable_http_authorized_with_env(
            config,
            oauth,
            &|name| std::env::var(name),
            sink,
        )
        .await
    }

    /// [`Self::connect_streamable_http_authorized`] with header variables read
    /// from `env`.
    pub async fn connect_streamable_http_authorized_with_env(
        config: &McpServerConfig,
        oauth: crate::oauth::OAuthConnector,
        env: &(dyn Fn(&str) -> Result<String, std::env::VarError> + Sync),
        sink: Arc<dyn McpAttributionSink>,
    ) -> Result<Self, McpConnectError> {
        let McpTransport::StreamableHttp { headers, .. } = config.transport() else {
            return Err(wrong_transport_for_http(config));
        };
        if !config.uses_oauth() {
            return Err(McpConnectError::Setup {
                server: config.name().to_owned(),
                reason: "the server is not configured for OAuth (\"oauth\": true)".to_owned(),
            });
        }
        let resolved = resolve_headers(config.name(), headers, env)?;
        let http_client = BoundedClient::new(
            http_client_for(config, resolved.map)?,
            config.name(),
            sink.clone(),
            Duration::from_millis(config.timeout_ms()),
        );
        // The OAuth token itself lives inside `AuthClient`; the redactor knows
        // the configured header secrets and removes `Bearer` credentials from
        // error text by pattern.
        let redactor = resolved.redactor;
        let transport = build_transport(
            config,
            RedactingClient::new(oauth.into_authorized_client(http_client), redactor.clone()),
        )?;
        Self::connect_http_transport(config, transport, redactor, sink).await
    }

    async fn connect_http_transport<C>(
        config: &McpServerConfig,
        transport: StreamableHttpClientTransport<C>,
        redactor: Redactor,
        sink: Arc<dyn McpAttributionSink>,
    ) -> Result<Self, McpConnectError>
    where
        C: rmcp::transport::streamable_http_client::StreamableHttpClient
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let timeout = Duration::from_millis(config.timeout_ms());
        let started = std::time::Instant::now();
        let connect = client_info().serve(transport);
        let outcome = match tokio::time::timeout(timeout, connect).await {
            Ok(outcome) => outcome,
            Err(_) => {
                // Egress that got no answer is still egress: attributed before
                // the error returns (decision 0051).
                sink.record(McpCallEvent {
                    server: config.name().to_owned(),
                    tool: None,
                    worker: None,
                    task: None,
                    kind: McpCallKind::Connect,
                    duration: started.elapsed(),
                    outcome: McpCallOutcome::TimedOut,
                    tool_is_error: None,
                    bytes_in: 0,
                    bytes_out: 0,
                });
                return Err(McpConnectError::Setup {
                    server: config.name().to_owned(),
                    reason: "timed out connecting".to_owned(),
                });
            }
        };
        let result = match outcome {
            Ok(peer) => match check_protocol_version(config.name(), &peer) {
                Ok(()) => Ok(peer),
                Err(error) => {
                    // Ends the session (DELETE) rather than leaving it open.
                    let _ = peer.cancel().await;
                    Err(error)
                }
            },
            Err(error) => Err(classify_init_error(config.name(), &error, &redactor)),
        };
        sink.record(McpCallEvent {
            server: config.name().to_owned(),
            tool: None,
            worker: None,
            task: None,
            kind: McpCallKind::Connect,
            duration: started.elapsed(),
            outcome: if result.is_ok() {
                McpCallOutcome::Success
            } else {
                McpCallOutcome::ProtocolError
            },
            tool_is_error: None,
            bytes_in: 0,
            bytes_out: 0,
        });
        Ok(Self {
            server: config.name().to_owned(),
            timeout,
            peer: Peer::Http(result?),
            sink,
            redactor,
        })
    }

    /// The protocol version the server negotiated at `initialize`, if connected.
    pub fn protocol_version(&self) -> Option<String> {
        let info = match &self.peer {
            Peer::Stdio(peer) => peer.peer_info(),
            Peer::Http(peer) => peer.peer_info(),
        };
        info.map(|info| info.protocol_version.as_str().to_owned())
    }

    /// A failed request, typed: for an HTTP peer it is classified (status,
    /// session expiry, server error) with credentials removed; a stdio peer keeps
    /// `rmcp`'s error as the source.
    fn service_error(&self, source: rmcp::ServiceError) -> McpConnectError {
        match self.peer {
            Peer::Stdio(_) => McpConnectError::Service {
                server: self.server.clone(),
                source,
            },
            Peer::Http(_) => classify_service_error(&self.server, &source, &self.redactor),
        }
    }

    /// List the server's tools, converted for [`crate::replace_server_tools`].
    pub async fn discover_tools(&self) -> Result<Vec<DiscoveredTool>, McpConnectError> {
        let started = std::time::Instant::now();
        let outcome = match &self.peer {
            Peer::Stdio(peer) => tokio::time::timeout(self.timeout, peer.list_all_tools()).await,
            Peer::Http(peer) => tokio::time::timeout(self.timeout, peer.list_all_tools()).await,
        };
        let (event_outcome, result) = match outcome {
            Ok(Ok(tools)) => (McpCallOutcome::Success, Ok(tools)),
            Ok(Err(source)) => (
                McpCallOutcome::ProtocolError,
                Err(self.service_error(source)),
            ),
            Err(_) => (
                McpCallOutcome::TimedOut,
                Err(McpConnectError::Setup {
                    server: self.server.clone(),
                    reason: "timed out listing tools".to_owned(),
                }),
            ),
        };
        self.sink.record(McpCallEvent {
            server: self.server.clone(),
            tool: None,
            worker: None,
            task: None,
            kind: McpCallKind::ListTools,
            duration: started.elapsed(),
            outcome: event_outcome,
            tool_is_error: None,
            bytes_in: 0,
            bytes_out: 0,
        });
        let tools = result?;
        Ok(tools
            .into_iter()
            .map(|tool| DiscoveredTool {
                name: tool.name.into_owned(),
                description: tool.description.map(|d| UntrustedText::new(d.into_owned())),
                input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
            })
            .collect())
    }

    /// Close the connection. Best-effort: a server that is already gone is not an
    /// error to shut down. (The two arms cannot share one `match` expression: each
    /// `RunningService`'s `cancel()` returns its own return-position-`impl Trait`
    /// future, which are distinct opaque types even with identical bounds.) For a
    /// stdio server this also tears down its process group (see the module docs).
    pub async fn close(self) {
        match self.peer {
            Peer::Stdio(peer) => {
                let _ = peer.cancel().await;
            }
            Peer::Http(peer) => {
                let _ = peer.cancel().await;
            }
        }
    }
}

/// Words (case-insensitive, whole underscore-separated words, not substrings)
/// that mark an environment variable's *name* as naming a credential. Same shape as `config::CREDENTIAL_WORDS` (this module's HTTP-header
/// counterpart), which matches as whole words rather than substrings for
/// exactly the same reason: an env var like `AUTHOR_NAME` or `PASS_THROUGH`
/// should not be treated as a credential just because it contains `AUTH` or
/// `PASS` as a substring. Kept as its own list because a stdio `env` map's
/// keys follow shell `SCREAMING_SNAKE_CASE` convention, split on `_`, rather
/// than a header's `Kebab-Case` or `camelCase`, split on any non-alphanumeric.
const CREDENTIAL_SHAPED_ENV_KEY_WORDS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASS",
    "AUTH",
    "CREDENTIAL",
];

/// Last words that make a key name a setting, not a secret: `AUTH_PROVIDER`,
/// `TOKEN_ENDPOINT`, `KEY_PATH`. Their values are ordinary
/// text, and registered they would be redacted from model input too.
const SETTING_ENV_KEY_LAST_WORDS: &[&str] = &[
    "URL", "URI", "PATH", "FILE", "DIR", "HOST", "ENDPOINT", "REGION", "TYPE", "MODE", "PROVIDER",
    "ISSUER", "SCOPE", "SCOPES",
];

/// Whether `key` (an env var name) looks like it names a credential: one of
/// [`CREDENTIAL_SHAPED_ENV_KEY_WORDS`] appears as a whole `_`-separated word,
/// and its last word is not one of [`SETTING_ENV_KEY_LAST_WORDS`].
fn is_credential_shaped_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    let last = upper.rsplit('_').next().unwrap_or("");
    !SETTING_ENV_KEY_LAST_WORDS.contains(&last)
        && upper
            .split('_')
            .any(|word| CREDENTIAL_SHAPED_ENV_KEY_WORDS.contains(&word))
}

/// Read a stdio server's stderr in the background, bounded, through `tracing`
/// rather than raw to this process's own terminal. Keeps
/// draining past the logged-line cap so a chatty server cannot block on a full
/// pipe and stall the connection.
fn spawn_bounded_stderr_reader(server: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut logged = 0usize;
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if logged < MAX_STDERR_LINES_LOGGED {
                        let (capped, truncated) = stderr_log_line(&line);
                        tracing::warn!(
                            target: "kgraf_mcp::stdio_stderr",
                            server = %server,
                            truncated,
                            "{capped}"
                        );
                        logged += 1;
                        if logged == MAX_STDERR_LINES_LOGGED {
                            tracing::warn!(
                                target: "kgraf_mcp::stdio_stderr",
                                server = %server,
                                "further stderr lines are being discarded, not logged"
                            );
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
}

/// One stderr line as it is logged: persist-scrubbed whole, then capped
/// (decision 0071). A server can print its own credential or
/// a token it was handed, and tracing output lands in terminals and log
/// files. Scrubbed before the cap, so the cap cannot cut a secret in two
/// where the scrub would miss both halves. Returns whether it was cut.
fn stderr_log_line(line: &str) -> (String, bool) {
    let scrubbed = kgraf_secrets::scrub_persist(line);
    let capped: String = scrubbed.chars().take(MAX_STDERR_LINE_CHARS).collect();
    let truncated = capped.len() < scrubbed.len();
    (capped, truncated)
}

#[async_trait::async_trait]
impl McpCaller for RmcpCaller {
    async fn call_tool(
        &self,
        tool: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let bytes_in = estimate_json_bytes(&arguments);
        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map as JsonObject),
            serde_json::Value::Null => None,
            other => {
                return Err(ToolError::InvalidInput {
                    tool: tool.to_owned(),
                    reason: format!(
                        "MCP tool arguments must be a JSON object, got {}",
                        describe_json_kind(&other)
                    ),
                });
            }
        };
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }

        let started = std::time::Instant::now();

        // A cancelled worker's in-flight MCP call must stop
        // immediately rather than run to completion or to its timeout. Both peer
        // variants' `call_tool` are boxed into one future type here because they
        // are otherwise distinct, anonymous `impl Future`s (one per generic
        // instantiation), which `tokio::select!`'s two branches cannot unify.
        let call_future: Pin<
            Box<
                dyn Future<Output = Result<rmcp::model::CallToolResult, rmcp::ServiceError>>
                    + Send
                    + '_,
            >,
        > = match &self.peer {
            Peer::Stdio(peer) => Box::pin(peer.call_tool(params)),
            Peer::Http(peer) => Box::pin(peer.call_tool(params)),
        };

        let outcome: Result<Result<rmcp::model::CallToolResult, rmcp::ServiceError>, CallEnded> = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Err(CallEnded::Cancelled),
            result = tokio::time::timeout(self.timeout, call_future) => {
                result.map_err(|_| CallEnded::TimedOut)
            }
        };

        let elapsed = started.elapsed();
        let (event_outcome, tool_is_error, output) = match outcome {
            Err(CallEnded::Cancelled) => {
                self.record(
                    tool,
                    ctx,
                    elapsed,
                    McpCallOutcome::Cancelled,
                    None,
                    bytes_in,
                    0,
                );
                return Err(ToolError::Cancelled {
                    tool: tool.to_owned(),
                });
            }
            Err(CallEnded::TimedOut) => {
                self.record(
                    tool,
                    ctx,
                    elapsed,
                    McpCallOutcome::TimedOut,
                    None,
                    bytes_in,
                    0,
                );
                return Err(ToolError::Timeout {
                    tool: tool.to_owned(),
                    after_ms: self.timeout.as_millis() as u64,
                });
            }
            Ok(Err(source)) => {
                self.record(
                    tool,
                    ctx,
                    elapsed,
                    McpCallOutcome::ProtocolError,
                    None,
                    bytes_in,
                    0,
                );
                // The reason reaches the model and the transcript: for an HTTP
                // peer it is the typed, credential-free classification.
                let reason = match self.peer {
                    Peer::Stdio(_) => {
                        format!("MCP server `{}` returned an error: {source}", self.server)
                    }
                    Peer::Http(_) => self.service_error(source).to_string(),
                };
                return Err(ToolError::Internal {
                    tool: tool.to_owned(),
                    reason,
                });
            }
            Ok(Ok(result)) => {
                let bytes_out = estimate_call_tool_result_bytes(&result);
                let output = parse_call_tool_result(result);
                (
                    McpCallOutcome::Success,
                    Some(output.is_error),
                    (output, bytes_out),
                )
            }
        };
        let (output, bytes_out) = output;
        self.record(
            tool,
            ctx,
            elapsed,
            event_outcome,
            tool_is_error,
            bytes_in,
            bytes_out,
        );
        Ok(output)
    }
}

/// Why an in-flight `call_tool` stopped without a normal protocol result.
enum CallEnded {
    Cancelled,
    TimedOut,
}

impl RmcpCaller {
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        tool: &str,
        ctx: &ToolContext,
        duration: Duration,
        outcome: McpCallOutcome,
        tool_is_error: Option<bool>,
        bytes_in: u64,
        bytes_out: u64,
    ) {
        self.sink.record(McpCallEvent {
            server: self.server.clone(),
            tool: Some(tool.to_owned()),
            worker: Some(ctx.worker.clone()),
            task: Some(ctx.task.clone()),
            kind: McpCallKind::CallTool,
            duration,
            outcome,
            tool_is_error,
            bytes_in,
            bytes_out,
        });
    }
}

fn estimate_json_bytes(value: &serde_json::Value) -> u64 {
    serde_json::to_vec(value)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn estimate_call_tool_result_bytes(result: &rmcp::model::CallToolResult) -> u64 {
    serde_json::to_vec(result)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn describe_json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Object(_) => "an object",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Null => "null",
    }
}

fn wrong_transport_for_http(config: &McpServerConfig) -> McpConnectError {
    McpConnectError::WrongTransport {
        server: config.name().to_owned(),
        configured: "stdio",
        attempted: "streamable_http",
    }
}

/// Refuse a server whose negotiated version is not one this client speaks over
/// Streamable HTTP. `rmcp`'s `initialize` records whatever the server answers and
/// does not check it, so without this a 2024-11-05 server (the older HTTP+SSE
/// transport) would connect and fail later in ways that do not name the cause.
fn check_protocol_version(
    server: &str,
    peer: &RunningService<RoleClient, ClientConfig>,
) -> Result<(), McpConnectError> {
    let offered = peer
        .peer_info()
        .map(|info| info.protocol_version.as_str().to_owned())
        .unwrap_or_else(|| "<none>".to_owned());
    if SUPPORTED_HTTP_PROTOCOL_VERSIONS.contains(&offered.as_str()) {
        Ok(())
    } else {
        Err(McpConnectError::ProtocolVersion {
            server: server.to_owned(),
            offered,
            accepted: SUPPORTED_HTTP_PROTOCOL_VERSIONS.join(", "),
        })
    }
}

fn build_transport<C>(
    config: &McpServerConfig,
    http_client: C,
) -> Result<StreamableHttpClientTransport<C>, McpConnectError>
where
    C: rmcp::transport::streamable_http_client::StreamableHttpClient
        + Clone
        + Send
        + Sync
        + 'static,
{
    Ok(StreamableHttpClientTransport::with_client(
        http_client,
        transport_config(config)?,
    ))
}

/// The transport settings for one server. Split from [`build_transport`] so the
/// settings themselves are testable.
fn transport_config(
    config: &McpServerConfig,
) -> Result<StreamableHttpClientTransportConfig, McpConnectError> {
    let McpTransport::StreamableHttp { url, .. } = config.transport() else {
        return Err(wrong_transport_for_http(config));
    };
    let timeout = Duration::from_millis(config.timeout_ms());
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
    transport_config.control_request_timeout = timeout;
    transport_config.session_recovery_timeout = timeout;
    // Fail loud (fail loud): `rmcp` defaults to re-running `initialize` on a 404 and
    // retrying the call on a new session. A new session can silently lose server
    // state the run relied on, so a 404 surfaces as `SessionExpired` instead.
    transport_config.reinit_on_expired_session = false;
    // The per-event SSE cap. It bounds SSE only: JSON replies and error bodies
    // are bounded by `BoundedClient` itself, because `rmcp`'s transport has no
    // setting for them.
    transport_config.max_sse_event_size = MAX_HTTP_BODY_BYTES;
    // A bounded retry for a failed GET stream; `rmcp`'s default
    // retries forever. A stream that ends cleanly is reopened no faster than
    // `BoundedClient`'s GET pacing allows, whatever `retry:` the server sends.
    let mut retry = ExponentialBackoff::default();
    retry.max_times = Some(GET_STREAM_MAX_RETRIES);
    retry.base_duration = MIN_GET_STREAM_INTERVAL;
    retry.max_delay = Some(GET_STREAM_MAX_RETRY_DELAY);
    transport_config.retry_config = Arc::new(retry);
    Ok(transport_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// an ordinary configuration value must not be
    /// registered as a secret just because it is 8+ characters, only a
    /// credential-shaped key with a high-entropy value.
    #[test]
    fn ordinary_environment_values_are_not_credential_shaped() {
        assert!(!is_credential_shaped_key("HOME"));
        assert!(!is_credential_shaped_key("PATH"));
        assert!(!is_credential_shaped_key("NODE_ENV"));
        // A whole-word match, not a substring one: neither name contains
        // `AUTH` or `PASS` as a whole `_`-separated word.
        assert!(!is_credential_shaped_key("AUTHOR_NAME"));
        assert!(!is_credential_shaped_key("COMPASSIONATE_MODE"));
    }

    #[test]
    fn credential_shaped_keys_are_recognized() {
        for key in [
            "API_KEY",
            "DB_PASSWORD",
            "AUTH_TOKEN",
            "MY_SECRET",
            "CLIENT_CREDENTIAL",
        ] {
            assert!(
                is_credential_shaped_key(key),
                "{key} should be credential-shaped"
            );
        }
    }

    /// A server's stderr is scrubbed before it is traced, and before it
    /// is capped.
    #[test]
    fn a_stderr_line_is_scrubbed_before_it_is_logged() {
        let token = format!("ghp_{}", "Rq5Ts7Uv9Wx1Yz3Ab5Cd7Ef9Gh1Ij3Kl5Mn7O");
        let (logged, truncated) = stderr_log_line(&format!("auth failed for {token}"));
        assert!(!logged.contains(&token), "{logged}");
        assert!(logged.contains("[redacted:github-pat;"), "{logged}");
        assert!(!truncated);
        let long = format!("{}{token}", "x".repeat(MAX_STDERR_LINE_CHARS - 10));
        let (logged, truncated) = stderr_log_line(&long);
        assert!(truncated);
        assert!(
            !logged.contains(&token[..10]),
            "no half of it survives the cap"
        );
    }

    /// A key that names a place, a kind or a party is not a
    /// credential, whatever other word it holds.
    #[test]
    fn a_key_naming_a_setting_is_not_credential_shaped() {
        for key in [
            "AUTH_PROVIDER",
            "AUTH_URL",
            "TOKEN_ENDPOINT",
            "AUTH_ISSUER",
            "TOKEN_SCOPES",
            "KEY_FILE",
            "KEY_PATH",
            "AUTH_REGION",
            "AUTH_TYPE",
            "AUTH_MODE",
            "AUTH_HOST",
            "KEY_DIR",
            "TOKEN_URI",
            "AUTH_SCOPE",
        ] {
            assert!(!is_credential_shaped_key(key), "{key}");
        }
        for key in [
            "API_KEY",
            "AUTH_TOKEN",
            "GITHUB_PERSONAL_ACCESS_TOKEN",
            "DB_PASSWORD",
        ] {
            assert!(is_credential_shaped_key(key), "{key}");
        }
    }

    #[test]
    fn a_low_entropy_placeholder_value_is_not_secret_shaped() {
        use kgraf_secrets::{Registration, Scrubber};
        let s = Scrubber::new();
        assert_eq!(
            s.register_exact_secret("mcp-stdio-env", "changeme"),
            Registration::Placeholder
        );
        assert_eq!(
            s.register_exact_secret("mcp-stdio-env", "your-api-key-here"),
            Registration::Placeholder
        );
    }

    #[test]
    fn a_real_looking_token_value_is_secret_shaped() {
        use kgraf_secrets::{Registration, Scrubber};
        let s = Scrubber::new();
        for key in ["aK9x2Qm7Lp4Rt8Zv1Ws5Yb3Nc6Hd0Fj", "9f86d081884c7d65"] {
            assert_eq!(
                s.register_exact_secret("mcp-stdio-env", key),
                Registration::Registered
            );
        }
    }

    /// The transport's own settings, checked as the values `rmcp`
    /// reads. Each assertion fails if its setting is removed: `rmcp`'s defaults
    /// retry a failed GET stream forever with unbounded delays, re-initialize on
    /// a 404, and cap SSE events at its own default.
    #[test]
    fn the_transport_settings_bound_retries_and_fail_loud() {
        let config = McpServerConfig::new(
            "s",
            McpTransport::StreamableHttp {
                url: "https://a.example/mcp".into(),
                headers: Default::default(),
                proxy: None,
                oauth: false,
            },
        )
        .expect("valid");
        let settings = transport_config(&config).expect("http");
        let retry = &settings.retry_config;
        assert!(retry.retry(GET_STREAM_MAX_RETRIES - 1).is_some());
        assert_eq!(
            retry.retry(GET_STREAM_MAX_RETRIES),
            None,
            "a failed GET stream stops after the retry limit"
        );
        assert!(
            (0..GET_STREAM_MAX_RETRIES)
                .filter_map(|n| retry.retry(n))
                .all(|delay| delay <= GET_STREAM_MAX_RETRY_DELAY),
            "no single wait exceeds the maximum delay"
        );
        assert_eq!(retry.retry(0), Some(MIN_GET_STREAM_INTERVAL));
        assert!(!settings.reinit_on_expired_session);
        assert_eq!(settings.max_sse_event_size, MAX_HTTP_BODY_BYTES);
    }

    /// A Streamable HTTP connect that timed out returned
    /// before its attribution was recorded, so egress to a server that never
    /// answered left no record. A loopback listener that accepts and never
    /// replies makes the connect time out; the sink must hold one `Connect`
    /// event with outcome `TimedOut`.
    #[tokio::test]
    async fn a_connect_that_times_out_is_still_attributed() {
        use crate::attribution::InMemoryAttributionSink;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hold = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let config = McpServerConfig::new(
            "silent",
            McpTransport::StreamableHttp {
                url: format!("http://{addr}/mcp"),
                proxy: None,
                oauth: false,
                headers: Default::default(),
            },
        )
        .expect("valid")
        .with_timeout_ms(300)
        .expect("timeout");
        let sink = Arc::new(InMemoryAttributionSink::new());
        let result = RmcpCaller::connect_streamable_http(&config, sink.clone()).await;
        assert!(result.is_err(), "the silent server cannot connect");
        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, McpCallKind::Connect);
        assert_eq!(events[0].outcome, McpCallOutcome::TimedOut);
        hold.abort();
    }

    /// Tested as the mechanism (test the mechanism, not the setting): a redirect response
    /// must come back to the caller as-is, not be followed. This runs a real,
    /// minimal HTTP/1.1 server on a loopback socket that answers every request with
    /// a 307 redirect to a second, different response; if the client followed it,
    /// the body received would be the second response's, not the first's status.
    #[tokio::test]
    async fn the_shared_http_client_builder_never_follows_a_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let body = "moved";
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{addr}/elsewhere\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let client = base_http_client_builder(Duration::from_secs(5))
            .build()
            .expect("client");
        let response = client
            .get(format!("http://{addr}/start"))
            .send()
            .await
            .expect("request completes locally");
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.text().await.expect("body"),
            "moved",
            "the redirect target must never have been fetched"
        );
    }
}
