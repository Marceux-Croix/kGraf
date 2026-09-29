//! The concrete [`kgraf_tools::external::ExternalExecutor`] for Terminal-Bench
//! runs, and its wire protocol with the Python Harbor `BaseAgent` shim.
//!
//! # Why this crate exists, and why it is not `kgraf-tools`
//!
//! `kgraf-tools::external` defines the trait and the seam inside `ShellTool`;
//! it ships no concrete implementation. This crate is that implementation. It
//! lives outside `kgraf-tools` (and is `publish = false`) because it is Harbor-
//! and Terminal-Bench-specific process wiring for a benchmark, not something
//! the shipped desktop app loads; keeping it separate means a change here
//! never touches `kgraf-tools` core.
//!
//! # Direction of control
//!
//! Harbor's `BaseAgent.run()` (Python) owns the live `BaseEnvironment`, so it
//! is the only side that can execute in the task container. The Rust side
//! (kGraf's own `worker`/`orchestrate` CLI, spawned by the Python shim) is the
//! *client*: it connects to a Unix domain socket the shim already listens on
//! and sends one request per routed shell command and one per host-side file
//! write. The shim is the *server*.
//!
//! # Wire protocol
//!
//! One JSON object per line, one response line per request line.
//!
//! | request | response |
//! |---|---|
//! | `{"op":"hello","host_root":P}` | `{"ok":bool,"error":str or null}` |
//! | `{"op":"exec","cmd":[..],"timeout_sec":f}` | stdout and stderr base64, `exit_code`, `timed_out`, `error` |
//! | `{"op":"push"}` | `{"ok":bool,"error":str or null}` |
//!
//! # File sync
//!
//! The host worktree `host_root` and the container working directory are kept
//! equal by hash-checked deltas, done by the shim (see `bridge_server.py`):
//! before a command the host's changes are uploaded and verified by sha256 in
//! the container; after it the container's changes are downloaded and verified
//! by sha256 on the host, before the response returns. `push` runs only the
//! first half, after a host-side write. The protocol carries no file bytes.
use async_trait::async_trait;
use kgraf_tools::external::{ExternalExecutor, ExternalOutcome};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("cannot connect to the bridge socket at {path}: {source}")]
    Connect {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the bridge refused the session at {path}: {reason}")]
    Refused {
        path: std::path::PathBuf,
        reason: String,
    },
}

#[derive(Serialize)]
struct HelloRequest<'a> {
    op: &'static str,
    host_root: &'a str,
}

#[derive(Serialize)]
struct Request<'a> {
    op: &'static str,
    cmd: &'a [String],
    timeout_sec: f64,
}

#[derive(Serialize)]
struct PushRequest {
    op: &'static str,
}

/// The answer to `hello` and `push`.
#[derive(Deserialize)]
struct Ack {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct Response {
    stdout_b64: String,
    stderr_b64: String,
    exit_code: Option<i32>,
    timed_out: bool,
    error: Option<String>,
}

/// The concrete, subprocess-bridged [`ExternalExecutor`]. One instance is bound
/// to one Unix socket (one Python shim, one live task container).
pub struct ProcessExecutor {
    stream: Mutex<BufReader<UnixStream>>,
}

impl ProcessExecutor {
    /// Connect to a Python shim already listening at `socket_path`, and tell
    /// it which host directory to keep equal to the container's working
    /// directory (`host_root`: the worker's own worktree). The shim checks
    /// that the two trees are equal now; if they are not it refuses, and so
    /// does this call.
    pub async fn connect(socket_path: &Path, host_root: &Path) -> Result<Self, BridgeError> {
        let connect_err = |source| BridgeError::Connect {
            path: socket_path.to_path_buf(),
            source,
        };
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(connect_err)?;
        let mut stream = BufReader::new(stream);
        let hello = HelloRequest {
            op: "hello",
            host_root: &host_root.to_string_lossy(),
        };
        let line = serde_json::to_string(&hello).expect("HelloRequest always serializes");
        write_line(&mut stream, &line).await.map_err(connect_err)?;
        let response = read_json_line(&mut stream).await.map_err(connect_err)?;
        let ack: Ack = serde_json::from_str(&response).map_err(|e| {
            connect_err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad hello response: {e}"),
            ))
        })?;
        if !ack.ok {
            return Err(BridgeError::Refused {
                path: socket_path.to_path_buf(),
                reason: ack.error.unwrap_or_else(|| "no reason given".into()),
            });
        }
        Ok(Self {
            stream: Mutex::new(stream),
        })
    }

    /// One request, one response line, under the connection lock.
    async fn round_trip(&self, line: &str) -> Result<String, String> {
        let mut stream = self.stream.lock().await;
        write_line(&mut stream, line)
            .await
            .map_err(|e| format!("writing the request to the bridge socket: {e}"))?;
        read_json_line(&mut stream)
            .await
            .map_err(|e| format!("reading the bridge response: {e}"))
    }
}

#[async_trait]
impl ExternalExecutor for ProcessExecutor {
    async fn exec(&self, cmd: &[String], timeout: Duration) -> Result<ExternalOutcome, String> {
        let request = Request {
            op: "exec",
            cmd,
            timeout_sec: timeout.as_secs_f64(),
        };
        let line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        let response = self.round_trip(&line).await?;
        let response: Response = serde_json::from_str(&response)
            .map_err(|e| format!("bad JSON from the bridge: {e} (line: {response})"))?;
        if let Some(err) = response.error {
            return Err(format!("the bridge shim reported an error: {err}"));
        }

        use base64::Engine;
        let stdout = base64::engine::general_purpose::STANDARD
            .decode(&response.stdout_b64)
            .map_err(|e| format!("bad stdout_b64: {e}"))?;
        let stderr = base64::engine::general_purpose::STANDARD
            .decode(&response.stderr_b64)
            .map_err(|e| format!("bad stderr_b64: {e}"))?;

        Ok(ExternalOutcome {
            stdout,
            stderr,
            exit_code: response.exit_code,
            timed_out: response.timed_out,
        })
    }

    async fn sync_to_remote(&self) -> Result<(), String> {
        let line = serde_json::to_string(&PushRequest { op: "push" }).map_err(|e| e.to_string())?;
        let response = self.round_trip(&line).await?;
        let ack: Ack = serde_json::from_str(&response)
            .map_err(|e| format!("bad JSON from the bridge: {e} (line: {response})"))?;
        if ack.ok {
            Ok(())
        } else {
            Err(format!(
                "the bridge shim could not sync: {}",
                ack.error.unwrap_or_else(|| "no reason given".into())
            ))
        }
    }
}

async fn write_line(stream: &mut BufReader<UnixStream>, line: &str) -> io::Result<()> {
    let inner = stream.get_mut();
    inner.write_all(line.as_bytes()).await?;
    inner.write_all(b"\n").await?;
    inner.flush().await
}

/// Read one newline-delimited line from `stream` (payload fields are base64
/// or JSON, so a raw newline cannot occur inside them).
async fn read_json_line(stream: &mut BufReader<UnixStream>) -> io::Result<String> {
    let mut buf = Vec::new();
    let n = stream.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the bridge socket closed before sending a response",
        ));
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The sha256 of every regular file under `root`, keyed by its path relative
/// to `root` (skipping `.git`). Used by this crate's live tests to verify a
/// sync round trip moved bytes exactly, not merely "some directory that
/// exists".
pub async fn hash_tree(root: &Path) -> std::collections::BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Some(entry) = entries.next_entry().await.expect("next_entry") {
            let path = entry.path();
            let file_type = entry.file_type().await.expect("file_type");
            if file_type.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                let bytes = tokio::fs::read(&path).await.expect("read");
                let digest = Sha256::digest(&bytes);
                let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, hex);
            }
        }
    }
    out
}
