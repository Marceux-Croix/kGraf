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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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

/// How long the shim gets, beyond a command's own timeout, to sync files and
/// answer an `exec`.
pub const EXEC_SLACK: Duration = Duration::from_secs(120);
/// How long the shim gets to answer `hello` and `push`.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest response line accepted. A response carries a command's stdout
/// and stderr in base64, so the bound is generous, and it exists so a shim
/// that never sends a newline cannot grow this process without limit.
pub const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// The bounds on one conversation with the shim.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Added to an `exec`'s own timeout.
    pub exec_slack: Duration,
    /// For `hello` and `push`.
    pub control_timeout: Duration,
    /// The longest response line.
    pub max_line_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            exec_slack: EXEC_SLACK,
            control_timeout: CONTROL_TIMEOUT,
            max_line_bytes: MAX_LINE_BYTES,
        }
    }
}

/// The concrete, subprocess-bridged [`ExternalExecutor`]. One instance is bound
/// to one Unix socket (one Python shim, one live task container).
///
/// A request that times out or whose response is too long leaves the stream
/// out of step (a late response would be read as the answer to the next
/// request), so the connection is marked broken and every later request fails
/// at once, naming the first failure.
pub struct ProcessExecutor {
    stream: Mutex<BufReader<UnixStream>>,
    limits: Limits,
    broken: AtomicBool,
}

impl ProcessExecutor {
    /// Connect to a Python shim already listening at `socket_path`, and tell
    /// it which host directory to keep equal to the container's working
    /// directory (`host_root`: the worker's own worktree). The shim checks
    /// that the two trees are equal now; if they are not it refuses, and so
    /// does this call.
    pub async fn connect(socket_path: &Path, host_root: &Path) -> Result<Self, BridgeError> {
        Self::connect_with_limits(socket_path, host_root, Limits::default()).await
    }

    /// [`Self::connect`], with the timeouts and the line bound given.
    pub async fn connect_with_limits(
        socket_path: &Path,
        host_root: &Path,
        limits: Limits,
    ) -> Result<Self, BridgeError> {
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
        let response =
            read_json_line_within(&mut stream, limits.control_timeout, limits.max_line_bytes)
                .await
                .map_err(connect_err)?;
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
            limits,
            broken: AtomicBool::new(false),
        })
    }

    /// One request, one response line, under the connection lock, with the
    /// response due within `within`.
    async fn round_trip(&self, line: &str, within: Duration) -> Result<String, String> {
        let mut stream = self.stream.lock().await;
        if self.broken.load(Ordering::Acquire) {
            return Err("the bridge connection is broken by an earlier timeout or \
                        oversized response"
                .to_owned());
        }
        let outcome = async {
            write_line(&mut stream, line)
                .await
                .map_err(|e| format!("writing the request to the bridge socket: {e}"))?;
            read_json_line_within(&mut stream, within, self.limits.max_line_bytes)
                .await
                .map_err(|e| format!("reading the bridge response: {e}"))
        }
        .await;
        if outcome.is_err() {
            self.broken.store(true, Ordering::Release);
        }
        outcome
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
        let within = timeout.saturating_add(self.limits.exec_slack);
        let response = self.round_trip(&line, within).await?;
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
        let response = self.round_trip(&line, self.limits.control_timeout).await?;
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

/// [`read_json_line`], due within `within`, at most `max_bytes` long.
async fn read_json_line_within(
    stream: &mut BufReader<UnixStream>,
    within: Duration,
    max_bytes: usize,
) -> io::Result<String> {
    tokio::time::timeout(within, read_json_line(stream, max_bytes))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no response line within {within:?}"),
            )
        })?
}

/// Read one newline-delimited line from `stream` (payload fields are base64
/// or JSON, so a raw newline cannot occur inside them). Stops with an error
/// once `max_bytes` have arrived without a newline.
async fn read_json_line(
    stream: &mut BufReader<UnixStream>,
    max_bytes: usize,
) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut limited = (&mut *stream).take(max_bytes as u64 + 1);
    let n = limited.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the bridge socket closed before sending a response",
        ));
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the bridge response line is longer than {max_bytes} bytes"),
        ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    const OK: &[u8] = b"{\"ok\":true,\"error\":null}\n";

    /// A fake shim: answers `hello`, then hands the connection to `after`.
    async fn shim<F, Fut>(after: F) -> (tempfile::TempDir, std::path::PathBuf)
    where
        F: FnOnce(BufReader<UnixStream>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("bridge.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.expect("hello");
            stream.get_mut().write_all(OK).await.expect("ack");
            after(stream).await;
        });
        (dir, path)
    }

    fn small(slack_ms: u64, max_line: usize) -> Limits {
        Limits {
            exec_slack: Duration::from_millis(slack_ms),
            control_timeout: Duration::from_millis(slack_ms),
            max_line_bytes: max_line,
        }
    }

    /// A shim that reads a request and never answers: the call ends at the
    /// bound, and the connection then refuses further requests at once.
    #[tokio::test]
    async fn a_silent_shim_times_out_and_breaks_the_connection() {
        let (_dir, path) = shim(|mut stream| async move {
            let mut line = String::new();
            let _ = stream.read_line(&mut line).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
        .await;
        let exec =
            ProcessExecutor::connect_with_limits(&path, Path::new("/x"), small(150, 1 << 20))
                .await
                .expect("connect");
        let started = std::time::Instant::now();
        let err = exec
            .exec(&["true".to_owned()], Duration::from_millis(50))
            .await
            .expect_err("no answer");
        assert!(err.contains("no response line"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let again = exec.sync_to_remote().await.expect_err("broken");
        assert!(again.contains("broken"), "{again}");
    }

    /// A shim that streams bytes with no newline is cut off at the bound.
    #[tokio::test]
    async fn an_endless_line_is_refused_at_the_bound() {
        let (_dir, path) = shim(|mut stream| async move {
            let mut line = String::new();
            let _ = stream.read_line(&mut line).await;
            let chunk = vec![b'a'; 4096];
            for _ in 0..64 {
                if stream.get_mut().write_all(&chunk).await.is_err() {
                    return;
                }
            }
        })
        .await;
        let exec = ProcessExecutor::connect_with_limits(&path, Path::new("/x"), small(5_000, 1000))
            .await
            .expect("connect");
        let err = exec.sync_to_remote().await.expect_err("too long");
        assert!(err.contains("longer than 1000 bytes"), "{err}");
    }

    /// The default limits leave an ordinary exchange alone.
    #[tokio::test]
    async fn an_ordinary_exchange_still_works() {
        let (_dir, path) = shim(|mut stream| async move {
            let mut line = String::new();
            let _ = stream.read_line(&mut line).await;
            let _ = stream.get_mut().write_all(OK).await;
        })
        .await;
        let exec = ProcessExecutor::connect(&path, Path::new("/x"))
            .await
            .expect("connect");
        exec.sync_to_remote().await.expect("pushed");
    }

    /// The bound is exact: a line of `max` bytes passes, one more does not.
    #[tokio::test]
    async fn the_line_bound_is_exact() {
        for (len, ok) in [(100usize, true), (101, false)] {
            let (a, mut b) = UnixStream::pair().expect("pair");
            let mut line = vec![b'x'; len];
            line.push(b'\n');
            b.write_all(&line).await.expect("write");
            let mut reader = BufReader::new(a);
            let got = read_json_line(&mut reader, 100).await;
            assert_eq!(got.is_ok(), ok, "len {len}: {got:?}");
        }
    }
}
