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

/// Serves one streaming reply that sends its headers and one text chunk, no
/// usage, then holds the connection open: a local server that reports usage
/// only in its last chunk, cut before that chunk.
async fn one_chunk_then_silence() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        // The headers and the JSON body, read until the body is whole.
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let event = format!(
            "data: {{\"id\":\"chatcmpl-cut\",\"object\":\"chat.completion.chunk\",\
             \"model\":\"{MODEL}\",\"choices\":[{{\"index\":0,\
             \"delta\":{{\"role\":\"assistant\",\"content\":\"partial\"}}}}]}}\n\n"
        );
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{event}"
        );
        if socket.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        // Hold the stream open until the client goes away.
        while matches!(socket.read(&mut chunk).await, Ok(n) if n > 0) {}
    });
    format!("http://{address}")
}

/// The real Chat Completions adapter, streaming, cut by the worker's deadline
/// after a chunk with no usage: kGraf stopped the call, so its row is
/// cancelled at zero by the adapter's own stream guard, with the request id
/// the chunk carried, not left unreconciled.
#[tokio::test(flavor = "multi_thread")]
async fn a_streaming_call_cut_by_the_deadline_settles_cancelled() {
    let url = one_chunk_then_silence().await;
    let mut env = Arc::into_inner(env(url)).expect("one owner");
    env.settings.call_mode = CallMode::Stream;
    let env = Arc::new(env);
    let brief = WorkerBrief {
        task: "cut-task".into(),
        worker: "cut-worker".into(),
        instruction: "Do the thing.".into(),
        workspace_root: std::env::temp_dir(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 4,
        context: ContextConfig::for_model(1_000_000).expect("config"),
        write_scope: None,
    };
    let options = SpawnOptions {
        deadline: Some(kgraf_runtime::WorkerDeadline {
            at: tokio::time::Instant::now() + Duration::from_millis(500),
            expected_round: None,
        }),
        ..SpawnOptions::default()
    };
    let mut handle = spawn_worker(env.clone(), brief, options)
        .await
        .expect("spawn");
    timeout(LIMIT, async {
        while handle.next_event().await.is_some() {}
    })
    .await
    .expect("the worker ended");
    let result = handle.result().await.expect("result");
    assert_eq!(result.outcome, WorkerOutcome::BudgetExhausted, "{result:?}");
    assert!(
        result.out_of_time.is_some_and(|stop| stop.cut),
        "{result:?}"
    );
    let rows = env.ledger.calls().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].settlement, kgraf_provider::Settlement::Cancelled);
    assert_eq!(rows[0].usage, kgraf_core::TokenUsage::default());
    assert_eq!(rows[0].request_id.as_deref(), Some("chatcmpl-cut"));
    // Cut before any usage: its spend is unknown, so it is listed for a
    // resolve against the provider's figure.
    let open = env.ledger.unreconciled().expect("query");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, rows[0].id);
}
