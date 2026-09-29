//! Stdio MCP round-trip latency, against a real `mcp-echo-server` process (the real-speaker rule:
//! the mechanism, not a mock, and not merely a config setting).
//!
//! Two numbers are reported:
//! - `list_tools`: one `tools/list` directly through `RmcpCaller::discover_tools`,
//!   isolating the transport cost with no approval machinery involved.
//! - `call_tool_through_registry`: one `tools/call`, through the only path a real
//!   worker can use (`ApprovalChain::review` then `ToolRegistry::execute`).
//!   `CallPermit` has no public constructor outside `kgraf_tools` and is minted
//!   only inside `ToolRegistry::execute` (approval is bound to execution by
//!   construction, `kgraf_tools`'s own crate doc), so there is no lower-level
//!   "raw `McpCaller::call_tool`" path to benchmark separately from another crate;
//!   this number already is the harness's real per-call MCP cost.
//!
//! Run: `cargo bench -p kgraf-mcp --bench stdio_round_trip`.

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_mcp::{
    InMemoryAttributionSink, McpServerConfig, McpTransport, RmcpCaller, replace_server_tools,
};
use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
use kgraf_tools::{
    CancellationToken, DiscoveredTools, Exposure, ToolContext, ToolRegistry, ToolView,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;

/// See `tests/stdio_live.rs`'s copy of this helper for why `escargot`, not
/// `CARGO_BIN_EXE_mcp-echo-server`, locates the binary here.
fn echo_server_binary() -> std::path::PathBuf {
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
}

fn config() -> McpServerConfig {
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

fn ctx() -> ToolContext {
    ToolContext {
        worker: "bench-worker".into(),
        task: "bench-task".into(),
        workspace_root: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        progress: kgraf_tools::ProgressSink::default(),
    }
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let config = config();

    let sink = Arc::new(InMemoryAttributionSink::new());
    let caller = rt
        .block_on(RmcpCaller::connect_stdio(&config, sink))
        .expect("connect to the echo server");

    let mut group = c.benchmark_group("mcp_stdio_round_trip");

    group.bench_function("list_tools", |b| {
        b.iter(|| {
            let tools = rt
                .block_on(caller.discover_tools())
                .expect("discover_tools");
            black_box(tools);
        });
    });

    let discovered = rt
        .block_on(caller.discover_tools())
        .expect("discover tools once, for registration");
    let mut registry = ToolRegistry::new();
    let arc_caller: Arc<dyn kgraf_mcp::McpCaller> = Arc::new(caller);
    let report = replace_server_tools(
        &mut registry,
        &config,
        discovered,
        arc_caller,
        Exposure::Prefix,
    );
    assert!(report.is_complete());
    let call_ctx = ctx();

    group.bench_function("call_tool_through_registry", |b| {
        b.iter(|| {
            rt.block_on(async {
                let discovered = DiscoveredTools::new();
                let view = ToolView {
                    registry: &registry,
                    discovered: &discovered,
                };
                let mut log = MemoryApprovalLog::default();
                let Verdict::Run(approved) = ApprovalChain::new(vec![])
                    .review(
                        view,
                        PendingCall {
                            call_id: "bench".into(),
                            tool: "mcp__echoserver__echo".into(),
                            input: json!({"message": "bench"}),
                            worker: "bench-worker".into(),
                            task: "bench-task".into(),
                        },
                        &mut log,
                    )
                    .await
                    .expect("approval review")
                else {
                    panic!("expected approval");
                };
                let out = registry
                    .execute(approved, &call_ctx)
                    .await
                    .expect("execute");
                black_box(out);
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
