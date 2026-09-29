//! A minimal, real MCP server over stdio, for `kgraf-mcp`'s
//! `tests/stdio_live.rs` (the real-speaker rule: the adapter under test must exchange a request
//! and a response with a real speaker, not a mock). Built with `rmcp`'s own
//! `server` feature directly, not the `#[tool_router]` macros, to keep this small
//! and its behavior obvious from reading it.
//!
//! **Why its own crate, not a `[[bin]]` in `kgraf-mcp` (release-blocker fix).**
//! A `[[bin]]` on `kgraf-mcp` makes `dist` package an archive for it on every
//! release target, alongside the real product binary (`kgraf-cli`); `dist plan`
//! confirmed this once the bin existed. This crate is `publish = false` and
//! `[package.metadata.dist] dist = false`, so it is invisible to both `cargo
//! publish` and `dist`, and is reached only as a `kgraf-mcp` dev-dependency
//! (which puts its binary path in `CARGO_BIN_EXE_mcp-echo-server` for
//! `kgraf-mcp`'s tests and benches; see Cargo's own docs for that variable).
//!
//! Five tools exercise the shapes `kgraf_mcp::parse_call_tool_result` and the
//! stdio child-process seam have to handle:
//! - `echo`: text in, text out.
//! - `tiny_png`: returns an `image` content block, to prove binary is summarized
//!   and never forwarded as base64.
//! - `boom`: returns a tool-level error (`CallToolResult::error`), to prove a
//!   failed call still reaches `RmcpCaller::call_tool` as `Ok(ToolOutput{is_error:
//!   true, ..})`, not an `Err`.
//! - `getenv`: returns one environment variable's value from this process, or the
//!   literal string `<absent>`, so a test can prove the client's environment
//!   allowlist actually reached the child rather than merely being configured.
//! - `sleep_ms`: sleeps for the given duration before answering, so a test can
//!   prove that cancelling a worker's `ToolContext` stops an in-flight MCP call
//!   before the sleep would otherwise return.
//!
//! Two more tools matter only over HTTP:
//! - `http_header`: returns one header of the HTTP request that carried the call,
//!   or `<absent>`, so a test can prove a configured header reached the server.
//! - `notify_then_echo`: sends a `tools/list_changed` notification before its
//!   result, so the reply is a multi-event `text/event-stream` even in JSON mode.
//!   (Not a logging notification: logging is deprecated by SEP-2577.)
//!
//! **Streamable HTTP mode** (`--http`), for `tests/http_live.rs`. The server binds
//! `127.0.0.1:0`, prints `LISTENING <url>` as its first stdout line, and exits when
//! its stdin closes. It serves MCP at `/mcp` through `rmcp`'s own
//! `StreamableHttpService`, with these options:
//! - default: sessions (`Mcp-Session-Id`), SSE replies to POST, a GET stream.
//! - `--json`: stateless, `application/json` replies.
//! - `--require-header NAME=VALUE`: any `/mcp` request without that exact header
//!   gets a 401 whose body echoes the value it did receive, the way a careless
//!   real server might. That is the case the client's redaction exists for.
//! - `--protocol-version V`: answer `initialize` with `V`, whatever was negotiated.
//!
//! Two admin paths let a test observe and steer the server: `GET /admin/stats`
//! (live sessions, request counts by method and reply content type, as JSON) and `POST
//! /admin/expire-sessions` (closes every session, so the next request that
//! carries a session id gets a 404).

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeRequestParams, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, transport::stdio};
use serde_json::json;
use std::convert::Infallible;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;

#[derive(Clone, Default)]
struct EchoServer {
    /// `--protocol-version`: the version `initialize` answers with, if forced.
    forced_version: Option<ProtocolVersion>,
}

impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("kgraf-mcp-echo-server", "0.1.0"));
        // The SDK falls back to this version when the client's is unsupported.
        if let Some(version) = &self.forced_version {
            info.protocol_version = version.clone();
        }
        info
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::InitializeResult, McpError> {
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    /// With `--protocol-version`, the only version this server supports, so the
    /// SDK's own negotiation answers `initialize` with it, as a real server that
    /// speaks only that revision would. (Setting the result's version in
    /// `initialize` is not enough: the SDK negotiates again after the handler.)
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        match &self.forced_version {
            Some(version) => std::borrow::Cow::Owned(vec![version.clone()]),
            None => std::borrow::Cow::Borrowed(ProtocolVersion::KNOWN_VERSIONS),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let object_schema =
            |value: serde_json::Value| value.as_object().expect("object literal").clone();
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "echo",
                "Return the `message` argument unchanged",
                object_schema(json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                })),
            ),
            Tool::new(
                "tiny_png",
                "Return a one-pixel PNG as an image content block",
                object_schema(json!({"type": "object", "properties": {}})),
            ),
            Tool::new(
                "boom",
                "Always returns a tool-level error result",
                object_schema(json!({"type": "object", "properties": {}})),
            ),
            Tool::new(
                "getenv",
                "Return the named environment variable from this process, or <absent>",
                object_schema(json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                })),
            ),
            Tool::new(
                "sleep_ms",
                "Sleep for the given number of milliseconds, then answer",
                object_schema(json!({
                    "type": "object",
                    "properties": {"duration_ms": {"type": "number"}},
                    "required": ["duration_ms"]
                })),
            ),
            Tool::new(
                "http_header",
                "Return the named header of the HTTP request that carried this call, or <absent>",
                object_schema(json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                })),
            ),
            Tool::new(
                "notify_then_echo",
                "Send a tools/list_changed notification, then return the `message` argument",
                object_schema(json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                })),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let string_arg = |key: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|a| a.get(key))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let result = match request.name.as_ref() {
            "echo" => {
                let message = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                CallToolResult::success(vec![ContentBlock::text(message.to_owned())])
            }
            "tiny_png" => {
                // The smallest possible valid PNG signature plus an IHDR chunk tag;
                // real bytes, not a placeholder, so the NUL-bearing header is real.
                const PNG_HEAD: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
                let data = base64_encode(PNG_HEAD);
                CallToolResult::success(vec![ContentBlock::image(data, "image/png")])
            }
            "boom" => CallToolResult::error(vec![ContentBlock::text("the tool refused")]),
            "getenv" => {
                let name = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let value = std::env::var(name).unwrap_or_else(|_| "<absent>".to_owned());
                CallToolResult::success(vec![ContentBlock::text(value)])
            }
            "sleep_ms" => {
                let duration_ms = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("duration_ms"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(duration_ms)).await;
                CallToolResult::success(vec![ContentBlock::text("awake")])
            }
            "http_header" => {
                let name = string_arg("name");
                let value = context
                    .extensions
                    .get::<hyper::http::request::Parts>()
                    .and_then(|parts| parts.headers.get(name.as_str()))
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("<absent>")
                    .to_owned();
                CallToolResult::success(vec![ContentBlock::text(value)])
            }
            "notify_then_echo" => {
                context
                    .peer
                    .notify_tool_list_changed()
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                CallToolResult::success(vec![ContentBlock::text(string_arg("message"))])
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown tool `{other}`"),
                    None,
                ));
            }
        };
        Ok(result.into())
    }
}

/// A tiny, dependency-free base64 encoder: this binary otherwise has no need of
/// the `base64` crate, and duplicating the three lines is cheaper than a
/// dependency for one constant byte string.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `--http` options.
struct HttpOptions {
    json: bool,
    required_header: Option<(String, String)>,
    forced_version: Option<ProtocolVersion>,
}

fn parse_http_options(args: &[String]) -> anyhow::Result<HttpOptions> {
    let mut options = HttpOptions {
        json: false,
        required_header: None,
        forced_version: None,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--http" => {}
            "--json" => options.json = true,
            "--require-header" => {
                let spec = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--require-header needs NAME=VALUE"))?;
                let (name, value) = spec
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--require-header needs NAME=VALUE"))?;
                options.required_header = Some((name.to_ascii_lowercase(), value.to_owned()));
            }
            "--protocol-version" => {
                let version = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--protocol-version needs a value"))?;
                options.forced_version = Some(serde_json::from_value(json!(version))?);
            }
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }
    Ok(options)
}

/// Request counts, for `GET /admin/stats`.
#[derive(Default)]
struct Stats {
    post_sse: AtomicUsize,
    post_json: AtomicUsize,
    post_other: AtomicUsize,
    get_sse: AtomicUsize,
    delete: AtomicUsize,
    rejected: AtomicUsize,
}

struct App {
    mcp: StreamableHttpService<EchoServer, LocalSessionManager>,
    sessions: Arc<LocalSessionManager>,
    required_header: Option<(String, String)>,
    stats: Stats,
}

type Body = BoxBody<Bytes, Infallible>;

fn text_response(status: StatusCode, content_type: &str, body: String) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(body)).boxed());
    *response.status_mut() = status;
    if let Ok(value) = content_type.parse() {
        response
            .headers_mut()
            .insert(hyper::header::CONTENT_TYPE, value);
    }
    response
}

async fn route(request: Request<Incoming>, app: Arc<App>) -> Result<Response<Body>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = match (&method, path.as_str()) {
        (&Method::GET, "/admin/stats") => {
            let s = &app.stats;
            let sessions = app.sessions.sessions.read().await.len();
            let body = json!({
                "sessions": sessions,
                "post_sse": s.post_sse.load(Ordering::SeqCst),
                "post_json": s.post_json.load(Ordering::SeqCst),
                "post_other": s.post_other.load(Ordering::SeqCst),
                "get_sse": s.get_sse.load(Ordering::SeqCst),
                "delete": s.delete.load(Ordering::SeqCst),
                "rejected": s.rejected.load(Ordering::SeqCst),
            });
            text_response(StatusCode::OK, "application/json", body.to_string())
        }
        (&Method::POST, "/admin/expire-sessions") => {
            let ids: Vec<_> = app.sessions.sessions.read().await.keys().cloned().collect();
            for id in &ids {
                let _ = app.sessions.close_session(id).await;
            }
            let body = json!({"expired": ids.len()});
            text_response(StatusCode::OK, "application/json", body.to_string())
        }
        (_, "/mcp") => {
            if let Some((name, expected)) = &app.required_header {
                let received = request
                    .headers()
                    .get(name.as_str())
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                if &received != expected {
                    app.stats.rejected.fetch_add(1, Ordering::SeqCst);
                    // Deliberately careless: echo what was received, as some real
                    // servers do. No WWW-Authenticate, so this is not an OAuth
                    // challenge, just an error body carrying the credential.
                    return Ok(text_response(
                        StatusCode::UNAUTHORIZED,
                        "text/plain",
                        format!("rejected credentials: {received}"),
                    ));
                }
            }
            let response = app.mcp.handle(request).await;
            let is_sse = response
                .headers()
                .get(hyper::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.starts_with("text/event-stream"));
            let is_json = response
                .headers()
                .get(hyper::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.starts_with("application/json"));
            let counter = match method {
                Method::POST if is_sse => &app.stats.post_sse,
                Method::POST if is_json => &app.stats.post_json,
                Method::POST => &app.stats.post_other,
                Method::GET if is_sse => &app.stats.get_sse,
                Method::DELETE => &app.stats.delete,
                _ => &app.stats.post_other,
            };
            counter.fetch_add(1, Ordering::SeqCst);
            response
        }
        _ => text_response(StatusCode::NOT_FOUND, "text/plain", "no such path".into()),
    };
    Ok(response)
}

async fn serve_http(options: HttpOptions) -> anyhow::Result<()> {
    let sessions = Arc::new(LocalSessionManager::default());
    let mut config = StreamableHttpServerConfig::default();
    if options.json {
        config.legacy_session_mode = false;
        config.json_response = true;
    }
    let handler = EchoServer {
        forced_version: options.forced_version,
    };
    let mcp = StreamableHttpService::new(move || Ok(handler.clone()), sessions.clone(), config);
    let app = Arc::new(App {
        mcp,
        sessions,
        required_header: options.required_header,
        stats: Stats::default(),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "LISTENING http://{address}/mcp")?;
        stdout.flush()?;
    }
    // Exit when the parent closes stdin, so a server never outlives its test.
    tokio::spawn(async {
        let mut stdin = tokio::io::stdin();
        let mut buffer = [0u8; 64];
        while let Ok(n) = stdin.read(&mut buffer).await {
            if n == 0 {
                break;
            }
        }
        std::process::exit(0);
    });
    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| route(request, app.clone()));
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--http") {
        return serve_http(parse_http_options(&args)?).await;
    }
    let service = EchoServer::default().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
