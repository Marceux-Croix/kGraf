//! The real-speaker rule: `RmcpCaller` over Streamable HTTP, verified against a real, running MCP
//! speaker. Each test spawns `mcp-echo-server --http` (`kgraf-mcp-test-server`,
//! `rmcp`'s own `StreamableHttpService` behind a hyper listener on 127.0.0.1) as a
//! second OS process and exchanges real HTTP with it.
//!
//! What is tested is the mechanism, not a setting (test the mechanism, not the setting): the server's own
//! request counters show that replies came as SSE or as JSON, that a GET stream was
//! opened and that DELETE ended the session; a session the server closed comes back
//! as a typed error with no new session created; a credential the server echoes in
//! an error body is absent from the error and from every trace line.
//!
//! No external network: every server is a local child process on loopback.

use kgraf_mcp::{
    HeaderFromEnv, InMemoryAttributionSink, McpCallKind, McpCallOutcome, McpConnectError,
    McpServerConfig, McpTransport, REDACTED, RmcpCaller, SUPPORTED_HTTP_PROTOCOL_VERSIONS,
    replace_server_tools,
};
use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
use kgraf_tools::{
    CancellationToken, Exposure, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolView,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::env::VarError;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

/// Build (or reuse) the `mcp-echo-server` binary; see `stdio_live.rs` for why
/// `escargot` and not `CARGO_BIN_EXE_*`.
fn echo_server_binary() -> std::path::PathBuf {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        escargot::CargoBuild::new()
            .bin("mcp-echo-server")
            .manifest_path(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../kgraf-mcp-test-server/Cargo.toml"
            ))
            .current_release()
            .current_target()
            .run()
            .expect("build kgraf-mcp-test-server's mcp-echo-server binary")
            .path()
            .to_path_buf()
    })
    .clone()
}

/// A running `mcp-echo-server --http`. Killed when dropped.
struct HttpServer {
    _child: tokio::process::Child,
    url: String,
    admin: String,
}

impl HttpServer {
    async fn spawn(args: &[&str]) -> Self {
        let mut child = tokio::process::Command::new(echo_server_binary())
            .arg("--http")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the echo server");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("the server announces its address in time")
            .expect("read stdout")
            .expect("a first line");
        let url = line
            .strip_prefix("LISTENING ")
            .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
            .to_owned();
        let admin = url.trim_end_matches("/mcp").to_owned() + "/admin";
        Self {
            _child: child,
            url,
            admin,
        }
    }

    async fn stats(&self) -> serde_json::Value {
        reqwest::get(format!("{}/stats", self.admin))
            .await
            .expect("stats request")
            .json()
            .await
            .expect("stats json")
    }

    async fn stat(&self, key: &str) -> u64 {
        self.stats().await[key].as_u64().expect("a count")
    }

    /// Poll a counter until it reaches `at_least`, for effects the client
    /// starts in the background (the GET stream, the DELETE on close).
    async fn wait_for(&self, key: &str, at_least: u64) -> u64 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let value = self.stat(key).await;
            if value >= at_least || tokio::time::Instant::now() > deadline {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn expire_sessions(&self) -> u64 {
        let body: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/expire-sessions", self.admin))
            .send()
            .await
            .expect("expire request")
            .json()
            .await
            .expect("expire json");
        body["expired"].as_u64().expect("a count")
    }
}

fn config(url: &str, headers: &[(&str, HeaderFromEnv)]) -> McpServerConfig {
    McpServerConfig::new(
        "remote",
        McpTransport::StreamableHttp {
            url: url.to_owned(),
            proxy: None,
            oauth: false,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        },
    )
    .expect("valid http config")
    .with_timeout_ms(10_000)
    .expect("timeout")
}

/// A fixed environment, so no test mutates the real one.
fn env(
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

fn bearer(var: &str) -> HeaderFromEnv {
    HeaderFromEnv::new(var).with_prefix("Bearer ")
}

fn test_context() -> ToolContext {
    ToolContext {
        worker: "http-worker".into(),
        task: "http-task".into(),
        workspace_root: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        progress: kgraf_tools::ProgressSink::default(),
    }
}

/// Run `tool` through the real approval chain and registry: the path a worker's
/// call takes, the same one `stdio_live.rs` uses for stdio servers.
async fn call(
    registry: &ToolRegistry,
    tool: &str,
    input: serde_json::Value,
) -> Result<ToolOutput, ToolError> {
    let ctx = test_context();
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
                call_id: format!("call-{tool}"),
                tool: tool.to_owned(),
                input,
                worker: ctx.worker.clone(),
                task: ctx.task.clone(),
            },
            &mut log,
        )
        .await
        .expect("approval review")
    else {
        panic!("expected the call to be approved");
    };
    registry.execute(approved, &ctx).await
}

async fn registered(caller: RmcpCaller, config: &McpServerConfig) -> ToolRegistry {
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

#[tokio::test]
async fn session_mode_initializes_lists_and_calls_over_sse_then_deletes_the_session() {
    let server = HttpServer::spawn(&["--require-header", "authorization=Bearer tok-LIVE-1"]).await;
    let config = config(&server.url, &[("Authorization", bearer("REMOTE_TOKEN"))]);
    let sink = Arc::new(InMemoryAttributionSink::new());
    let caller = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &env(&[("REMOTE_TOKEN", "tok-LIVE-1")]),
        sink.clone(),
    )
    .await
    .expect("connect: the env-sourced header satisfied the server");
    let version = caller.protocol_version().expect("negotiated");
    assert!(
        SUPPORTED_HTTP_PROTOCOL_VERSIONS.contains(&version.as_str()),
        "{version}"
    );

    let discovered = caller.discover_tools().await.expect("list tools");
    let mut names: Vec<&str> = discovered.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert!(
        names.contains(&"echo") && names.contains(&"http_header"),
        "{names:?}"
    );
    let caller = Arc::new(caller);
    let mut registry = ToolRegistry::new();
    let report = replace_server_tools(
        &mut registry,
        &config,
        discovered,
        caller.clone(),
        Exposure::Prefix,
    );
    assert!(report.is_complete(), "{:?}", report.refused);

    let echoed = call(
        &registry,
        "mcp__remote__echo",
        json!({"message": "over http"}),
    )
    .await
    .expect("echo")
    .render();
    assert!(echoed.contains("over http"), "{echoed}");

    // A reply with a notification ahead of the result: two SSE events, one POST.
    let streamed = call(
        &registry,
        "mcp__remote__notify_then_echo",
        json!({"message": "after a notification"}),
    )
    .await
    .expect("notify_then_echo")
    .render();
    assert!(streamed.contains("after a notification"), "{streamed}");

    // The server saw the configured header on the tool call (not `<absent>`),
    // and the value it echoed back was scrubbed before it reached the result.
    let header = call(
        &registry,
        "mcp__remote__http_header",
        json!({"name": "authorization"}),
    )
    .await
    .expect("http_header")
    .render();
    assert!(header.contains(REDACTED), "{header}");
    assert!(!header.contains("tok-LIVE-1"), "{header}");
    assert!(!header.contains("<absent>"), "{header}");

    // initialize, tools/list and three tools/call, each answered as SSE.
    assert!(
        server.stat("post_sse").await >= 5,
        "{}",
        server.stats().await
    );
    assert_eq!(
        server.stat("post_json").await,
        0,
        "{}",
        server.stats().await
    );
    assert_eq!(server.stat("rejected").await, 0);
    assert!(
        server.wait_for("get_sse", 1).await >= 1,
        "the client opens the optional GET stream: {}",
        server.stats().await
    );
    assert_eq!(server.stat("sessions").await, 1);

    drop(registry);
    let caller = Arc::try_unwrap(caller).unwrap_or_else(|_| panic!("sole owner"));
    caller.close().await;
    assert_eq!(
        server.wait_for("delete", 1).await,
        1,
        "close ends the session with DELETE: {}",
        server.stats().await
    );
    assert_eq!(server.wait_for_sessions_gone().await, 0);

    let events = sink.events();
    assert!(
        events.iter().any(|e| e.kind == McpCallKind::OpenStream),
        "the GET stream open is attributed: {events:?}"
    );
    let events: Vec<_> = events
        .into_iter()
        .filter(|e| e.kind != McpCallKind::OpenStream)
        .collect();
    assert_eq!(events[0].kind, McpCallKind::Connect);
    assert_eq!(events[0].outcome, McpCallOutcome::Success);
    let calls: Vec<_> = events
        .iter()
        .filter(|e| e.kind == McpCallKind::CallTool)
        .collect();
    assert_eq!(calls.len(), 3, "{events:?}");
    assert!(
        calls
            .iter()
            .all(|e| e.worker.as_ref().map(|w| w.as_str()) == Some("http-worker"))
    );
}

impl HttpServer {
    async fn wait_for_sessions_gone(&self) -> u64 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let live = self.stat("sessions").await;
            if live == 0 || tokio::time::Instant::now() > deadline {
                return live;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test]
async fn json_mode_answers_with_application_json_and_falls_back_to_sse_for_a_notification() {
    let server = HttpServer::spawn(&["--json"]).await;
    let config = config(&server.url, &[]);
    let sink = Arc::new(InMemoryAttributionSink::new());
    let caller = RmcpCaller::connect_streamable_http_with_env(&config, &env(&[]), sink)
        .await
        .expect("connect");
    let registry = registered(caller, &config).await;
    let json_before = server.stat("post_json").await;
    assert!(
        json_before >= 2,
        "initialize and tools/list as JSON: {}",
        server.stats().await
    );

    let echoed = call(
        &registry,
        "mcp__remote__echo",
        json!({"message": "plain json"}),
    )
    .await
    .expect("echo")
    .render();
    assert!(echoed.contains("plain json"), "{echoed}");
    assert_eq!(server.stat("post_json").await, json_before + 1);

    let sse_before = server.stat("post_sse").await;
    let streamed = call(
        &registry,
        "mcp__remote__notify_then_echo",
        json!({"message": "streamed anyway"}),
    )
    .await
    .expect("notify_then_echo")
    .render();
    assert!(streamed.contains("streamed anyway"), "{streamed}");
    assert_eq!(server.stat("post_sse").await, sse_before + 1);
    assert_eq!(server.stat("sessions").await, 0, "JSON mode is stateless");
}

#[tokio::test]
async fn a_session_the_server_ended_is_a_typed_error_and_no_new_session_starts() {
    let server = HttpServer::spawn(&[]).await;
    let config = config(&server.url, &[]);
    let sink = Arc::new(InMemoryAttributionSink::new());
    let caller = RmcpCaller::connect_streamable_http_with_env(&config, &env(&[]), sink)
        .await
        .expect("connect");
    let discovered = caller
        .discover_tools()
        .await
        .expect("list tools while live");
    assert!(!discovered.is_empty());
    assert_eq!(server.expire_sessions().await, 1);

    let error = caller
        .discover_tools()
        .await
        .expect_err("the session is gone");
    assert!(
        matches!(error, McpConnectError::SessionExpired { ref server } if server == "remote"),
        "{error:?}"
    );

    // The same failure through a worker's tool call: a tool error that names it.
    let caller = Arc::new(caller);
    let mut registry = ToolRegistry::new();
    replace_server_tools(
        &mut registry,
        &config,
        discovered,
        caller.clone(),
        Exposure::Prefix,
    );
    let tool_error = call(&registry, "mcp__remote__echo", json!({"message": "x"}))
        .await
        .expect_err("a call on an ended session fails");
    let ToolError::Internal { reason, .. } = &tool_error else {
        panic!("an internal tool error, got {tool_error:?}");
    };
    assert!(reason.contains("ended the session"), "{reason}");

    // `rmcp`'s default would have re-initialized here; the server would then hold
    // a new session.
    assert_eq!(server.stat("sessions").await, 0, "{}", server.stats().await);
}

#[tokio::test]
async fn a_server_that_negotiates_an_unsupported_version_is_refused() {
    let server = HttpServer::spawn(&["--protocol-version", "2024-11-05"]).await;
    let config = config(&server.url, &[]);
    let sink = Arc::new(InMemoryAttributionSink::new());
    let error = match RmcpCaller::connect_streamable_http_with_env(&config, &env(&[]), sink.clone())
        .await
    {
        Ok(caller) => panic!(
            "a 2024-11-05 server must be refused; it negotiated {:?}",
            caller.protocol_version()
        ),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            McpConnectError::ProtocolVersion { ref offered, .. } if offered == "2024-11-05"
        ),
        "{error:?}"
    );
    // GET stream opens are attributed too; this test is about the connect.
    let events: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|e| e.kind == McpCallKind::Connect)
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].outcome, McpCallOutcome::ProtocolError);
    // The refused session is ended, not left open on the server.
    assert_eq!(
        server.wait_for("delete", 1).await,
        1,
        "{}",
        server.stats().await
    );
}

/// A `MakeWriter` that keeps every formatted trace line.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A current-thread runtime, so the thread-local trace subscriber also sees the
/// lines `rmcp`'s background worker task writes.
#[tokio::test(flavor = "current_thread")]
async fn a_credential_the_server_echoes_is_absent_from_the_error_and_the_traces() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server =
        HttpServer::spawn(&["--require-header", "authorization=Bearer the-right-one"]).await;
    let config = config(&server.url, &[("Authorization", bearer("REMOTE_TOKEN"))]);
    let sink = Arc::new(InMemoryAttributionSink::new());
    let Err(error) = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &env(&[("REMOTE_TOKEN", "wrong-SECRET-777")]),
        sink,
    )
    .await
    else {
        panic!("a wrong credential must be refused");
    };
    assert!(
        matches!(error, McpConnectError::HttpStatus { status: 401, .. }),
        "{error:?}"
    );
    // The server did receive the request, and did echo the credential.
    assert_eq!(server.stat("rejected").await, 1);

    let shown = format!("{error} {error:?}");
    assert!(!shown.contains("wrong-SECRET-777"), "{shown}");
    assert!(shown.contains("rejected credentials"), "{shown}");
    assert!(shown.contains(REDACTED), "{shown}");

    let traces = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    assert!(
        traces.contains("401"),
        "rmcp traced the failure, so the check below means something: {traces}"
    );
    assert!(!traces.contains("wrong-SECRET-777"), "{traces}");
}

#[tokio::test]
async fn a_header_variable_that_is_not_set_fails_before_any_request() {
    let server = HttpServer::spawn(&[]).await;
    let config = config(
        &server.url,
        &[("Authorization", bearer("NOT_SET_ANYWHERE"))],
    );
    let sink = Arc::new(InMemoryAttributionSink::new());
    let Err(error) =
        RmcpCaller::connect_streamable_http_with_env(&config, &env(&[]), sink.clone()).await
    else {
        panic!("an unset variable must fail the connect");
    };
    assert!(
        matches!(
            error,
            McpConnectError::HeaderEnv { ref variable, reason: "is not set", .. }
                if variable == "NOT_SET_ANYWHERE"
        ),
        "{error:?}"
    );
    let stats = server.stats().await;
    for key in ["post_sse", "post_json", "post_other", "rejected"] {
        assert_eq!(stats[key], 0, "{key}: {stats}");
    }
    assert!(
        sink.events().is_empty(),
        "no egress, so nothing to attribute"
    );
}

/// The production constructor reads the real process environment. `cargo test`
/// sets `CARGO_PKG_NAME` in the test process, so no test has to mutate it.
#[tokio::test]
async fn the_default_constructor_reads_the_process_environment() {
    let value = std::env::var("CARGO_PKG_NAME").expect("cargo sets CARGO_PKG_NAME for tests");
    let requirement = format!("x-probe={value}");
    let server = HttpServer::spawn(&["--require-header", &requirement]).await;
    let config = config(
        &server.url,
        &[(
            "X-Probe",
            HeaderFromEnv::new("CARGO_PKG_NAME").with_secret(false),
        )],
    );
    let caller =
        RmcpCaller::connect_streamable_http(&config, Arc::new(InMemoryAttributionSink::new()))
            .await
            .expect("the header read from the process environment satisfied the server");
    assert_eq!(server.stat("rejected").await, 0);
    caller.close().await;
}

/// Connect, list, close under a TRACE subscriber with `directives`; the text.
async fn traced_session(server: &HttpServer, directives: &str) -> String {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(directives))
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let config = config(&server.url, &[]);
    let caller = RmcpCaller::connect_streamable_http_with_env(
        &config,
        &env(&[]),
        Arc::new(InMemoryAttributionSink::new()),
    )
    .await
    .expect("connect");
    caller.discover_tools().await.expect("list");
    caller.close().await;
    drop(guard);
    String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8")
}

/// `rmcp` logs the session id when it deletes a session. With the crate's
/// recommended filter it does not reach the log. Current-thread runtime, so the
/// thread-local subscriber sees `rmcp`'s worker task.
#[tokio::test(flavor = "current_thread")]
async fn the_recommended_log_filter_keeps_the_session_id_out_of_traces() {
    let server = HttpServer::spawn(&[]).await;
    let unfiltered = traced_session(&server, "trace").await;
    assert!(
        unfiltered.contains("session_id"),
        "without the filter rmcp logs it, so the check below means something"
    );
    let filtered = traced_session(
        &server,
        &format!("trace,{}", kgraf_mcp::oauth::RECOMMENDED_LOG_FILTER),
    )
    .await;
    assert!(!filtered.contains("session_id"), "{filtered}");
    assert!(
        !filtered.is_empty(),
        "other trace lines still pass the filter"
    );
}
