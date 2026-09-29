//! Attribution for every remote MCP call (decision 0051, main-session ruling: a
//! remote MCP call is attributed as a provider call is, the accounting rule's sibling for
//! non-token egress).
//!
//! [`McpAttributionSink`] is required **at construction**, not merely available:
//! [`crate::client::RmcpCaller`] and [`crate::oauth::OAuthConnector`] cannot be
//! built without one, so there is no code path on which MCP traffic -- a
//! connection, `tools/list`, `tools/call`, or OAuth discovery/registration/token
//! exchange -- can occur unattributed. [`LedgerAttributionSink`] writes each
//! event to the accounting ledger as a chained `mcp` record
//! (`kgraf_provider::Ledger::record_mcp_call`), and also emits it as a
//! structured `tracing` event. [`TracingAttributionSink`] emits only the
//! `tracing` event. [`InMemoryAttributionSink`] is for tests.

use kgraf_core::{AgentId, TaskId};
use kgraf_provider::{Ledger, McpCall};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What kind of MCP traffic one event records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpCallKind {
    /// Establishing the connection (the `initialize` handshake).
    Connect,
    /// A `tools/list` round trip.
    ListTools,
    /// A `tools/call` round trip.
    CallTool,
    /// OAuth discovery, registration, token exchange or refresh traffic.
    OAuth,
    /// Opening the Streamable HTTP GET stream: the first open, each reopen
    /// after the server closed it, and the refusal once the session's reopen
    /// budget runs out (outcome `ProtocolError`, no request sent).
    OpenStream,
}

/// How one piece of MCP traffic ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpCallOutcome {
    /// The protocol call itself succeeded. For `CallTool` this covers both a
    /// tool-level success and a tool-level error result (`CallToolResult { is_error:
    /// true, .. }`): the server answered, which is what this variant records: use
    /// `tool_is_error` on the event for the finer distinction.
    Success,
    /// The protocol call failed (a transport error, a JSON-RPC error response).
    ProtocolError,
    /// The call did not finish within its timeout.
    TimedOut,
    /// The call was cancelled by the worker's own cancellation token before it
    /// finished.
    Cancelled,
}

impl McpCallKind {
    /// The stable name stored in the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            McpCallKind::Connect => "connect",
            McpCallKind::ListTools => "list_tools",
            McpCallKind::CallTool => "call_tool",
            McpCallKind::OAuth => "oauth",
            McpCallKind::OpenStream => "open_stream",
        }
    }
}

impl McpCallOutcome {
    /// The stable name stored in the ledger.
    pub fn as_str(&self) -> &'static str {
        match self {
            McpCallOutcome::Success => "success",
            McpCallOutcome::ProtocolError => "protocol_error",
            McpCallOutcome::TimedOut => "timed_out",
            McpCallOutcome::Cancelled => "cancelled",
        }
    }
}

/// One attributed piece of MCP traffic.
#[derive(Clone, Debug)]
pub struct McpCallEvent {
    /// The server's own name (`McpServerConfig::name`).
    pub server: String,
    /// The tool called, for `CallTool` events.
    pub tool: Option<String>,
    /// The worker that made the call, where one exists (OAuth traffic run ahead of
    /// any worker dispatch has none).
    pub worker: Option<AgentId>,
    /// The task the worker was on.
    pub task: Option<TaskId>,
    pub kind: McpCallKind,
    pub duration: Duration,
    pub outcome: McpCallOutcome,
    /// Set only for a `CallTool` `Success`: whether the tool result itself was a
    /// tool-level error (`CallToolResult::is_error`).
    pub tool_is_error: Option<bool>,
    /// Bytes sent to the server (the serialized request), where known.
    pub bytes_in: u64,
    /// Bytes received from the server (the serialized response), where known.
    pub bytes_out: u64,
}

/// Records attributed MCP traffic. [`LedgerAttributionSink`] is the production
/// implementation: every event becomes a chained ledger row.
pub trait McpAttributionSink: Send + Sync {
    fn record(&self, event: McpCallEvent);
}

/// Emits each event as a structured `tracing` record (`target:
/// "kgraf_mcp::attribution"`). [`TracingAttributionSink`] emits only this;
/// [`LedgerAttributionSink`] emits it beside the ledger row, so attribution is
/// visible in logs as well.
pub fn emit_tracing_event(event: &McpCallEvent) {
    tracing::info!(
        target: "kgraf_mcp::attribution",
        server = %event.server,
        tool = event.tool.as_deref().unwrap_or(""),
        worker = event.worker.as_ref().map(ToString::to_string).unwrap_or_default(),
        task = event.task.as_ref().map(ToString::to_string).unwrap_or_default(),
        kind = ?event.kind,
        duration_ms = event.duration.as_millis() as u64,
        outcome = ?event.outcome,
        tool_is_error = event.tool_is_error,
        bytes_in = event.bytes_in,
        bytes_out = event.bytes_out,
        "mcp_call"
    );
}

/// A sink that only emits the `tracing` event above: no ledger row. For tools
/// and tests with no ledger; a worker uses [`LedgerAttributionSink`].
#[derive(Debug, Default)]
pub struct TracingAttributionSink;

impl McpAttributionSink for TracingAttributionSink {
    fn record(&self, event: McpCallEvent) {
        emit_tracing_event(&event);
    }
}

/// Writes every event to the accounting ledger (decision 0051: a remote MCP call
/// is attributed as a provider call is), and emits the `tracing` event too.
///
/// **Bound to one worker and task.** A worker's MCP connections are opened for
/// that worker, so the `connect` and `list_tools` exchanges the client records
/// with no worker (they happen before any tool call) are attributed to the
/// worker and task this sink was built for. An event that names its own worker
/// keeps it.
///
/// **A failed write is not silent (fail loud).** [`McpAttributionSink::record`]
/// cannot return an error: the exchange it describes has already happened. A
/// ledger write that fails is logged at `error` and counted; [`Self::failures`]
/// reports the count and [`Self::check`] turns a nonzero count into an error,
/// which the caller runs before it reports the run as finished.
pub struct LedgerAttributionSink {
    ledger: Arc<Ledger>,
    worker: Option<AgentId>,
    task: Option<TaskId>,
    failures: AtomicU64,
}

/// The ledger refused one or more MCP records.
#[derive(Debug, thiserror::Error)]
#[error("{failures} MCP exchange(s) could not be written to the ledger; see the error log")]
pub struct AttributionLost {
    pub failures: u64,
}

impl LedgerAttributionSink {
    /// A sink attributing unowned exchanges to `worker` and `task`.
    pub fn for_worker(ledger: Arc<Ledger>, worker: AgentId, task: TaskId) -> Self {
        Self {
            ledger,
            worker: Some(worker),
            task: Some(task),
            failures: AtomicU64::new(0),
        }
    }

    /// A sink for traffic outside any worker (an OAuth flow the user started).
    pub fn unowned(ledger: Arc<Ledger>) -> Self {
        Self {
            ledger,
            worker: None,
            task: None,
            failures: AtomicU64::new(0),
        }
    }

    /// Ledger writes that failed so far.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::SeqCst)
    }

    /// `Err` if any ledger write failed.
    pub fn check(&self) -> Result<(), AttributionLost> {
        match self.failures() {
            0 => Ok(()),
            failures => Err(AttributionLost { failures }),
        }
    }

    fn to_ledger(&self, event: &McpCallEvent) -> McpCall {
        let worker = event.worker.as_ref().or(self.worker.as_ref());
        let task = event.task.as_ref().or(self.task.as_ref());
        McpCall {
            server: event.server.clone(),
            tool: event.tool.clone(),
            kind: event.kind.as_str().to_owned(),
            agent_id: worker.map(|w| w.as_str().to_owned()),
            task_id: task.map(|t| t.as_str().to_owned()),
            duration_ms: i64::try_from(event.duration.as_millis()).unwrap_or(i64::MAX),
            outcome: event.outcome.as_str().to_owned(),
            tool_is_error: event.tool_is_error,
            bytes_in: i64::try_from(event.bytes_in).unwrap_or(i64::MAX),
            bytes_out: i64::try_from(event.bytes_out).unwrap_or(i64::MAX),
        }
    }
}

impl McpAttributionSink for LedgerAttributionSink {
    fn record(&self, event: McpCallEvent) {
        emit_tracing_event(&event);
        let call = self.to_ledger(&event);
        // The ledger's writer thread commits the row; this waits for that commit,
        // off the executor thread when there is another to hand tasks to. The
        // wait stays synchronous so `failures` is exact when `check` runs.
        let written = off_executor(|| self.ledger.record_mcp_call(&call));
        if let Err(error) = written {
            self.failures.fetch_add(1, Ordering::SeqCst);
            tracing::error!(
                target: "kgraf_mcp::attribution",
                server = %event.server,
                kind = event.kind.as_str(),
                %error,
                "an MCP exchange could not be written to the ledger"
            );
        }
    }
}

/// `block_in_place` on a multi-thread runtime; inline elsewhere (a
/// current-thread runtime has no other thread to move its tasks to).
fn off_executor<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Collects events in memory. For tests: assert on `events()` rather than parsing
/// log output.
#[derive(Debug, Default)]
pub struct InMemoryAttributionSink {
    events: Mutex<Vec<McpCallEvent>>,
}

impl InMemoryAttributionSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<McpCallEvent> {
        self.events
            .lock()
            .expect("attribution sink mutex poisoned")
            .clone()
    }
}

impl McpAttributionSink for InMemoryAttributionSink {
    fn record(&self, event: McpCallEvent) {
        self.events
            .lock()
            .expect("attribution sink mutex poisoned")
            .push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(worker: Option<&str>, kind: McpCallKind) -> McpCallEvent {
        McpCallEvent {
            server: "fs".into(),
            tool: (kind == McpCallKind::CallTool).then(|| "read".to_owned()),
            worker: worker.map(AgentId::from),
            task: worker.map(|_| TaskId::from("own-task")),
            kind,
            duration: Duration::from_millis(7),
            outcome: McpCallOutcome::Success,
            tool_is_error: (kind == McpCallKind::CallTool).then_some(false),
            bytes_in: 10,
            bytes_out: 20,
        }
    }

    #[test]
    fn the_ledger_sink_writes_chained_rows_and_fills_the_owner_of_unowned_traffic() {
        let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
        let sink = LedgerAttributionSink::for_worker(ledger.clone(), "w1".into(), "t1".into());
        sink.record(event(None, McpCallKind::Connect));
        sink.record(event(Some("w2"), McpCallKind::CallTool));
        let rows = ledger.mcp_calls().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].call.kind, "connect");
        assert_eq!(rows[0].call.agent_id.as_deref(), Some("w1"));
        assert_eq!(rows[0].call.task_id.as_deref(), Some("t1"));
        // An event that names its own worker keeps it.
        assert_eq!(rows[1].call.agent_id.as_deref(), Some("w2"));
        assert_eq!(rows[1].call.task_id.as_deref(), Some("own-task"));
        assert_eq!(rows[1].call.tool.as_deref(), Some("read"));
        assert_eq!(rows[1].call.duration_ms, 7);
        assert_eq!((rows[1].call.bytes_in, rows[1].call.bytes_out), (10, 20));
        assert!(ledger.verify().expect("verify").ok);
        sink.check().expect("no failures");
    }
}
