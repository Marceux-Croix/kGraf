//! The real-speaker rule: `RmcpCaller` is verified against a real, running MCP speaker, not a
//! mock. This test spawns `mcp-echo-server` (`kgraf-mcp-test-server`, a second
//! real OS process, dev-dependency-only) and talks real MCP to it over stdio:
//! `initialize`, `tools/list`, and `tools/call` round trips exercising text,
//! binary, tool-level-error, environment-allowlist and cancellation behavior.
//!
//! No network access: the server is a local child process, matching the real-speaker rule's "do
//! not contact hosted servers".

use kgraf_mcp::{
    InMemoryAttributionSink, McpAttributionSink, McpCallKind, McpServerConfig, McpTransport,
    RmcpCaller, replace_server_tools,
};
use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
use kgraf_tools::{
    CancellationToken, Exposure, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolView,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Build (or reuse the already-built) `mcp-echo-server` binary from the sibling
/// `kgraf-mcp-test-server` crate and return its path. `CARGO_BIN_EXE_<name>`
/// does not reach a bin in a workspace-sibling crate (see the `Cargo.toml`
/// comment), so `escargot` runs `cargo build` against that crate's own manifest.
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

fn echo_server_config() -> McpServerConfig {
    McpServerConfig::new(
        "echoserver",
        McpTransport::Stdio {
            command: echo_server_binary().to_string_lossy().into_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        },
    )
    .expect("valid stdio config")
}

fn test_context(cancel: CancellationToken) -> ToolContext {
    ToolContext {
        worker: "test-worker".into(),
        task: "test-task".into(),
        workspace_root: std::env::temp_dir(),
        cancel,
        progress: kgraf_tools::ProgressSink::default(),
    }
}

/// Run `tool` with `input` through the real approval chain and registry, the same
/// path a worker's tool call takes (register.rs's own tests use the same shape).
async fn call_with_ctx(
    registry: &ToolRegistry,
    tool: &str,
    input: serde_json::Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
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
    registry.execute(approved, ctx).await
}

async fn call(registry: &ToolRegistry, tool: &str, input: serde_json::Value) -> String {
    let ctx = test_context(CancellationToken::new());
    call_with_ctx(registry, tool, input, &ctx)
        .await
        .expect("execute")
        .render()
}

#[tokio::test]
async fn a_real_stdio_mcp_server_lists_and_serves_its_tools() {
    let config = echo_server_config();
    let sink = Arc::new(InMemoryAttributionSink::new());
    let caller = RmcpCaller::connect_stdio(&config, sink.clone())
        .await
        .expect("connect to the real echo server process");

    let discovered = caller.discover_tools().await.expect("list tools");
    let mut names: Vec<&str> = discovered.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "boom",
            "echo",
            "getenv",
            "http_header",
            "notify_then_echo",
            "sleep_ms",
            "tiny_png"
        ]
    );

    let mut registry = ToolRegistry::new();
    let caller: Arc<dyn kgraf_mcp::McpCaller> = Arc::new(caller);
    let report = replace_server_tools(&mut registry, &config, discovered, caller, Exposure::Prefix);
    assert!(report.is_complete(), "{:?}", report.refused);

    let echoed = call(
        &registry,
        "mcp__echoserver__echo",
        json!({"message": "hi from a real MCP round trip"}),
    )
    .await;
    assert_eq!(echoed, "hi from a real MCP round trip");

    let image = call(&registry, "mcp__echoserver__tiny_png", json!({})).await;
    assert!(image.contains("image/png"), "{image}");
    assert!(!image.contains("iVBOR"), "no base64 leaked: {image}");
    assert!(
        !image.to_lowercase().contains("ihdr"),
        "raw bytes must not leak: {image}"
    );

    let failed = call(&registry, "mcp__echoserver__boom", json!({})).await;
    assert!(failed.starts_with("ERROR"), "{failed}");
    assert!(failed.contains("the tool refused"));

    // The construction-required attribution sink (main-session ruling on 0051)
    // recorded the connect, the list, and every call above.
    let events = sink.events();
    assert!(events.iter().any(|e| e.kind == McpCallKind::Connect));
    assert!(events.iter().any(|e| e.kind == McpCallKind::ListTools));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == McpCallKind::CallTool)
            .count(),
        3
    );
}

/// Tested end to end through a real child process: a stdio
/// server does not inherit this test process's whole environment. `PATH` (in the
/// allowlist) must still reach the child, or nothing using `PATH` to resolve an
/// interpreter would ever start; an unrelated secret-shaped variable must not.
#[tokio::test]
async fn a_stdio_child_does_not_inherit_an_unallowlisted_environment_variable() {
    // SAFETY (test-only): no other test in this binary reads or writes this name.
    unsafe {
        std::env::set_var("KGRAF_TEST_PROVIDER_KEY", "leak-me-not");
    }

    let config = echo_server_config();
    let sink: Arc<dyn McpAttributionSink> = Arc::new(InMemoryAttributionSink::new());
    let caller = RmcpCaller::connect_stdio(&config, sink)
        .await
        .expect("connect");
    let discovered = caller.discover_tools().await.expect("list tools");
    let mut registry = ToolRegistry::new();
    let caller: Arc<dyn kgraf_mcp::McpCaller> = Arc::new(caller);
    replace_server_tools(&mut registry, &config, discovered, caller, Exposure::Prefix);

    let leaked = call(
        &registry,
        "mcp__echoserver__getenv",
        json!({"name": "KGRAF_TEST_PROVIDER_KEY"}),
    )
    .await;
    assert_eq!(
        leaked, "<absent>",
        "an unallowlisted environment variable must not reach the child"
    );

    let path = call(
        &registry,
        "mcp__echoserver__getenv",
        json!({"name": "PATH"}),
    )
    .await;
    assert_ne!(
        path, "<absent>",
        "an allowlisted variable (PATH) must still reach the child"
    );

    unsafe {
        std::env::remove_var("KGRAF_TEST_PROVIDER_KEY");
    }
}

/// Cancelling a worker's `ToolContext` mid-call stops an
/// in-flight MCP call immediately, rather than waiting for the server's answer or
/// the connection timeout.
#[tokio::test]
async fn cancelling_the_context_stops_an_in_flight_call_immediately() {
    let config = echo_server_config();
    let sink: Arc<dyn McpAttributionSink> = Arc::new(InMemoryAttributionSink::new());
    let caller = RmcpCaller::connect_stdio(&config, sink)
        .await
        .expect("connect");
    let discovered = caller.discover_tools().await.expect("list tools");
    let mut registry = ToolRegistry::new();
    let caller: Arc<dyn kgraf_mcp::McpCaller> = Arc::new(caller);
    replace_server_tools(&mut registry, &config, discovered, caller, Exposure::Prefix);
    let registry = Arc::new(registry);

    let cancel = CancellationToken::new();
    let ctx = test_context(cancel.clone());
    let registry_for_task = registry.clone();
    let handle = tokio::spawn(async move {
        call_with_ctx(
            &registry_for_task,
            "mcp__echoserver__sleep_ms",
            json!({"duration_ms": 60_000}),
            &ctx,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    let started_cancel = std::time::Instant::now();
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the call must return long before the 60s sleep or the 5s test timeout")
        .expect("task did not panic");

    assert!(
        started_cancel.elapsed() < Duration::from_secs(5),
        "cancellation must be near-immediate, not wait out the sleep"
    );
    assert!(
        matches!(result, Err(ToolError::Cancelled { .. })),
        "expected a Cancelled error, got {result:?}"
    );
}
