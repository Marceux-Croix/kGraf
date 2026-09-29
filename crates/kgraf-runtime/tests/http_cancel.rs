//! Regression tests for the `cancel_to_release/http_in_flight` benchmark hang.
//!
//! The benchmark cancelled about 16k real HTTP requests inside 30 s. Each cancel
//! left the client's port in TIME_WAIT, the ephemeral range (16384 ports on macOS)
//! ran out, the next connect failed, and the worker ended `Failed` before its
//! request reached the server. The benchmark waited only on the server's
//! announcement, so it parked forever at 0% CPU.
//!
//! Both tests run the real Chat Completions adapter through the worker loop, and
//! each is bounded by a timeout so a regression fails instead of hanging CI.

#[path = "support/silent_http.rs"]
mod silent_http;

use kgraf_core::Protocol;
use kgraf_provider::{Content, Ledger, Provider, ProviderConfig};
use kgraf_provider_openai::{ChatCompletionsAdapter, UsageDialect};
use kgraf_runtime::{
    CallMode, ContextConfig, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot, SharedPrefix,
    SpawnOptions, WorkerBrief, WorkerEnv, WorkerHandle, WorkerOutcome, spawn_worker,
};
use kgraf_tools::ToolRegistry;
use silent_http::{Arrival, request_or_end, silent_server};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const MODEL: &str = "cancel-model";
/// Far above either test's real duration (well under a second), far below a
/// CI job's patience.
const LIMIT: Duration = Duration::from_secs(20);

fn env(url: String) -> Arc<WorkerEnv> {
    let provider: Arc<dyn Provider> = Arc::new(
        ChatCompletionsAdapter::new(
            ProviderConfig::local(url, vec![MODEL.into()], Protocol::Inclusive),
            UsageDialect::Details,
        )
        .expect("adapter"),
    );
    let registry = ToolRegistry::new();
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "cancel-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a test agent.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("prefix");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    Arc::new(env)
}

async fn spawn(env: &Arc<WorkerEnv>) -> WorkerHandle {
    let brief = WorkerBrief {
        task: "cancel-task".into(),
        worker: "cancel-worker".into(),
        instruction: "Do the thing.".into(),
        workspace_root: std::env::temp_dir(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 4,
        context: ContextConfig::for_model(1_000_000).expect("config"),
        write_scope: None,
    };
    spawn_worker(env.clone(), brief, SpawnOptions::default())
        .await
        .expect("spawn")
}

/// A worker whose connect is refused ends before any request lands. The wait
/// must return that, with the worker's failure, instead of waiting forever on
/// an announcement that cannot come (the bench's hang).
#[tokio::test(flavor = "multi_thread")]
async fn a_request_that_never_lands_ends_the_wait() {
    // A port that was just free and now has no listener: connect is refused.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = closed.local_addr().expect("address");
    drop(closed);
    // The server exists only so there is an announcement channel to wait on;
    // the worker is pointed elsewhere.
    let (_url, mut announcements) = silent_server().await;

    let env = env(format!("http://{address}"));
    let mut handle = spawn(&env).await;
    let arrival = timeout(LIMIT, request_or_end(&mut handle, &mut announcements))
        .await
        .expect("the wait ended instead of hanging");
    assert_eq!(arrival, Arrival::WorkerEnded);

    let result = handle.result().await.expect("result");
    let WorkerOutcome::Failed { error } = result.outcome else {
        panic!("expected a failure, got {:?}", result.outcome);
    };
    assert!(error.contains("never sent"), "{error}");
}

/// Cancels in a row for the port test. Each must free its port; 50 keeps the test
/// well under a second while exercising the repeated-cancel pattern the benchmark
/// runs thousands of times.
const CANCELS: usize = 50;

/// A cancelled in-flight request leaves its client port free at once, not in
/// TIME_WAIT for 2 x MSL, so repeated cancels cannot exhaust the ephemeral range.
/// Exhaustion also failed unrelated HTTP tests running at the same time (4 of 5
/// in `kgraf-cli`'s `acceptance_providers`, "request ... was never sent").
/// Proved per cancel by binding the exact client address again without
/// `SO_REUSEADDR`, which the kernel refuses while a TIME_WAIT socket holds it.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_requests_leave_no_client_port_in_time_wait() {
    let (url, mut announcements) = silent_server().await;
    let env = env(url);
    for round in 0..CANCELS {
        let mut handle = spawn(&env).await;
        let arrival = timeout(LIMIT, request_or_end(&mut handle, &mut announcements))
            .await
            .expect("the request landed or the worker ended");
        let Arrival::Received { client } = arrival else {
            panic!(
                "round {round}: the worker ended before its request landed: {:?}",
                handle.result().await
            );
        };
        handle.cancel();
        timeout(LIMIT, async {
            while handle.next_event().await.is_some() {}
        })
        .await
        .expect("the event channel closed after cancel");
        let result = handle.result().await.expect("result");
        assert_eq!(result.outcome, WorkerOutcome::Cancelled, "round {round}");

        // The server resets once it reads the client's FIN; poll briefly for
        // that. Without the reset the port stays held for 30 s (macOS) and this
        // times out.
        timeout(LIMIT, async {
            loop {
                let socket = tokio::net::TcpSocket::new_v4().expect("socket");
                match socket.bind(client) {
                    Ok(()) => return,
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(e) => panic!("bind {client}: {e}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("round {round}: client port {client} still held after cancel (TIME_WAIT)")
        });
    }
}
