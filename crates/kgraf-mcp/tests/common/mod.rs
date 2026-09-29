//! A raw HTTP/1.1 fake MCP server for hostile-server tests: replies that `rmcp`'s
//! own server would never send (JSON-escaped credentials, oversized bodies, a
//! `retry: 0` GET stream). Ported from hostile-server probes.
//!
//! It answers `initialize` and `tools/list` itself (one tool, `leak`) and every
//! other request with the configured [`Reply`]. It records each request and
//! counts the body bytes it managed to write, so a test can tell whether the
//! client read a body to the end or stopped early.

#![allow(dead_code)]

use kgraf_mcp::{HeaderFromEnv, McpServerConfig, McpTransport, RmcpCaller, replace_server_tools};
use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
use kgraf_tools::{
    CancellationToken, Exposure, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolView,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the fake sends for a `tools/call`.
#[derive(Clone, Copy)]
pub enum Reply {
    /// `application/json`, `text` spliced into a JSON string as written (so the
    /// caller controls its escaping).
    Json(&'static str),
    /// `text/event-stream`, one event, `text` spliced in as written.
    Sse(&'static str),
    /// A 200 JSON reply of about this many bytes, with `Content-Length`.
    BigJson(usize),
    /// A 200 JSON reply of about this many bytes, no `Content-Length`.
    BigJsonUnsized(usize),
    /// A 500 error body of this many bytes, no `Content-Length`.
    BigError(usize),
    /// `text/event-stream`: one small event with an id, then one event of this
    /// many bytes that never ends.
    OversizedSse(usize),
}

#[derive(Clone, Copy)]
pub struct Options {
    pub reply: Reply,
    /// Answer GET with an SSE stream that says `retry: 0` and closes at once.
    pub hammer_get: bool,
    /// Answer every POST with 401, this `WWW-Authenticate` value, and a body
    /// that echoes the request's `Authorization` header.
    pub unauthorized: Option<&'static str>,
    /// Answer GET with one SSE event of 17 MiB, over the client's cap.
    pub huge_get_event: bool,
}

impl Options {
    pub fn reply(reply: Reply) -> Self {
        Self {
            reply,
            hammer_get: false,
            unauthorized: None,
            huge_get_event: false,
        }
    }
}

pub struct Fake {
    pub url: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    /// Body bytes of big replies that were written before the client hung up.
    pub big_body_bytes_sent: Arc<AtomicUsize>,
}

impl Fake {
    pub fn count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .expect("lock")
            .iter()
            .filter(|r| r.starts_with(method))
            .count()
    }
}

pub async fn fake(options: Options) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sent = Arc::new(AtomicUsize::new(0));
    let (rec, sent_in) = (requests.clone(), sent.clone());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, options, rec.clone(), sent_in.clone()));
        }
    });
    Fake {
        url: format!("http://{addr}/mcp"),
        requests,
        big_body_bytes_sent: sent,
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    options: Options,
    rec: Arc<Mutex<Vec<String>>>,
    sent: Arc<AtomicUsize>,
) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        let n = socket.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while buf.len() < head_end + content_length {
        let n = socket.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
    rec.lock().expect("lock").push(format!("{head}{body}"));
    let first = head.lines().next().unwrap_or("").to_owned();

    if first.starts_with("GET") && options.huge_get_event {
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nid: 1\ndata: ";
        write_big(&mut socket, head, 17 * 1024 * 1024, "\n\n", &sent).await;
        let _ = socket.shutdown().await;
        return;
    }
    if first.starts_with("GET") {
        let response = if options.hammer_get {
            let body = "retry: 0\n\n";
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        } else {
            "HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_owned()
        };
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        return;
    }
    if let Some(challenge) = options.unauthorized
        && first.starts_with("POST")
    {
        let echoed = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .unwrap_or("")
            .to_owned();
        let body = format!("refused: {echoed}");
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: {challenge}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        return;
    }
    if first.starts_with("DELETE") {
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
        let _ = socket.shutdown().await;
        return;
    }
    let message: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let method = message["method"].as_str().unwrap_or("").to_owned();
    let Some(id) = message.get("id").cloned() else {
        let _ = socket
            .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
        let _ = socket.shutdown().await;
        return;
    };
    match method.as_str() {
        "initialize" => {
            write_json(
                &mut socket,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1"}}}}}}"#
                ),
            )
            .await
        }
        "tools/list" => {
            write_json(
                &mut socket,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"leak","inputSchema":{{"type":"object"}}}}]}}}}"#
                ),
            )
            .await
        }
        _ => match options.reply {
            Reply::Json(text) => {
                write_json(
                    &mut socket,
                    &format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"echo {text}"}}]}}}}"#
                    ),
                )
                .await
            }
            Reply::Sse(text) => {
                let data = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"echo {text}"}}]}}}}"#
                );
                let body = format!("data: {data}\n\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nmcp-session-id: s1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
            Reply::BigJson(n) => {
                let prefix = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":""#
                );
                let suffix = r#""}]}}"#;
                let total = prefix.len() + n + suffix.len();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n{prefix}"
                );
                write_big(&mut socket, &head, n, suffix, &sent).await;
            }
            Reply::BigJsonUnsized(n) => {
                let prefix = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":""#
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\nconnection: close\r\n\r\n{prefix}"
                );
                write_big(&mut socket, &head, n, r#""}]}}"#, &sent).await;
            }
            Reply::OversizedSse(n) => {
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nmcp-session-id: s1\r\nconnection: close\r\n\r\nid: 1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\ndata: ";
                write_big(&mut socket, head, n, "", &sent).await;
            }
            Reply::BigError(n) => {
                let head = "HTTP/1.1 500 Internal Server Error\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n";
                write_big(&mut socket, head, n, "", &sent).await;
            }
        },
    }
    let _ = socket.shutdown().await;
}

async fn write_json(socket: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
}

/// Write `head`, then `n` filler bytes in 256 KiB chunks, then `tail`, counting
/// filler bytes written until the client hangs up.
async fn write_big(
    socket: &mut tokio::net::TcpStream,
    head: &str,
    n: usize,
    tail: &str,
    sent: &AtomicUsize,
) {
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let chunk = vec![b'a'; 256 * 1024];
    let mut left = n;
    while left > 0 {
        let size = left.min(chunk.len());
        if socket.write_all(&chunk[..size]).await.is_err() {
            return;
        }
        sent.fetch_add(size, Ordering::SeqCst);
        left -= size;
    }
    let _ = socket.write_all(tail.as_bytes()).await;
}

pub fn config(url: &str, headers: &[(&str, HeaderFromEnv)]) -> McpServerConfig {
    McpServerConfig::new(
        "remote",
        McpTransport::StreamableHttp {
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            proxy: None,
            oauth: false,
        },
    )
    .expect("valid config")
    .with_timeout_ms(20_000)
    .expect("timeout")
}

pub fn fixed_env(value: &'static str) -> impl Fn(&str) -> Result<String, std::env::VarError> {
    move |_| Ok(value.to_owned())
}

pub async fn registered(caller: RmcpCaller, config: &McpServerConfig) -> ToolRegistry {
    let discovered = caller.discover_tools().await.expect("list tools");
    let mut registry = ToolRegistry::new();
    let report = replace_server_tools(
        &mut registry,
        config,
        discovered,
        Arc::new(caller),
        Exposure::Prefix,
    );
    assert!(report.is_complete(), "{:?}", report.refused);
    registry
}

/// Run a tool through the approval chain and registry, as a worker would.
pub async fn call(
    registry: &ToolRegistry,
    tool: &str,
    input: serde_json::Value,
) -> Result<ToolOutput, ToolError> {
    let ctx = ToolContext {
        worker: "w".into(),
        task: "t".into(),
        workspace_root: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        progress: kgraf_tools::ProgressSink::default(),
    };
    let discovered = kgraf_tools::DiscoveredTools::new();
    let view = ToolView {
        registry,
        discovered: &discovered,
    };
    let mut log = MemoryApprovalLog::default();
    let Verdict::Run(approved) = ApprovalChain::new(vec![])
        .review(
            view,
            PendingCall {
                call_id: "c".into(),
                tool: tool.to_owned(),
                input,
                worker: ctx.worker.clone(),
                task: ctx.task.clone(),
            },
            &mut log,
        )
        .await
        .expect("review")
    else {
        panic!("expected the call to be approved");
    };
    registry.execute(approved, &ctx).await
}

/// A server that answers a `tools/call` by SEP-1699 polling
/// the reply stream ends after an event id, and each GET with
/// `Last-Event-ID` gets one more empty event, until the `resumes`-th, which
/// carries the result. A GET without `Last-Event-ID` gets 405. Returns the URL
/// and the count of resumes served.
pub async fn polling_server(resumes: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let served = Arc::new(AtomicUsize::new(0));
    let call_id = Arc::new(Mutex::new(String::new()));
    let counter = served.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let served = counter.clone();
            let call_id = call_id.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head_end = loop {
                    let n = socket.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let lower = head.to_ascii_lowercase();
                let length = lower
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + length {
                    let n = socket.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let sse = |body: String| {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nmcp-session-id: s1\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                let json = |body: String| {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                let response = if head.starts_with("GET") {
                    if !lower.contains("last-event-id") {
                        "HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n".to_owned()
                    } else {
                        let n = served.fetch_add(1, Ordering::SeqCst) + 1;
                        if n >= resumes {
                            let id = call_id.lock().expect("lock").clone();
                            sse(format!(
                                "id: done\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"finished after {n} resumes\"}}]}}}}\n\n"
                            ))
                        } else {
                            sse(format!("id: e{n}\nretry: 1000\ndata: \n\n"))
                        }
                    }
                } else if head.starts_with("DELETE") {
                    "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_owned()
                } else {
                    let message: serde_json::Value =
                        serde_json::from_slice(&buf[head_end..]).unwrap_or_default();
                    let method = message["method"].as_str().unwrap_or("").to_owned();
                    match (message.get("id").cloned(), method.as_str()) {
                        (None, _) => {
                            "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned()
                        }
                        (Some(id), "initialize") => json(format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"f","version":"1"}}}}}}"#
                        )),
                        (Some(id), "tools/list") => json(format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"slow","inputSchema":{{"type":"object"}}}}]}}}}"#
                        )),
                        (Some(id), _) => {
                            *call_id.lock().expect("lock") = id.to_string();
                            sse("id: e0\nretry: 1000\ndata: \n\n".to_owned())
                        }
                    }
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}/mcp"), served)
}

/// A fake OAuth token endpoint: every request gets a new token, or, with
/// `redirect_to`, a 307 to that URL. Records each request.
pub async fn token_endpoint(redirect_to: Option<String>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let rec = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let rec = rec.clone();
            let redirect_to = redirect_to.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                rec.lock()
                    .expect("lock")
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let response = match &redirect_to {
                    Some(target) => format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nlocation: {target}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    ),
                    None => {
                        let body = r#"{"access_token":"new-access-token","token_type":"bearer","expires_in":3600}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Fake {
        url: format!("http://{addr}/token"),
        requests,
        big_body_bytes_sent: Arc::new(AtomicUsize::new(0)),
    }
}

/// Refresh a stored, expired token at `token_url` through `client`, the way
/// `OAuthConnector` builds its manager (keychain store replaced by memory).
pub async fn refresh_through(
    client: impl rmcp::transport::auth::OAuthHttpClient + 'static,
    token_url: &str,
) -> Result<(), rmcp::transport::auth::AuthError> {
    use rmcp::transport::auth::{
        AuthorizationManager, AuthorizationMetadata, CredentialStore, InMemoryCredentialStore,
        StoredCredentials,
    };
    let base = token_url.trim_end_matches("/token");
    let mut manager =
        AuthorizationManager::new_with_oauth_http_client(format!("{base}/mcp"), Arc::new(client))
            .await?;
    let metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": token_url,
    }))
    .expect("metadata");
    manager.set_metadata(metadata);
    manager.configure_client_id("kgraf-test")?;
    let store = InMemoryCredentialStore::new();
    let stored: StoredCredentials = serde_json::from_value(serde_json::json!({
        "client_id": "kgraf-test",
        "token_response": {
            "access_token": "old-access-token",
            "token_type": "bearer",
            "refresh_token": "refresh-SECRET-1",
            "expires_in": 1
        }
    }))
    .expect("credentials");
    store.save(stored).await?;
    manager.set_credential_store(store);
    manager.refresh_token().await.map(|_| ())
}
