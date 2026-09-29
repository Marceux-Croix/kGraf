//! A local HTTP server that reads each request whole and never answers, for the
//! cancel-in-flight test and the `cancel_to_release/http_in_flight` benchmark.
//!
//! Shared by `tests/http_cancel.rs` and `benches/worker_loop.rs` (included by path),
//! so the regression test exercises the same server and wait the benchmark uses.
//!
//! Two properties matter, both learned from a benchmark hang:
//!
//! - A cancelled request leaves no client socket in TIME_WAIT. The client closes
//!   first, so without help each cancel parks one ephemeral port for 2 x MSL (30 s
//!   on macOS). Criterion ran about 16k iterations inside that window, used up the
//!   whole range (49152 to 65535), and the next connect failed. The server resets
//!   the connection when it reads the client's FIN (`SO_LINGER` zero), and a reset
//!   in FIN_WAIT_2 closes the client socket at once.
//! - A wait for a request never outlives the worker. A worker that fails before its
//!   request lands (connect refused, ports exhausted) must surface its result, not
//!   park the caller on an announcement that cannot come.

use kgraf_runtime::WorkerHandle;
use std::net::SocketAddr;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedReceiver;

/// Starts the server on an ephemeral loopback port. Returns its base URL and a
/// channel that receives, per request read whole, the client's address.
pub async fn silent_server() -> (String, UnboundedReceiver<SocketAddr>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let (received, announcements) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, peer)) = listener.accept().await else {
                return;
            };
            let received = received.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut announced = false;
                loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => {
                            // The client has gone. Reset rather than FIN so the
                            // client's port skips TIME_WAIT (see the module doc).
                            let _ = socket.set_zero_linger();
                            return;
                        }
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                    if !announced && request_complete(&buffer) {
                        announced = true;
                        let _ = received.send(peer);
                    }
                }
            });
        }
    });
    (format!("http://{address}"), announcements)
}

fn request_complete(buffer: &[u8]) -> bool {
    let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
    let length = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    buffer.len() >= end + 4 + length
}

/// How a wait for the worker's request ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Arrival {
    /// The server read the request whole; the call is in flight from `client`.
    Received { client: SocketAddr },
    /// The worker's event stream closed first: it ended without the request
    /// reaching the server. The caller takes `handle.result()` for the reason.
    WorkerEnded,
}

/// Waits until the server has read the worker's request, or the worker ends.
/// Events the worker emits meanwhile are consumed.
pub async fn request_or_end(
    handle: &mut WorkerHandle,
    announcements: &mut UnboundedReceiver<SocketAddr>,
) -> Arrival {
    loop {
        tokio::select! {
            // Prefer the announcement when both are ready, so a request that did
            // land is never reported as lost.
            biased;
            announced = announcements.recv() => {
                let client = announced.expect("the server task is alive");
                return Arrival::Received { client };
            }
            event = handle.next_event() => {
                if event.is_none() {
                    return Arrival::WorkerEnded;
                }
            }
        }
    }
}
