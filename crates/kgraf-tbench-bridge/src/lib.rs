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
//! | `{"op":"hello","host_root":P,"resume":true}` | the same |
//! | `{"op":"exec","cmd":[..],"timeout_sec":f}` | stdout and stderr base64, `exit_code`, `timed_out`, `error` |
//! | `{"op":"push"}` | `{"ok":bool,"error":str or null}` |
//!
//! The shim keeps at most 8 MiB of each of stdout and stderr, the last bytes,
//! with a first line saying how many were cut, so an `exec` response always
//! fits the line bound ([`MAX_LINE_BYTES`]).
//!
//! The shim ends a command in the container at its `timeout_sec`, process
//! group and all, and stops a running command when its connection closes.
//!
//! # A broken connection
//!
//! A request whose answer was not read (it timed out, or its caller stopped
//! waiting) leaves the connection out of step: a late answer would be read as
//! the answer to the next request. That connection is closed, never read
//! again, and the next request opens a new one with `hello` and
//! `"resume": true`, which continues the same session on the shim (its sync
//! state for `host_root` is kept). If that fails, the request fails with both
//! the first failure and the reconnect's, and the next request tries again.
//!
//! # File sync
//!
//! The host worktree `host_root` and the container working directory are kept
//! equal by hash-checked deltas, done by the shim (see `bridge_server.py`):
//! before a command the host's changes are uploaded and verified by sha256 in
//! the container; after it the container's changes are downloaded and verified
//! by sha256 on the host, before the response returns. `push` runs only the
//! first half, after a host-side write. The protocol carries no file bytes.
use std::io;
use std::path::Path;
use std::time::Duration;

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
    /// The transport is a Unix domain socket, which this platform lacks.
    #[error(
        "the Terminal-Bench bridge is not supported on this platform: it talks to the shim \
         over a Unix domain socket"
    )]
    Unsupported,
}

/// How long the shim gets, beyond a command's own timeout, to sync files and
/// answer an `exec`.
pub const EXEC_SLACK: Duration = Duration::from_secs(120);
/// How long the shim gets to answer `hello` and `push`.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long past [`Limits::exec_slack`] the host waits for an `exec` (see
/// [`kgraf_tools::external::ExternalExecutor::report_grace`]), so the bridge's
/// own bound, with its clearer error, ends a request first.
pub const REPORT_MARGIN: Duration = Duration::from_secs(5);
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

/// The Unix-socket transport and the executor built on it.
#[cfg(unix)]
mod transport {
    use super::{BridgeError, Limits};
    use async_trait::async_trait;
    use kgraf_tools::external::{ExternalExecutor, ExternalOutcome};
    use serde::{Deserialize, Serialize};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;

    #[derive(Serialize)]
    struct HelloRequest<'a> {
        op: &'static str,
        host_root: &'a str,
        /// Continue the session of an earlier connection for `host_root`.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        resume: bool,
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
    /// to one Unix socket (one Python shim, one live task container) and one host
    /// tree.
    ///
    /// A request that times out, or whose caller stops waiting for it, leaves the
    /// stream out of step (a late response would be read as the answer to the
    /// next request), so that connection is closed and never read again; the
    /// next request connects again and resumes the session (see the crate
    /// docs). A response line over the bound is read to its end and dropped:
    /// that request fails and the connection stays usable. If the stream ends
    /// inside such a line, the connection is broken.
    pub struct ProcessExecutor {
        socket_path: PathBuf,
        host_root: PathBuf,
        /// `None` once a broken connection was closed.
        stream: Mutex<Option<BufReader<UnixStream>>>,
        limits: Limits,
        broken: AtomicBool,
        /// What broke the connection, named if connecting again fails.
        why_broken: std::sync::Mutex<String>,
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
            let stream = open(socket_path, host_root, limits, false).await?;
            Ok(Self {
                socket_path: socket_path.to_path_buf(),
                host_root: host_root.to_path_buf(),
                stream: Mutex::new(Some(stream)),
                limits,
                broken: AtomicBool::new(false),
                why_broken: std::sync::Mutex::new(String::new()),
            })
        }

        fn mark_broken(&self, why: &str) {
            mark_broken(&self.broken, &self.why_broken, why);
        }

        /// One request, one response line, under the connection lock, with the
        /// response due within `within`. A broken connection is replaced first.
        async fn round_trip(&self, line: &str, within: Duration) -> Result<String, String> {
            let mut slot = self.stream.lock().await;
            if self.broken.load(Ordering::Acquire) || slot.is_none() {
                // Closed, never read: a late answer on it belongs to a request
                // nobody waits for, and the shim stops that request's command
                // when it sees the close.
                *slot = None;
                let why = self
                    .why_broken
                    .lock()
                    .map(|w| w.clone())
                    .unwrap_or_default();
                match open(&self.socket_path, &self.host_root, self.limits, true).await {
                    Ok(stream) => {
                        *slot = Some(stream);
                        self.broken.store(false, Ordering::Release);
                    }
                    Err(e) => {
                        return Err(format!(
                            "the bridge connection broke ({why}), and connecting again failed: {e}"
                        ));
                    }
                }
            }
            let Some(stream) = slot.as_mut() else {
                return Err("the bridge connection is closed".to_owned());
            };
            // A caller that stops waiting (a cancel, the host's own bound)
            // drops this future between the request and its response. The
            // late response would then be read as the answer to the next
            // request, so a drop before the end breaks the connection.
            let pending = BreakOnDrop {
                broken: &self.broken,
                why_broken: &self.why_broken,
                armed: true,
            };
            let outcome = async {
                write_line(stream, line).await.map_err(|e| {
                    (
                        format!("writing the request to the bridge socket: {e}"),
                        false,
                    )
                })?;
                read_json_line_within(stream, within, self.limits.max_line_bytes)
                    .await
                    .map_err(|e| {
                        (
                            format!("reading the bridge response: {e}"),
                            is_oversized(&e),
                        )
                    })
            }
            .await;
            pending.disarm();
            // An oversized line that was read to its end leaves the stream in step,
            // so only that failure keeps the connection usable.
            match outcome {
                Ok(line) => Ok(line),
                Err((message, in_step)) => {
                    if !in_step {
                        self.mark_broken(&message);
                        *slot = None;
                    }
                    Err(message)
                }
            }
        }
    }

    /// Connect to the shim at `socket_path` and say `hello` for `host_root`;
    /// `resume` continues the session of an earlier connection.
    async fn open(
        socket_path: &Path,
        host_root: &Path,
        limits: Limits,
        resume: bool,
    ) -> Result<BufReader<UnixStream>, BridgeError> {
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
            resume,
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
        Ok(stream)
    }

    fn mark_broken(broken: &AtomicBool, why_broken: &std::sync::Mutex<String>, why: &str) {
        if let Ok(mut w) = why_broken.lock() {
            why.clone_into(&mut w);
        }
        broken.store(true, Ordering::Release);
    }

    /// Marks a connection broken when dropped while armed: a request whose
    /// response was never read.
    struct BreakOnDrop<'a> {
        broken: &'a AtomicBool,
        why_broken: &'a std::sync::Mutex<String>,
        armed: bool,
    }

    impl BreakOnDrop<'_> {
        fn disarm(mut self) {
            self.armed = false;
        }
    }

    impl Drop for BreakOnDrop<'_> {
        fn drop(&mut self) {
            if self.armed {
                mark_broken(
                    self.broken,
                    self.why_broken,
                    "an earlier request was given up before its answer arrived",
                );
            }
        }
    }

    #[async_trait]
    impl ExternalExecutor for ProcessExecutor {
        /// The connection is broken already (the dropped request marked it).
        /// Closing it now makes the shim stop the abandoned command at once;
        /// the next request connects again.
        async fn abandon(&self) {
            if !self.broken.load(Ordering::Acquire) {
                self.mark_broken("an earlier request was abandoned");
            }
            if let Ok(mut slot) = self.stream.try_lock()
                && let Some(mut stream) = slot.take()
            {
                let _ = stream.get_mut().shutdown().await;
            }
        }

        /// The shim may take its own slack past the command's timeout to sync
        /// the tree and answer; the host waits a little longer than that.
        fn report_grace(&self) -> Duration {
            self.limits.exec_slack.saturating_add(super::REPORT_MARGIN)
        }

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
            let line =
                serde_json::to_string(&PushRequest { op: "push" }).map_err(|e| e.to_string())?;
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
    pub(super) async fn read_json_line(
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
            // Read on to the end of the line without keeping it, so the next
            // response is read from its own start. Only when the newline arrives
            // is the stream still in step and the error an [`Oversized`] one.
            let in_step = discard_to_newline(stream).await?;
            return Err(if in_step {
                io::Error::new(io::ErrorKind::InvalidData, Oversized { max_bytes })
            } else {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "the bridge response line is longer than {max_bytes} bytes and the socket \
                         closed before it ended"
                    ),
                )
            });
        }
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// A response line longer than the bound whose end was reached and dropped:
    /// this request failed, and the connection is in step for the next.
    #[derive(Debug)]
    struct Oversized {
        max_bytes: usize,
    }

    impl std::fmt::Display for Oversized {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "the bridge response line is longer than {} bytes; it was discarded",
                self.max_bytes
            )
        }
    }

    impl std::error::Error for Oversized {}

    fn is_oversized(e: &io::Error) -> bool {
        e.get_ref().is_some_and(|inner| inner.is::<Oversized>())
    }

    /// Consume input up to and including the next newline. `false` when the
    /// stream ended first.
    async fn discard_to_newline(stream: &mut BufReader<UnixStream>) -> io::Result<bool> {
        loop {
            let chunk = stream.fill_buf().await?;
            if chunk.is_empty() {
                return Ok(false);
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    stream.consume(i + 1);
                    return Ok(true);
                }
                None => {
                    let all = chunk.len();
                    stream.consume(all);
                }
            }
        }
    }
}

#[cfg(unix)]
pub use transport::ProcessExecutor;

/// The executor is a Unix-socket client. On this platform no connection can be
/// made: [`ProcessExecutor::connect`] fails with [`BridgeError::Unsupported`],
/// so a run that asks for the bridge stops with that error instead of running
/// without it.
#[cfg(not(unix))]
pub struct ProcessExecutor {
    never: std::convert::Infallible,
}

#[cfg(not(unix))]
impl ProcessExecutor {
    /// Always fails with [`BridgeError::Unsupported`].
    pub async fn connect(_socket_path: &Path, _host_root: &Path) -> Result<Self, BridgeError> {
        Err(BridgeError::Unsupported)
    }

    /// Always fails with [`BridgeError::Unsupported`].
    pub async fn connect_with_limits(
        _socket_path: &Path,
        _host_root: &Path,
        _limits: Limits,
    ) -> Result<Self, BridgeError> {
        Err(BridgeError::Unsupported)
    }
}

#[cfg(not(unix))]
#[async_trait::async_trait]
impl kgraf_tools::external::ExternalExecutor for ProcessExecutor {
    async fn exec(
        &self,
        _cmd: &[String],
        _timeout: Duration,
    ) -> Result<kgraf_tools::external::ExternalOutcome, String> {
        match self.never {}
    }

    async fn sync_to_remote(&self) -> Result<(), String> {
        match self.never {}
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::transport::*;
    use super::*;
    use kgraf_tools::external::ExternalExecutor;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

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

    /// One connection of a fake shim that serves several in turn.
    type Conn = Box<
        dyn FnOnce(
                BufReader<UnixStream>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send,
    >;

    /// A fake shim that hands its `n`th accepted connection to `conns[n]`,
    /// hello included, and stops listening (the socket file removed) after
    /// the last one.
    async fn shim_conns(conns: Vec<Conn>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("bridge.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let socket = path.clone();
        tokio::spawn(async move {
            for conn in conns {
                let (stream, _) = listener.accept().await.expect("accept");
                tokio::spawn(conn(BufReader::new(stream)));
            }
            drop(listener);
            let _ = std::fs::remove_file(&socket);
        });
        (dir, path)
    }

    /// Read the hello line and answer it with `ack`; the hello as sent.
    async fn hello(stream: &mut BufReader<UnixStream>, ack: &[u8]) -> serde_json::Value {
        let mut line = String::new();
        stream.read_line(&mut line).await.expect("hello");
        stream.get_mut().write_all(ack).await.expect("ack");
        serde_json::from_str(&line).expect("hello is JSON")
    }

    fn exec_answer(stdout: &str) -> Vec<u8> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(stdout);
        format!(
            "{{\"stdout_b64\":\"{b64}\",\"stderr_b64\":\"\",\"exit_code\":0,\"timed_out\":false,\"error\":null}}\n"
        )
        .into_bytes()
    }

    /// The first connection reads an `exec` and answers it only after the
    /// client gave up; it reports whether the client then closed it. The
    /// second must be a `resume` hello and answers an `exec` with `second`.
    fn late_then_resumed(
        closed: tokio::sync::oneshot::Sender<bool>,
        resumed: tokio::sync::oneshot::Sender<serde_json::Value>,
    ) -> Vec<Conn> {
        vec![
            Box::new(move |mut stream| {
                Box::pin(async move {
                    hello(&mut stream, OK).await;
                    let mut line = String::new();
                    let _ = stream.read_line(&mut line).await;
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let _ = stream.get_mut().write_all(&exec_answer("late")).await;
                    line.clear();
                    let eof =
                        tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line))
                            .await
                            .is_ok_and(|n| n.is_ok_and(|n| n == 0));
                    let _ = closed.send(eof);
                })
            }),
            Box::new(move |mut stream| {
                Box::pin(async move {
                    let sent = hello(&mut stream, OK).await;
                    let _ = resumed.send(sent);
                    let mut line = String::new();
                    while stream.read_line(&mut line).await.is_ok_and(|n| n > 0) {
                        let _ = stream.get_mut().write_all(&exec_answer("second")).await;
                        line.clear();
                    }
                })
            }),
        ]
    }

    /// A request that times out breaks its connection, and the next request
    /// is served on a new one: it connects again with a `resume` hello, gets
    /// its own answer (never the first request's late one), and the first
    /// connection is closed, so the shim can stop the command it still runs.
    #[tokio::test]
    async fn after_a_timed_out_request_the_next_one_reconnects_and_is_answered() {
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let (resumed_tx, resumed_rx) = tokio::sync::oneshot::channel();
        let (_dir, path) = shim_conns(late_then_resumed(closed_tx, resumed_tx)).await;
        let exec =
            ProcessExecutor::connect_with_limits(&path, Path::new("/x"), small(150, 1 << 20))
                .await
                .expect("connect");
        let started = std::time::Instant::now();
        let err = exec
            .exec(
                &["sleep".to_owned(), "60".to_owned()],
                Duration::from_millis(50),
            )
            .await
            .expect_err("no answer in time");
        assert!(err.contains("no response line"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let next = exec
            .exec(&["true".to_owned()], Duration::from_secs(5))
            .await
            .expect("the next request is served on a new connection");
        assert_eq!(next.stdout, b"second");
        let sent = resumed_rx.await.expect("a second connection");
        assert_eq!(sent["resume"], serde_json::json!(true), "{sent}");
        assert_eq!(sent["host_root"], serde_json::json!("/x"), "{sent}");
        assert!(
            closed_rx.await.expect("first connection"),
            "the broken connection was not closed"
        );
    }

    /// A caller that stops waiting drops an `exec` before its response. The
    /// late response must not answer the next request: `abandon` closes the
    /// connection, and the next request is answered on a new one.
    #[tokio::test]
    async fn a_dropped_request_is_never_answered_by_its_late_response() {
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let (resumed_tx, _resumed_rx) = tokio::sync::oneshot::channel();
        let (_dir, path) = shim_conns(late_then_resumed(closed_tx, resumed_tx)).await;
        let exec = ProcessExecutor::connect(&path, Path::new("/x"))
            .await
            .expect("connect");
        let dropped = tokio::time::timeout(
            Duration::from_millis(50),
            exec.exec(&["true".to_owned()], Duration::from_secs(1)),
        )
        .await;
        assert!(dropped.is_err(), "the exec must still be waiting");
        exec.abandon().await;
        assert!(
            closed_rx.await.expect("first connection"),
            "abandon did not close it"
        );
        let next = exec
            .exec(&["true".to_owned()], Duration::from_secs(5))
            .await
            .expect("served on a new connection");
        assert_eq!(
            next.stdout, b"second",
            "a late answer was read as this one's"
        );
    }

    /// When connecting again fails, the error names the first failure and
    /// the reconnect's, and a later request tries again.
    #[tokio::test]
    async fn a_failed_reconnect_names_both_failures() {
        let silent: Conn = Box::new(|mut stream| {
            Box::pin(async move {
                hello(&mut stream, OK).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
        });
        let refusing: Conn = Box::new(|mut stream| {
            Box::pin(async move {
                hello(
                    &mut stream,
                    b"{\"ok\":false,\"error\":\"no earlier session for '/x' on this server to resume\"}\n",
                )
                .await;
            })
        });
        let (_dir, path) = shim_conns(vec![silent, refusing]).await;
        let exec =
            ProcessExecutor::connect_with_limits(&path, Path::new("/x"), small(150, 1 << 20))
                .await
                .expect("connect");
        let first = exec
            .exec(&["true".to_owned()], Duration::from_millis(50))
            .await
            .expect_err("no answer");
        assert!(first.contains("no response line"), "{first}");
        let refused = exec.sync_to_remote().await.expect_err("resume refused");
        assert!(
            refused.contains("no response line")
                && refused.contains("connecting again failed")
                && refused.contains("no earlier session"),
            "{refused}"
        );
        // The shim is gone now: the next request tries again and says so.
        let gone = exec.sync_to_remote().await.expect_err("no shim");
        assert!(
            gone.contains("no response line") && gone.contains("cannot connect"),
            "{gone}"
        );
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

    /// A response line over the bound fails its own request, is read to its
    /// end, and the next request is answered from the right place.
    #[tokio::test]
    async fn an_oversized_line_fails_one_request_and_keeps_the_connection() {
        let (_dir, path) = shim(|mut stream| async move {
            let mut line = String::new();
            let _ = stream.read_line(&mut line).await;
            let mut big = vec![b'a'; 5000];
            big.push(b'\n');
            let _ = stream.get_mut().write_all(&big).await;
            line.clear();
            let _ = stream.read_line(&mut line).await;
            let _ = stream.get_mut().write_all(OK).await;
        })
        .await;
        let exec = ProcessExecutor::connect_with_limits(&path, Path::new("/x"), small(5_000, 1000))
            .await
            .expect("connect");
        let err = exec.sync_to_remote().await.expect_err("too long");
        assert!(err.contains("longer than 1000 bytes"), "{err}");
        exec.sync_to_remote()
            .await
            .expect("the next response is read from its own start");
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
