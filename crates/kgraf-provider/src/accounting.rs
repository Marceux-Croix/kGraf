//! The accounting ledger.
//!
//! The accounting rule: **no provider call may be issued outside the accounting path.** That is
//! usually a convention checked in review. Here it is a type rule.
//!
//! [`crate::Provider::complete`] requires a `&CallToken`. `CallToken` has no public
//! constructor, so the only way to obtain one is [`Ledger::begin`], which writes the
//! call's row before returning it. A code path that reaches a provider without
//! being counted does not compile, so the rule survives every future edit,
//! including edits made by kGraf's own workers, which is the point.
//!
//! The token is also a guard. Dropping it without calling [`CallToken::settle`]
//! leaves the row **unreconciled**, and [`Ledger::unreconciled`] reports those.
//! That is how the interrupted-stream case is handled: one provider states plainly
//! that an interrupted stream may never deliver its usage chunk, so a request that
//! billed but was not counted must be visible as a hole with its request id, not as
//! spend that quietly vanished.
//!
//! What is stored is the **six-field normal form plus the raw usage blob verbatim**
//! (decision 0006). Cost is not a column: it is derived at read time from a price
//! table, so a pricing change re-derives history instead of invalidating it.
//!
//! **Decision 0059: the ledger is hash-chained, event by event.** The first version of this file had eight ways to defeat a
//! single-digest-per-row chain (the `atk_*` fixtures below are regression tests
//! for them). The design here answers each of them directly:
//!
//! - Every **mutation** (`begin`, `settle` (used for settle/fail/cancel/leave
//!   unreconciled alike), `reconcile`, `redact`, and `mcp` (one recorded MCP
//!   exchange, decision 0051; [`Ledger::record_mcp_call`])) appends its own
//!   immutable `ledger_chain` row, never just the first one. A legitimate `reconcile` after
//!   an unreconciled `settle` is its own event, so it does not invalidate the
//!   settle event's digest (`atk_reconcile_breaks_chain`).
//! - [`Ledger::verify`] recomputes each settled call's digest from its **live**
//!   row and compares it to that call's **latest** chain event, rather than
//!   trusting a `redacted` flag to mean "skip the check"
//!   (`atk_redacted_flag_hides_edit`). Redaction clears only `raw_usage` and is
//!   itself a chained event with its own digest, so an edit made after a
//!   legitimate redaction still mismatches that event's digest
//!   (`atk_redact_then_edit`).
//! - The digest is computed from the **raw stored bytes** of every text and
//!   integer column, never from a value already parsed into `Settlement`,
//!   `Protocol` or JSON. The old code hashed the *parsed* enum, so an unparsable
//!   settlement string and a garbage `raw_usage` blob both collapsed onto the
//!   same fallback value as a legitimate row and left the digest unchanged
//!   (`atk_garbage_raw_and_settlement`).
//! - Because SQLite's raw usage blob is what redaction exists to delete, the
//!   digest never hashes it directly; it hashes `raw_hash`, a `blake3` digest of
//!   the blob stored the moment usage is recorded and left untouched by
//!   redaction. That is what lets `verify` bind a redacted row to its
//!   pre-redaction content without keeping a second copy of the secret.
//! - The one-time migration that back-fills a pre-chain database is gated on
//!   `PRAGMA user_version`, run at most once ever. After that, `verify` reports
//!   any settled row with **no** chain event as forged, nothing re-backfills a
//!   hand-inserted row into looking legitimate on a later reopen
//!   (`atk_inserted_row_laundered_by_backfill`).
//! - Trace writes ([`CallToken::record_first_token_async`],
//!   [`CallToken::record_tool_round_async`]) are refused once a row is no longer
//!   `in_flight`, so a call cannot be mutated after the event that chained it
//!   (`atk_trace_after_settle`).
//! - Finally, a plain hash chain cannot detect its own tail being cut off:
//!   deleting a call's row and every one of its chain events removes the
//!   sequence gap along with the evidence (`atk_tail_truncation`), and rebuilding
//!   a self-consistent chain from scratch after resetting SQLite's own
//!   `sqlite_sequence` counter passes every in-database check
//!   (`atk_recompute_all`). Both are answered the way git's own reflog answers
//!   `git commit --amend --no-edit` on a rewritten history: every committed writer
//!   batch publishes its last `(seq, chain_hash)` to [`AnchorWriter`] (after the
//!   commit, so the anchor never names a sequence that could still roll back),
//!   which durably writes it to a
//!   **separate** `<ledger>.tip` file (or, for an in-memory ledger, an in-process
//!   anchor). The write is fsynced, via a temp-file-then-rename so it is atomic,
//!   **off the settle path**. `verify` compares the chain against that anchor
//!   independently of anything stored in the database itself.
//!
//! **The anchor write moved off the settle path (after this file's first version), because it fsynced inline and measured at
//! ~5 ms per settled call on disk, enough to break the per-turn loop budget.** [`AnchorWriter`] runs on its own thread, coalescing
//! (`publish` only ever keeps the latest tip; a burst of settles under load
//! costs one write, not one per settle) and never blocking the caller.
//! **This narrows what `verify` can prove, and the narrowing is exact, not
//! approximate: an event chained after the anchor writer's last completed
//! write is not yet covered by the anchor, until the next write lands or
//! [`Ledger::flush_anchor`] (or `Drop`) forces one.** `verify` accordingly
//! accepts an anchor that lags the chain, `anchor.seq` at or behind the
//! chain's tail, as long as the row actually at `anchor.seq` still carries the
//! hash the anchor recorded, and rejects only an anchor naming a sequence
//! that no longer exists (truncation) or whose hash has changed (rewrite). See
//! the comment on `Ledger::verify`'s pass 4 for the exact accept/reject rule.
//!
//! **What `verify` can and cannot prove, stated plainly per the ANCHOR
//! requirement.** It proves that the `calls` table and `ledger_chain` table agree
//! with each other, and that neither has drifted from the last externally
//! anchored tip this process (or an earlier one, for an on-disk ledger) durably
//! recorded, **not necessarily the current tip**, per the lag window above. It
//! cannot prove anything about events chained since that last anchor write, and
//! it cannot prove anything against an attacker who rewrites **both** the
//! database file and the `.tip` file consistently with each other. That
//! adversary is out of scope, exactly as a git reflog does not defend against an
//! attacker who also rewrites the reflog.
//!
//! **After a power loss, `verify` may report a truncation that did not
//! happen.** The `.tip` file and the database are flushed with `fsync(2)`,
//! which on macOS does not flush the drive's write cache (see [`LedgerAck`]).
//! If the power fails, the drive may keep the newer anchor and lose the last
//! commits it names, and `verify` then reports the anchored sequence as
//! truncated. `verify`'s logic is unchanged for this; decision 0059 records the
//! limit.
//!
//! **Who writes (2026-09-26).** One writer thread owns every mutation and
//! group-commits whatever is queued; reads run on separate read-only WAL
//! connections. [`Ledger`]'s doc comment states what that preserves; the chain,
//! redaction, the anchor, back-fill gating and `verify` above are unchanged by it.

use kgraf_core::cost::{ExactMoney, Money, PriceTable};
use kgraf_core::{CallId, Protocol, TokenUsage};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Ledger failures.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The ledger database refused an operation.
    #[error("ledger database error")]
    Sqlite(#[from] rusqlite::Error),

    /// A call was settled twice. A bug, and one that would double count.
    #[error("call `{call}` was already settled")]
    AlreadySettled { call: String },

    /// Cost could not be derived.
    #[error("cannot derive cost")]
    Cost(#[from] kgraf_core::cost::CostError),

    /// A settlement or reconciliation named a call whose state does not allow it:
    /// a settle needs an `in_flight` row, a reconcile an `unreconciled` or
    /// `in_flight` one. Refused so a resolve cannot overwrite a counted row and a
    /// late settle cannot overwrite a reconciled figure.
    #[error("call `{call}` is `{state}`; {wanted}")]
    WrongState {
        call: String,
        state: String,
        wanted: &'static str,
    },

    /// A call named for redaction does not exist, is not settled yet, or has no
    /// chain event yet to preserve.
    #[error("call `{call}` cannot be redacted: {reason}")]
    NotRedactable { call: String, reason: &'static str },

    /// A call named for redaction is already redacted. Redacting twice would let a
    /// second redaction event imply a second, different removal of the same
    /// payload.
    #[error("call `{call}` was already redacted")]
    AlreadyRedacted { call: String },

    /// A trace field was written after the row left `in_flight`. Refused rather
    /// than applied silently: the row is already chained by the event that
    /// settled it, and a write afterward would leave the live row diverging from
    /// that event without either being wrong on its own (fail loud).
    #[error("call `{call}` is no longer in flight; {what} cannot be written")]
    NotInFlight { call: String, what: &'static str },

    /// An existing `tool_rounds` column held bytes that do not parse as the JSON
    /// array this module itself always writes. Refused rather than silently
    /// treated as an empty list, which would discard every prior round without
    /// trace (the fail-loud rule, the test-the-mechanism rule).
    #[error("call `{call}` has a corrupt tool_rounds column; refusing to overwrite it blind")]
    CorruptTrace { call: String },

    /// The writer thread has stopped (the ledger is closing, or the thread
    /// died), so the operation was not recorded.
    #[error("the ledger writer has stopped; the operation was not recorded")]
    WriterStopped,

    /// The writer's batch transaction failed as a whole, so no operation in it
    /// was recorded, this one included.
    #[error("the ledger batch was not recorded: {reason}")]
    BatchFailed { reason: String },

    /// An invariant inside this module did not hold. A bug, reported rather
    /// than panicked on.
    #[error("ledger internal error: {0}")]
    Internal(&'static str),

    /// The operating system refused something the ledger needs (a thread).
    #[error("ledger I/O error: {0}")]
    Io(String),
}

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settlement {
    /// Usage was reported and recorded.
    Counted,
    /// The call provably billed nothing: it never reached the provider, or the
    /// provider rejected it before generation. A failure after the request may
    /// have generated is `Unreconciled`, never this; the vendor bills a failed
    /// attempt that ran.
    Failed,
    /// The call was cancelled before completing. Whether it billed is unknown,
    /// which is why it is not `Failed`. One cancelled with no usage reported
    /// has unknown spend ([`CallRecord::cancelled_without_usage`]).
    Cancelled,
    /// The call finished or died without delivering usage.
    ///
    /// The important state. Provider-side spend may exist with no local record of
    /// its size, so the row is kept, flagged, and reported until a human or a
    /// reconciliation pass resolves it.
    Unreconciled,
}

impl Settlement {
    /// The settlement's stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Settlement::Counted => "counted",
            Settlement::Failed => "failed",
            Settlement::Cancelled => "cancelled",
            Settlement::Unreconciled => "unreconciled",
        }
    }

    fn parse(name: &str) -> Option<Settlement> {
        Some(match name {
            "counted" => Settlement::Counted,
            "failed" => Settlement::Failed,
            "cancelled" => Settlement::Cancelled,
            "unreconciled" => Settlement::Unreconciled,
            _ => return None,
        })
    }
}

/// Below this many recorded calls on a route, [`Ledger::route_round_stats`]
/// reports `None` for both figures rather than a median of one or two
/// samples (design, "The per-machine maximum": "20 s is the default until
/// then", i.e. until the route has 20 rounds).
pub const ROUTE_STATS_MIN_ROUNDS: usize = 20;

/// [`Ledger::route_round_stats`]'s result: `kgraf-capacity::RouteInputs`'s
/// `round_ms` and `tokens_per_round`, plus how many calls the figures are
/// drawn from (for a caller that wants to show its own confidence).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouteRoundStats {
    pub round_ms: Option<u64>,
    pub tokens_per_round: Option<u64>,
    pub rounds: usize,
}

/// The median of `values`, sorting in place (the caller's copy, never the
/// ledger's own data). Panics on an empty slice: every call site here has
/// already checked `is_empty()`/`len()` first.
fn median_u64(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// One accounted call, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct CallRecord {
    pub id: CallId,
    pub adapter: String,
    pub protocol: Protocol,
    pub model: String,
    /// The agent the spend is attributed to.
    pub agent_id: String,
    /// The task the spend is attributed to.
    pub task_id: String,
    pub settlement: Settlement,
    pub usage: TokenUsage,
    /// The provider's request id, where it sent one. The handle for reconciling
    /// against a provider-side invoice.
    pub request_id: Option<String>,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    /// The trace fields (parent call,
    /// kind, role, time to first token, the tool list per round, and what the
    /// call was given). All optional: a call issued through [`Ledger::begin`]
    /// rather than [`Ledger::begin_traced`] carries none of them, and nothing
    /// downstream treats their absence as an error.
    pub trace: CallTrace,
    /// Set once the row's payload has been redacted.
    pub redacted: bool,
    /// The catalog route id this call was made on (decision 0067), written by [`crate::ratelimit::RateLimited`] (a
    /// limited route) or [`crate::ratelimit::RouteRecorded`] (any other) once it
    /// knows it, distinct from `adapter` (the wire adapter constant, which
    /// several different routes can share). `None` for a call made without
    /// going through either wrapper (most test fixtures).
    pub route: Option<String>,
}

impl CallRecord {
    /// A call cancelled before the provider reported any usage. Its spend is
    /// unknown, not zero: the vendor may have billed what it processed before
    /// the cut. Charged like an unreconciled call, at a bound, never as `$0`.
    /// A cancelled call with partial usage is charged at that usage.
    pub fn cancelled_without_usage(&self) -> bool {
        self.settlement == Settlement::Cancelled && self.usage.is_empty()
    }
}

/// What a call was for and what it saw.
///
/// Everything here is optional because it is filled in only by
/// [`Ledger::begin_traced`]; a plain [`Ledger::begin`] call (the CLI's `call`
/// command, every older test) leaves it empty rather than failing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallTrace {
    /// The call this one was made in service of, if any (a sub-call a worker's
    /// loop issued while answering a parent call).
    pub parent_call: Option<CallId>,
    /// What kind of call this is (free text: the runtime, not this crate, owns
    /// the vocabulary of kinds).
    pub kind: Option<String>,
    /// The role this call played (free text, same reasoning as `kind`).
    pub role: Option<String>,
    /// Milliseconds from `started_ms` to the first output token, set by
    /// [`CallToken::record_first_token_async`] once streaming begins.
    pub ttft_ms: Option<i64>,
    /// The ordered tool list of each round, appended one round at a time by
    /// [`CallToken::record_tool_round_async`].
    pub tool_rounds: Vec<Vec<String>>,
    /// The graph slice ids the call was given as context.
    pub given_slice_ids: Vec<String>,
    /// The blake3 hex digest of the shared prefix (decision 0043) the call was
    /// given. Stored as a hash, never as the prefix text itself: that keeps
    /// "what did this worker see" exact without duplicating the prefix bytes
    /// once per call.
    pub given_prefix_hash: Option<String>,
}

/// One piece of MCP traffic to record (decision 0051), as the MCP client saw it.
///
/// The vocabulary of `kind` and `outcome` belongs to the MCP client
/// (`kgraf-mcp`), not to this crate, which names no protocol above its own
/// seam; they are stored as the client's stable names (`connect`,
/// `list_tools`, `call_tool`, `oauth`; `success`, `protocol_error`,
/// `timed_out`, `cancelled`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpCall {
    /// The server's configured name.
    pub server: String,
    /// The tool called, for a `call_tool` exchange.
    pub tool: Option<String>,
    pub kind: String,
    /// The agent the exchange is attributed to. `None` only for traffic that
    /// happened outside any worker (an OAuth flow the user started by hand).
    pub agent_id: Option<String>,
    pub task_id: Option<String>,
    pub duration_ms: i64,
    pub outcome: String,
    /// For a successful `call_tool`: whether the tool reported an error result.
    pub tool_is_error: Option<bool>,
    /// Bytes sent to the server, where known.
    pub bytes_in: i64,
    /// Bytes received from the server, where known.
    pub bytes_out: i64,
}

/// One recorded MCP exchange, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpCallRecord {
    pub id: String,
    pub recorded_ms: i64,
    pub call: McpCall,
}

/// What [`Ledger::spend_by`] groups on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    /// The agent the spend is attributed to.
    Agent,
    /// The run: the part of the task id before its first `/` (an orchestrator
    /// run's own calls carry the run id as the task, its workers' `run/...`), or
    /// the whole task id when it has no `/`.
    Run,
    /// The UTC hour the call started in.
    Hour,
}

/// A group's key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupKey {
    /// An agent id or a run id.
    Name(String),
    /// The hour's start, in milliseconds since the Unix epoch.
    HourStartMs(i64),
}

/// The spend of one group, across models.
#[derive(Clone, Debug, PartialEq)]
pub struct SpendGroup {
    pub key: GroupKey,
    /// The priced spend. Excludes the models in `unpriced`.
    pub cost: Money,
    pub calls: u64,
    /// Models in this group the price table cannot price; their spend is not in
    /// `cost`, and is not zero.
    pub unpriced: Vec<String>,
}

/// One task's usage history on one model, for sizing.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskUsage {
    pub model: String,
    pub task_id: String,
    pub calls: u64,
    /// Input tokens of the task's first call (by start time).
    pub first_input: u64,
    /// The largest input of any of the task's calls.
    pub peak_input: u64,
    pub usage: TokenUsage,
}

/// The outcome of [`Ledger::spend_of`].
#[derive(Clone, Debug, PartialEq)]
pub struct SpendOf {
    /// The priced spend of the calls whose spend is known.
    pub total: Money,
    /// Calls whose spend is not known: missing, in flight, unreconciled,
    /// cancelled before any usage was reported, or on a model the table cannot
    /// price.
    pub unknown: Vec<CallId>,
}

/// Permission to issue exactly one provider call, and the obligation to report
/// what it cost.
///
/// Cannot be constructed outside this module. That is the whole mechanism.
///
/// Every write method comes in two forms. The plain one (`settle`) blocks until
/// the row is durable and is for synchronous callers: tests, the CLI. The
/// `_async` one (`settle_async`) submits the write **at once**, before it
/// returns, and gives back a [`LedgerAck`] an async caller awaits without
/// blocking a runtime worker thread. Because the submission is eager, dropping
/// the ack (a cancelled `select!` branch) does not unsubmit the write: the row is
/// still recorded, the caller only stops waiting to hear so.
pub struct CallToken<'ledger> {
    ledger: &'ledger Ledger,
    id: CallId,
    /// Atomic rather than a `Cell` because a token is held across an `await` in
    /// every adapter, so it must be `Sync`.
    settled: AtomicBool,
    adapter: &'static str,
    /// Set when the caller stops the call on purpose (a cancel, a steer, a
    /// time limit), so an abandoned call settles `Cancelled`, not
    /// `Unreconciled`. See [`Self::stop_on_purpose`].
    stopped: AtomicBool,
    /// A caller-supplied check for the same thing, read when the call is
    /// abandoned. See [`Self::stop_when`].
    stop_probe: std::sync::OnceLock<StopProbe>,
}

/// Reports whether a caller has stopped a call on purpose.
type StopProbe = Box<dyn Fn() -> bool + Send + Sync>;

impl<'ledger> CallToken<'ledger> {
    /// Record that the caller is stopping this call on purpose, before it
    /// drops the future that makes the call.
    ///
    /// Whatever then closes the abandoned row (an adapter's
    /// [`crate::StreamGuard`], or this token's own `Drop`) settles it
    /// [`Settlement::Cancelled`], with the partial usage it observed, rather
    /// than [`Settlement::Unreconciled`]. A call that ends without usage for
    /// a reason the caller did not choose (the stream broke, the provider
    /// failed, a panic) is not marked and stays unreconciled. Has no effect
    /// once the row is settled.
    pub fn stop_on_purpose(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Like [`Self::stop_on_purpose`], for a caller that cannot reach the
    /// token when it stops the call: `probe` is read when the call is
    /// abandoned, and a `true` means the caller stopped it. Only the first
    /// probe set is kept; a later one is ignored.
    pub fn stop_when(&self, probe: impl Fn() -> bool + Send + Sync + 'static) {
        let _first_only = self.stop_probe.set(Box::new(probe));
    }

    /// Whether the caller has stopped this call on purpose.
    pub fn stopped_on_purpose(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) || self.stop_probe.get().is_some_and(|probe| probe())
    }

    /// The ledger id of this call.
    pub fn id(&self) -> &CallId {
        &self.id
    }

    /// The adapter this token was minted for.
    pub fn adapter(&self) -> &'static str {
        self.adapter
    }

    /// Record the time to first token, measured by the caller against its own
    /// clock. Refused ([`LedgerError::NotInFlight`]) once already set or once
    /// the row has settled (the row is already chained by then).
    ///
    /// The result is the caller's to act on (every call is attributed to the ledger): a
    /// streaming loop must not fail because a metric arrived late, but it must
    /// not lose the fact either, so it counts and reports the refusal rather
    /// than this method swallowing it.
    ///
    /// Operations from one token reach the writer in submission order, so a
    /// time to first token submitted before the settlement is applied before it,
    /// even when the caller awaits neither until the stream has ended.
    pub fn record_first_token_async(&self, ttft_ms: i64) -> LedgerAck<()> {
        self.ledger.submit(
            Op::Ttft {
                id: self.id.clone(),
                ttft_ms,
            },
            Done::unit,
        )
    }

    /// Record the catalog route id this call was made on (decision 0067), so [`Ledger::route_round_stats`] can group by the
    /// route a user actually configured rather than by the wire adapter
    /// several routes can share. Never called by a bare `Ledger::begin`
    /// caller directly: [`crate::ratelimit::RateLimited`] (a limited route) and
    /// [`crate::ratelimit::RouteRecorded`] (an unlimited one) call this once
    /// they know their own route, right after the token is minted; a call made
    /// without going through either wrapper
    /// (most test fixtures) simply leaves this `NULL`, which
    /// `route_round_stats` and `kgraf-capacity` both already treat as
    /// "unmeasured" rather than as an error. Unlike
    /// [`Self::record_first_token_async`], setting the same route again is not
    /// refused (a rebuilt provider for one route re-binds its
    /// `RouteLimiter` and records the same route on every call it wraps).
    pub fn record_route_async(&self, route: &str) -> LedgerAck<()> {
        self.ledger.submit(
            Op::Route {
                id: self.id.clone(),
                route: route.to_owned(),
            },
            Done::unit,
        )
    }

    /// Append one round's ordered tool list. Called once per round, in order, so
    /// the stored `tool_rounds` array is the round sequence itself, not a set.
    /// Refused once the row has settled, for the same reason as
    /// [`Self::record_first_token_async`], whose note on the result applies here too.
    pub fn record_tool_round_async(&self, tools: &[String]) -> LedgerAck<()> {
        self.ledger.submit(
            Op::ToolRound {
                id: self.id.clone(),
                tools: tools.to_vec(),
            },
            Done::unit,
        )
    }

    /// Record the usage this call consumed.
    pub fn settle(&self, usage: &TokenUsage, request_id: Option<&str>) -> Result<(), LedgerError> {
        self.settle_async(usage, request_id).wait()
    }

    /// [`Self::settle`] for an async caller.
    pub fn settle_async(&self, usage: &TokenUsage, request_id: Option<&str>) -> LedgerAck<()> {
        self.finish(Settlement::Counted, Some(usage), request_id)
    }

    /// Close the call from the error it ended with.
    ///
    /// The one place the failed-versus-unreconciled judgement is made, so no
    /// adapter can record a billed failure as free. See
    /// [`crate::ProviderError::billing`] for the rule.
    pub fn close_on(
        &self,
        error: &crate::ProviderError,
        request_id: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.close_on_async(error, request_id).wait()
    }

    /// [`Self::close_on`] for an async caller.
    pub fn close_on_async(
        &self,
        error: &crate::ProviderError,
        request_id: Option<&str>,
    ) -> LedgerAck<()> {
        match error.billing() {
            crate::error::Billing::NotBilled => self.fail_async(),
            crate::error::Billing::MayHaveBilled => self.leave_unreconciled_async(request_id),
        }
    }

    /// Record that the call failed before billing anything.
    ///
    /// Only for a failure that provably billed nothing. Prefer [`Self::close_on`],
    /// which makes that judgement from the error.
    pub fn fail(&self) -> Result<(), LedgerError> {
        self.fail_async().wait()
    }

    /// [`Self::fail`] for an async caller.
    pub fn fail_async(&self) -> LedgerAck<()> {
        self.finish(Settlement::Failed, None, None)
    }

    /// Record that the call was cancelled.
    ///
    /// Any usage observed before cancellation is kept: a cancelled stream still
    /// billed for what it produced, and discarding the partial count would
    /// understate spend.
    pub fn cancel(&self, partial: Option<&TokenUsage>) -> Result<(), LedgerError> {
        self.cancel_async(partial).wait()
    }

    /// [`Self::cancel`] for an async caller.
    pub fn cancel_async(&self, partial: Option<&TokenUsage>) -> LedgerAck<()> {
        self.cancel_with_async(partial, None)
    }

    /// [`Self::cancel_async`], keeping the provider's request id when the
    /// stream had already sent one.
    pub fn cancel_with_async(
        &self,
        partial: Option<&TokenUsage>,
        request_id: Option<&str>,
    ) -> LedgerAck<()> {
        self.finish(Settlement::Cancelled, partial, request_id)
    }

    /// Record that the call ended without usage, with whatever handle exists for
    /// reconciling it later.
    pub fn leave_unreconciled(&self, request_id: Option<&str>) -> Result<(), LedgerError> {
        self.leave_unreconciled_async(request_id).wait()
    }

    /// [`Self::leave_unreconciled`] for an async caller.
    pub fn leave_unreconciled_async(&self, request_id: Option<&str>) -> LedgerAck<()> {
        self.leave_unreconciled_with_async(None, request_id)
    }

    /// Record that the call ended unreconciled, but with a **partial** usage
    /// figure observed before the stream broke, plus the provider's request
    /// id.
    ///
    /// For the case `leave_unreconciled` cannot represent: a provider that
    /// reports usage in its first stream chunk (`message_start`, for one
    /// vendor surveyed) so a truncated stream still yields a real, if
    /// possibly-incomplete, usage figure, not zero. Recording that as
    /// `leave_unreconciled(request_id)` would store zero usage, which
    /// silently understates spend exactly as decision 0006 warns against;
    /// recording it as [`Self::cancel`] would finalize the row as billed with
    /// no way to reconcile it against the provider's own later figure, and
    /// `cancel` also has nowhere to put a request id.
    ///
    /// **The partial usage is a stated lower bound, not a spend figure.** The
    /// row settles as [`Settlement::Unreconciled`], not `Counted`, so it
    /// stays out of [`Ledger::usage_by_model`], [`Ledger::total_cost`] and
    /// [`Ledger::cost_for_agent`] exactly like a zero-usage unreconciled row
    /// already does: those totals would otherwise look complete while
    /// resting on a number known to be a floor, not the true figure. The
    /// point of carrying it at all is that [`Ledger::unreconciled`] shows it,
    /// so whatever reconciles this call later has a starting point instead of
    /// nothing.
    ///
    /// Chained as a `settle` event like any other finalization, this is not
    /// a new event kind, since an unreconciled row with a nonzero usage
    /// figure is still, structurally, just a settlement whose `Settlement` is
    /// `Unreconciled`. [`Ledger::reconcile`] replaces it later with a
    /// `reconcile` event carrying the provider's final figure, the same way
    /// it already replaces a zero-usage unreconciled row.
    pub fn leave_unreconciled_with(
        &self,
        partial: Option<&TokenUsage>,
        request_id: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.leave_unreconciled_with_async(partial, request_id)
            .wait()
    }

    /// [`Self::leave_unreconciled_with`] for an async caller.
    pub fn leave_unreconciled_with_async(
        &self,
        partial: Option<&TokenUsage>,
        request_id: Option<&str>,
    ) -> LedgerAck<()> {
        self.finish(Settlement::Unreconciled, partial, request_id)
    }

    fn finish(
        &self,
        settlement: Settlement,
        usage: Option<&TokenUsage>,
        request_id: Option<&str>,
    ) -> LedgerAck<()> {
        if self.settled.swap(true, Ordering::SeqCst) {
            return LedgerAck::ready(Err(LedgerError::AlreadySettled {
                call: self.id.to_string(),
            }));
        }
        // The flag is already set, so a concurrent second settle is refused above
        // rather than racing this write. A failed write leaves the flag set and the
        // row in flight, which `unreconciled` then reports, the honest outcome.
        self.ledger.submit(
            Op::Settle {
                id: self.id.clone(),
                settlement,
                usage: usage.cloned(),
                request_id: request_id.map(str::to_owned),
            },
            Done::unit,
        )
    }
}

impl Drop for CallToken<'_> {
    fn drop(&mut self) {
        if self.settled.swap(true, Ordering::SeqCst) {
            return;
        }
        // A token dropped without settling is a call whose cost is unknown. It must
        // not disappear: the accounting rule is violated by an uncounted call, and pretending the
        // call never happened is exactly how spend goes missing.
        //
        // Submitted without waiting: `drop` cannot fail and often runs on an async
        // worker thread (a cancelled future), which must not block on an fsync. The
        // write is still ordered before any later read through this ledger (see
        // `Ledger::read_barrier`), and a write that fails leaves the row in flight,
        // which `unreconciled` reports.
        //
        // A call its caller stopped on purpose is cancelled, with no usage
        // known here; any other abandoned call is unreconciled.
        let settlement = if self.stopped_on_purpose() {
            Settlement::Cancelled
        } else {
            Settlement::Unreconciled
        };
        self.ledger.submit_detached(Op::Settle {
            id: self.id.clone(),
            settlement,
            usage: None,
            request_id: None,
        });
    }
}

/// The acknowledgement of one ledger write.
///
/// Resolves once the write's transaction has committed with `synchronous =
/// FULL`, so a caller that has it `Ok` may proceed as if the row is recorded.
///
/// **What "durable" covers.** An acknowledged row survives a crash of the
/// process or of the operating system. On macOS it does not necessarily
/// survive a power loss: SQLite flushes with `fsync(2)`, not `F_FULLFSYNC`
/// (`PRAGMA fullfsync` stays off, SQLite's default, which the ledger keeps for
/// performance), and `fsync` on macOS does not flush the drive's own write
/// cache. Other platforms' `fsync` does.
///
/// It
/// is a [`Future`](std::future::Future) for an async caller and has
/// [`Self::wait`] for a blocking one; the write itself was submitted when this
/// value was created, so dropping it does not cancel the write.
#[must_use = "the write is submitted either way; drop the ack only when its outcome truly does not matter"]
pub struct LedgerAck<T> {
    state: AckState<T>,
}

enum AckState<T> {
    Ready(Option<Result<T, LedgerError>>),
    Waiting {
        rx: futures_channel::oneshot::Receiver<Result<Done, LedgerError>>,
        map: fn(Done) -> Result<T, LedgerError>,
    },
}

impl<T> LedgerAck<T> {
    fn ready(result: Result<T, LedgerError>) -> Self {
        Self {
            state: AckState::Ready(Some(result)),
        }
    }

    /// Block the current thread until the write is durable (or refused).
    ///
    /// Safe on any thread, including inside a tokio runtime: it parks on the
    /// writer thread's reply, never on the runtime. An async caller should
    /// `.await` instead, so its worker thread stays free.
    pub fn wait(self) -> Result<T, LedgerError> {
        futures_executor::block_on(self)
    }

    /// Give up on the outcome and return the value if the write succeeded
    /// before this point. Closing the reply first makes the split exact: a
    /// reply sent before the close is read here, and a send after it fails,
    /// so the writer sees that nobody took the value (`run_writer`).
    fn abandon(self) -> Option<T> {
        match self.state {
            AckState::Ready(result) => result.and_then(Result::ok),
            AckState::Waiting { mut rx, map } => {
                rx.close();
                match rx.try_recv() {
                    Ok(Some(Ok(done))) => map(done).ok(),
                    _ => None,
                }
            }
        }
    }
}

/// The future [`Ledger::begin_traced_async`] returns: the `begin` is already
/// submitted, and the token is made when its id is known.
///
/// Dropped before that (its caller was cancelled: a stopped worker, a steer,
/// a time limit), it settles the row unreconciled, as a dropped
/// [`CallToken`] does, so a reserved call never stays in flight for good. The
/// guard has two halves, and together they leave no gap: a row whose id
/// already reached this side is settled here; a row whose reply had not yet
/// been sent when this side closed it is settled by the writer, whose send
/// then fails.
struct PendingBegin<'a> {
    ledger: &'a Ledger,
    adapter: &'static str,
    /// `None` once the token has been handed out.
    ack: Option<LedgerAck<CallId>>,
}

impl<'a> std::future::Future for PendingBegin<'a> {
    type Output = Result<CallToken<'a>, LedgerError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let Some(ack) = self.ack.as_mut() else {
            return std::task::Poll::Ready(Err(LedgerError::Internal(
                "a begin future was polled after it completed",
            )));
        };
        let std::task::Poll::Ready(result) = std::pin::Pin::new(ack).poll(cx) else {
            return std::task::Poll::Pending;
        };
        self.ack = None;
        std::task::Poll::Ready(result.map(|id| self.ledger.token(id, self.adapter)))
    }
}

impl Drop for PendingBegin<'_> {
    fn drop(&mut self) {
        let Some(ack) = self.ack.take() else {
            return;
        };
        if let Some(id) = ack.abandon() {
            // Detached, like `CallToken::drop`: `drop` cannot fail and must not
            // block on an fsync.
            self.ledger.submit_detached(Op::Settle {
                id,
                settlement: Settlement::Unreconciled,
                usage: None,
                request_id: None,
            });
        }
    }
}

// Never pinned structurally: `poll` only moves the result out of its `Option`
// and polls the receiver, which is itself `Unpin`.
impl<T> Unpin for LedgerAck<T> {}

impl<T> std::future::Future for LedgerAck<T> {
    type Output = Result<T, LedgerError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match &mut self.state {
            AckState::Ready(result) => {
                std::task::Poll::Ready(result.take().unwrap_or(Err(LedgerError::WriterStopped)))
            }
            AckState::Waiting { rx, map } => match std::pin::Pin::new(rx).poll(cx) {
                std::task::Poll::Pending => std::task::Poll::Pending,
                std::task::Poll::Ready(Ok(Ok(done))) => std::task::Poll::Ready(map(done)),
                std::task::Poll::Ready(Ok(Err(e))) => std::task::Poll::Ready(Err(e)),
                // The writer dropped the reply without sending: it stopped before
                // this write committed, so the write is not recorded (fail loud).
                std::task::Poll::Ready(Err(_canceled)) => {
                    std::task::Poll::Ready(Err(LedgerError::WriterStopped))
                }
            },
        }
    }
}

/// The chain's tip: the last event's sequence number and chain hash.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tip {
    seq: i64,
    chain_hash: String,
}

impl Tip {
    fn genesis() -> Self {
        Tip {
            seq: 0,
            chain_hash: chain_genesis(),
        }
    }
}

/// The anchor file's writer, off the settle path entirely.
///
/// **Why.** The first version of this fsynced `.tip` on every settle, inline,
/// under the same lock a settling call waits on: ~5 ms per settled call on
/// disk, measured, which broke the per-turn loop budget. Performance wins over having
/// every settle wait for a durable anchor. What it must not do instead is drop
/// the anchor's guarantee silently, the fail-loud rule forbids trading a security property
/// away without saying so, so the guarantee is narrowed and stated precisely
/// in [`Ledger::verify`]'s doc comment rather than quietly weakened: the anchor
/// may lag the chain by whatever the writer has not yet flushed, and
/// [`Ledger::flush_anchor`] (and `Drop`) exist so a caller that needs the
/// stronger guarantee at a specific point (before shutdown, before a backup)
/// can wait for it explicitly.
///
/// **How.** `publish` never blocks: it swaps in the latest tip and notifies the
/// writer thread, coalescing under load exactly as "latest wins" says, if
/// three calls settle before the writer gets to run, it writes the anchor
/// once, for the third, not three times. The writer thread does the
/// `write_tip_file` (temp file, fsync, rename) entirely off the caller's
/// thread.
/// How long the anchor writer waits after finishing one write before it looks
/// for the next pending tip. See the comment inside `AnchorWriter::run` where
/// it is used for why this exists: it is a measured fix, not a guess.
const ANCHOR_COALESCE_WINDOW: Duration = Duration::from_millis(8);

struct AnchorWriter {
    state: Mutex<AnchorState>,
    cvar: Condvar,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Test-only: an artificial delay before each write, so a test can prove
    /// the settle path does not wait for it (see
    /// `settle_does_not_block_on_a_deliberately_slow_anchor_writer`).
    #[cfg(test)]
    delay: Duration,
}

struct AnchorState {
    pending: Option<Tip>,
    /// The highest sequence number ever handed to `publish`.
    published_seq: i64,
    /// The highest sequence number the writer has durably finished.
    written_seq: i64,
    stop: bool,
}

impl AnchorWriter {
    fn start_with_delay(path: PathBuf, delay: Duration) -> Arc<Self> {
        let writer = Arc::new(AnchorWriter {
            state: Mutex::new(AnchorState {
                pending: None,
                published_seq: 0,
                written_seq: 0,
                stop: false,
            }),
            cvar: Condvar::new(),
            handle: Mutex::new(None),
            #[cfg(test)]
            delay,
        });
        #[cfg(not(test))]
        let _ = delay;
        let worker = Arc::clone(&writer);
        let join = std::thread::Builder::new()
            .name("kgraf-ledger-anchor".to_owned())
            .spawn(move || worker.run(path))
            .expect("spawning the anchor writer thread");
        *writer.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(join);
        writer
    }

    /// Publish a new tip. Never blocks on I/O: it only ever takes the state
    /// mutex to swap a pointer and notifies the writer thread to pick it up.
    fn publish(&self, tip: Tip) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.published_seq = state.published_seq.max(tip.seq);
        state.pending = Some(tip);
        self.cvar.notify_all();
    }

    /// Block until every tip published so far has been durably written.
    fn flush(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.written_seq < state.published_seq {
            state = self.cvar.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Flush, then stop and join the writer thread. Idempotent: a second call
    /// finds no handle left and does nothing.
    fn shutdown(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stop = true;
        }
        self.cvar.notify_all();
        if let Some(handle) = self.handle.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = handle.join();
        }
    }

    fn run(&self, path: PathBuf) {
        loop {
            let tip = {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if let Some(tip) = state.pending.take() {
                        break Some(tip);
                    }
                    if state.stop {
                        break None;
                    }
                    state = self.cvar.wait(state).unwrap_or_else(|p| p.into_inner());
                }
            };
            let Some(tip) = tip else {
                return;
            };
            #[cfg(test)]
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            // Best-effort in the sense `Drop` already is elsewhere in this
            // module: a failed anchor write cannot propagate to whichever
            // settle call happened to trigger it (that call already
            // committed and returned), so it is swallowed here. It is not
            // silent forever, the next `verify` compares the chain against
            // whatever the anchor last says, and a write that never lands
            // leaves the anchor further behind, which is exactly the lag
            // `verify` already tolerates and, past a point a human sets,
            // would need its own alerting, out of scope here.
            let _ = write_tip_file(&path, &tip);
            {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.written_seq = state.written_seq.max(tip.seq);
            }
            self.cvar.notify_all();
            // Coalescing window: sleep before looking for the next pending
            // tip rather than draining it immediately. Without this, a burst
            // of rapid settles each gets its own full write-and-fsync on this
            // thread, and (measured, not assumed) those concurrent fsyncs
            // to the *same* filesystem as the database's own commit fsyncs
            // contended badly enough to add roughly a millisecond back onto
            // the foreground settle latency this whole mechanism exists to
            // avoid (config C dropped from ~5 ms to ~1.3 ms with only the
            // pragma tuning below, then to comfortably under 1 ms once this
            // window was added). `flush_anchor` still returns as soon as the
            // pending tip it cares about is written; this only delays how
            // soon the *next* write after that one begins.
            std::thread::sleep(ANCHOR_COALESCE_WINDOW);
        }
    }
}

/// The most operations one writer transaction takes from the queue. Bounds the
/// latency the first operation in a batch pays for the ones queued behind it; at
/// tens of microseconds of SQL per operation, a full batch is a few milliseconds.
const MAX_BATCH: usize = 256;

/// The most read connections an on-disk ledger opens. A reader beyond this waits
/// for one to be returned rather than opening another file handle.
const MAX_READERS: usize = 4;

/// One ledger mutation, as submitted to the writer thread.
enum Op {
    Begin {
        adapter: &'static str,
        protocol: Protocol,
        model: String,
        agent_id: String,
        task_id: String,
        trace: CallTrace,
    },
    Settle {
        id: CallId,
        settlement: Settlement,
        usage: Option<TokenUsage>,
        request_id: Option<String>,
    },
    Reconcile {
        id: CallId,
        usage: TokenUsage,
    },
    Redact {
        id: CallId,
        reason: String,
    },
    Ttft {
        id: CallId,
        ttft_ms: i64,
    },
    /// The catalog route id this call was actually made on (decision 0067), written by [`crate::ratelimit::RateLimited`]
    /// once it knows it, after `begin` already minted the row.
    Route {
        id: CallId,
        route: String,
    },
    ToolRound {
        id: CallId,
        tools: Vec<String>,
    },
    Mcp {
        call: McpCall,
    },
}

/// What a committed operation produced.
enum Done {
    Unit,
    Call(CallId),
    Mcp(String),
}

impl Done {
    fn unit(_: Done) -> Result<(), LedgerError> {
        Ok(())
    }

    fn call(done: Done) -> Result<CallId, LedgerError> {
        match done {
            Done::Call(id) => Ok(id),
            _ => Err(LedgerError::Internal(
                "a begin was answered with no call id",
            )),
        }
    }

    fn mcp(done: Done) -> Result<String, LedgerError> {
        match done {
            Done::Mcp(id) => Ok(id),
            _ => Err(LedgerError::Internal(
                "an MCP record was answered with no id",
            )),
        }
    }
}

/// One queued operation: the op, where to send its outcome (none for a
/// fire-and-forget write from `Drop`), and its place in submission order.
struct Request {
    op: Op,
    reply: Option<futures_channel::oneshot::Sender<Result<Done, LedgerError>>>,
    ticket: u64,
}

/// The sending side of the writer's queue, and the ticket counter, under one
/// mutex so that ticket order is queue order (a read barrier depends on it).
struct Submitter {
    tx: Option<std::sync::mpsc::Sender<Request>>,
    next_ticket: u64,
}

/// How far the writer has got: every ticket at or below `processed` has
/// committed or been refused. `stopped` is set when the writer thread exits for
/// any reason, so nothing waits on it forever.
struct Progress {
    state: Mutex<(u64, bool)>,
    cvar: Condvar,
    /// Write transactions the writer has run. With `processed`, what a test
    /// uses to show that queued operations share one commit.
    batches: std::sync::atomic::AtomicU64,
}

impl Progress {
    fn advance(&self, ticket: u64) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.0 = state.0.max(ticket);
        self.cvar.notify_all();
    }

    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.1 = true;
        self.cvar.notify_all();
    }

    fn wait_for(&self, ticket: u64) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.0 < ticket && !state.1 {
            state = self.cvar.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// Marks the writer stopped when its thread exits, including by a panic that
/// escaped the per-batch guard.
struct StopOnExit(Arc<Progress>);

impl Drop for StopOnExit {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Where reads run.
enum Reads {
    /// An in-memory database exists only inside its one connection, so reads
    /// share it with the writer under its mutex.
    Shared(Arc<Mutex<Connection>>),
    /// An on-disk database: read-only WAL connections, pooled. A WAL reader
    /// never blocks the writer and is never blocked by it, so a full-table
    /// aggregate or a `verify` walk no longer stalls a settling worker.
    Pool {
        path: PathBuf,
        pool: Mutex<ReaderPool>,
        returned: Condvar,
    },
}

/// The read connections of an on-disk ledger.
#[derive(Default)]
struct ReaderPool {
    idle: Vec<Connection>,
    /// Connections open now, idle or lent out. Never above [`MAX_READERS`].
    open: usize,
}

/// A read connection lent out of the pool; `Drop` gives it back.
struct LentReader<'a> {
    conn: Option<Connection>,
    pool: &'a Mutex<ReaderPool>,
    returned: &'a Condvar,
}

impl Drop for LentReader<'_> {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        // A read that failed inside `verify`'s snapshot leaves it open; never
        // return a connection mid-transaction to the pool.
        if !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        let mut p = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if conn.is_autocommit() {
            p.idle.push(conn);
        } else {
            p.open -= 1;
        }
        drop(p);
        self.returned.notify_one();
    }
}

/// Test-only fault injection for the writer: panic or abort the process after
/// applying the `n`th operation (counted from 1 over the writer's lifetime), in
/// the middle of its batch, before that batch commits.
///
/// `pub(crate)` and feature-gated (not only `#[cfg(test)]`) so `ratelimit.rs`'s
/// own tests can reach it too, to prove a ledger-close failure is kept on
/// [`crate::error::ProviderError::RateLimitStoreUnavailable`], not only on
/// [`crate::error::ProviderError::RateLimited`].
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
#[derive(Clone, Copy, Default)]
pub(crate) struct WriterFaults {
    pub(crate) panic_after_op: Option<u64>,
    pub(crate) abort_after_op: Option<u64>,
}

/// The accounting ledger. One per project, alongside the graph.
///
/// **On disk, one writer thread owns every mutation** (decision 0059 still holds; this
/// changes only who runs the SQL and when it commits). Callers submit an
/// operation over a channel and wait for its acknowledgement; the writer drains
/// whatever is queued into one `BEGIN IMMEDIATE ... COMMIT`, one fsync, and
/// acknowledges each caller only after that commit returns. Measured before this
/// design (2026-09-26): the fsync itself is about 0.04 ms per WAL
/// commit, and the contention was elsewhere, adapters settling synchronously on
/// async worker threads, full `calls()` scans holding the one connection lock,
/// and per-id lookups taking that lock once each. So:
///
/// - **Durability per call is unchanged.** No caller's ack resolves before its
///   row is committed under `synchronous = FULL`. A batch that fails to commit
///   fails every operation in it; none is acknowledged.
/// - **Chain order is total and deterministic.** Operations are applied in queue
///   order, each chained onto the tip left by the one before it, inside one
///   transaction that holds SQLite's write lock (so a second process cannot
///   interleave). The tip is re-read from the database once per batch.
/// - **One operation's refusal does not sink its batch.** Each runs inside its
///   own `SAVEPOINT`; a refused operation rolls back to it and restores the tip.
/// - **Reads never wait on the write lock.** An on-disk ledger reads through
///   pooled read-only WAL connections; aggregates are SQL (`GROUP BY`), not
///   Rust loops over [`Ledger::calls`].
/// - **In memory there is no writer thread**: each operation is applied inline
///   (see the `inline` field for why and what it costs).
/// - **Read-your-writes.** A read first waits until every write submitted before
///   it has been processed ([`Ledger::read_barrier`]), so a caller that
///   submitted a write without waiting (a dropped token) still reads it back.
///
/// **Reuse (reuse existing code), searched 2026-09-26 on crates.io.** `tokio-rusqlite` 0.8
/// (MIT) runs one closure per call on a background thread: no group commit, so
/// one fsync per operation, and async-only. `sqlite-kit` 0.2 (MIT) splits a read
/// pool from a write connection but serializes writers, again one transaction
/// each. Neither acknowledges many callers from one commit or keeps the chain's
/// tip inside the writer, so the writer here is novel; its oneshot
/// acknowledgement is `futures-channel` (MIT OR Apache-2.0).
pub struct Ledger {
    /// Failures of calls their callers asked again; memory only, outside the
    /// chain. See [`crate::call_errors`].
    call_errors: crate::call_errors::CallErrorLog,
    /// Answers their callers repaired or asked again for; memory only,
    /// outside the chain. See [`crate::call_errors`].
    answer_recoveries: crate::call_errors::AgentLog<crate::call_errors::AnswerRecovery>,
    /// Test-only: the write connection, which tests tamper through to play an
    /// attacker. Outside tests only the writer thread (and, in memory,
    /// [`Reads::Shared`]) holds it.
    #[cfg(test)]
    conn: Arc<Mutex<Connection>>,
    submitter: Mutex<Submitter>,
    progress: Arc<Progress>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set for an in-memory ledger, which has no writer thread: each operation
    /// is applied on the caller's thread, under this connection's mutex, through
    /// the same `apply_batch` as the writer's. Group commit exists to share an
    /// fsync, and memory has none to share; a thread there only adds two
    /// context switches per operation (measured: the runtime's in-memory turn
    /// went from 90 us to 192 us through a writer thread, over its 100 us
    /// budget). Nothing in memory is durable, so no guarantee is traded.
    inline: Option<Arc<Mutex<Connection>>>,
    reads: Reads,
    /// The chain's current tip as of the last commit. The writer re-reads the
    /// tip from the database at the start of every batch (another process may
    /// have appended since); this copy is what [`Self::verify`] anchors an
    /// in-memory ledger against, where there is no file to read instead.
    tip: Arc<Mutex<Tip>>,
    /// Where the external anchor file lives, for an on-disk ledger. `None` for
    /// an in-memory one, which has no path to anchor a sibling file to, its
    /// anchor is `tip` itself, which still defeats same-process tampering
    /// through the very connection under test.
    tip_path: Option<PathBuf>,
    /// The background writer that durably persists `tip_path`, off the settle
    /// path (see [`AnchorWriter`]'s own doc comment for why). `None` exactly
    /// when `tip_path` is `None`.
    anchor: Option<Arc<AnchorWriter>>,
}

impl Ledger {
    /// Keep the failure of a call its caller is about to ask again. The row
    /// itself cannot carry it (see [`crate::call_errors`]).
    pub fn note_call_error(&self, entry: crate::call_errors::CallError) {
        self.call_errors.push(entry);
    }

    /// Take the kept failures of the agents whose id starts with
    /// `agent_prefix`.
    pub fn take_call_errors(&self, agent_prefix: &str) -> crate::call_errors::Drained {
        self.call_errors.drain(agent_prefix)
    }

    /// Keep an answer its caller repaired or asked again for because the model
    /// ended it before its JSON object closed (see [`crate::call_errors`]).
    pub fn note_answer_recovery(&self, entry: crate::call_errors::AnswerRecovery) {
        self.answer_recoveries.push(entry);
    }

    /// Take the kept answer recoveries of the agents whose id starts with
    /// `agent_prefix`, and the count evicted for lack of room.
    pub fn take_answer_recoveries(
        &self,
        agent_prefix: &str,
    ) -> (Vec<crate::call_errors::AnswerRecovery>, u64) {
        self.answer_recoveries.drain_entries(agent_prefix)
    }

    /// Open a ledger on disk.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        Self::open_on_disk(path, Duration::ZERO, WriterConfig::default())
    }

    /// Open an in-memory ledger.
    pub fn open_in_memory() -> Result<Self, LedgerError> {
        Self::from_connection(
            Connection::open_in_memory()?,
            None,
            Duration::ZERO,
            WriterConfig::default(),
        )
    }

    /// Test-only: open an on-disk ledger whose anchor writer sleeps `delay`
    /// before every write, so a test can prove the settle path never waits
    /// for it.
    #[cfg(test)]
    fn open_with_anchor_delay(path: &Path, delay: Duration) -> Result<Self, LedgerError> {
        Self::open_on_disk(path, delay, WriterConfig::default())
    }

    /// Test-only: open an on-disk ledger whose writer faults as `faults` says.
    /// `pub(crate)` and feature-gated: see [`WriterFaults`]'s own doc comment.
    ///
    /// `allow(dead_code)`: every in-crate call site is inside a `#[cfg(test)]`
    /// module (`ratelimit.rs`'s own tests, this module's own tests), so this
    /// function is unused, and correctly so, in the one build shape this gate
    /// exists for: a *dependent* crate (`kgraf-shell`, decision 0067's
    /// capacity re-measure thread, which needs `test-support` for
    /// `limits::clear_429_for_test`, an unrelated function) enabling
    /// `test-support` without `cfg(test)` also being active for this crate.
    /// Not a mistake to silence; a real caller of this specific function from
    /// outside the crate does not exist yet.
    #[cfg(any(test, feature = "test-support"))]
    #[cfg_attr(not(test), allow(dead_code))]
    #[doc(hidden)]
    pub(crate) fn open_with_faults(path: &Path, faults: WriterFaults) -> Result<Self, LedgerError> {
        Self::open_on_disk(path, Duration::ZERO, WriterConfig { faults })
    }

    fn open_on_disk(
        path: &Path,
        anchor_delay: Duration,
        config: WriterConfig,
    ) -> Result<Self, LedgerError> {
        let mut tip_path = path.to_path_buf();
        tip_path.set_extension("tip");
        Self::from_connection(
            Connection::open(path)?,
            Some(tip_path),
            anchor_delay,
            config,
        )
    }

    fn from_connection(
        conn: Connection,
        tip_path: Option<PathBuf>,
        anchor_delay: Duration,
        config: WriterConfig,
    ) -> Result<Self, LedgerError> {
        // WAL plus a real `busy_timeout` let a reader (`kgraf ledger verify`)
        // and a second writer (another process's `Ledger::open` on the same
        // file) both make progress instead of one
        // failing instantly with `SQLITE_BUSY`.
        //
        // `synchronous = FULL`, not `NORMAL`: NORMAL only fsyncs at WAL
        // checkpoints, so a committed `begin` row can still be lost to a power
        // loss between commit and checkpoint. The accounting rule says every call is
        // attributed; a lost `begin` row is a call that billed (or might have)
        // with no local record at all, which is worse than the accounting
        // ledger being slow. FULL fsyncs on every commit, so a crash can only
        // lose a transaction that never returned success to its caller. This
        // module's first hash-chain draft used `synchronous = NORMAL` for
        // speed; decision 0059 corrects that trade toward the durability
        // The accounting rule actually asks for. `benches/ledger_overhead.rs` reports the
        // measured cost, broken out from the chain's own cost.
        // Set before any pragma that can itself need the write lock (switching
        // `journal_mode` to WAL is one): without this, two connections racing
        // to open the same on-disk file for the first time can have the
        // second's mode-switch fail immediately with `SQLITE_BUSY` instead of
        // waiting, which is exactly backwards for a `busy_timeout` whose whole
        // point is that a caller should wait, not fail fast.
        conn.busy_timeout(Duration::from_secs(5))?;
        // `wal_autocheckpoint` (pages before an automatic checkpoint) and
        // `checkpoint_fullfsync` (macOS: whether a checkpoint's own fsync uses
        // the slow `F_FULLFSYNC`) are pure throughput tuning, neither weakens
        // `synchronous = FULL`'s per-commit durability guarantee, since a
        // checkpoint folding the WAL back into the main file is recoverable
        // from the WAL regardless of when it runs. Measured, not assumed: with
        // the chain and the async anchor writer both active, leaving these at
        // their defaults left frequent expensive checkpoints as the dominant
        // cost in `benches/ledger_overhead.rs`'s on-disk case.
        // Switching *into* WAL mode for the first time needs a moment where no
        // connection holds any lock at all, which is a stricter requirement
        // than an ordinary write (measured, not assumed:
        // `two_opens_migrating_the_same_old_database_concurrently_both_
        // succeed` failed roughly two times in five without this). SQLite
        // does not always retry it through the same busy-handler path a
        // `busy_timeout`-covered `BEGIN IMMEDIATE` uses. Retried by hand here
        // rather than trusting `busy_timeout` alone for this one pragma.
        retry_on_busy(|| {
            conn.execute_batch("PRAGMA journal_mode = WAL;")
                .map_err(Into::into)
        })?;
        conn.execute_batch(
            "PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON; \
             PRAGMA wal_autocheckpoint = 10000; PRAGMA checkpoint_fullfsync = OFF;",
        )?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        // The writer runs about a dozen distinct statements per batch and
        // caches each (`prepare_cached`): parsing, not I/O, was most of the
        // per-operation cost, and the writer is the serial stage.
        conn.set_prepared_statement_cache_capacity(32);

        let tip = Arc::new(Mutex::new(read_chain_tail(&conn)?));
        let anchor = tip_path
            .clone()
            .map(|path| AnchorWriter::start_with_delay(path, anchor_delay));
        let reads_path = match &tip_path {
            Some(_) => conn.path().map(PathBuf::from),
            None => None,
        };
        let conn = Arc::new(Mutex::new(conn));
        let reads = match reads_path {
            Some(path) => Reads::Pool {
                path,
                pool: Mutex::new(ReaderPool::default()),
                returned: Condvar::new(),
            },
            None => Reads::Shared(conn.clone()),
        };

        let progress = Arc::new(Progress {
            state: Mutex::new((0, false)),
            cvar: Condvar::new(),
            batches: std::sync::atomic::AtomicU64::new(0),
        });
        let (tx, writer, inline) = match &reads {
            Reads::Shared(_) => {
                #[cfg(not(test))]
                let _ = config;
                (None, None, Some(conn.clone()))
            }
            Reads::Pool { .. } => {
                let (tx, rx) = std::sync::mpsc::channel::<Request>();
                let conn = conn.clone();
                let tip = tip.clone();
                let anchor = anchor.clone();
                let progress = progress.clone();
                let writer = std::thread::Builder::new()
                    .name("kgraf-ledger-writer".to_owned())
                    .spawn(move || run_writer(rx, conn, tip, anchor, progress, config))
                    .map_err(|e| LedgerError::Io(e.to_string()))?;
                (Some(tx), Some(writer), None)
            }
        };

        Ok(Self {
            call_errors: Default::default(),
            answer_recoveries: Default::default(),
            #[cfg(test)]
            conn,
            submitter: Mutex::new(Submitter { tx, next_ticket: 1 }),
            progress,
            writer: Mutex::new(writer),
            inline,
            reads,
            tip,
            tip_path,
            anchor,
        })
    }

    /// Block until every anchor write published so far has been durably
    /// written. A no-op for an in-memory ledger, whose anchor has no I/O to
    /// wait for.
    ///
    /// Also run by `Drop`, so this is for a caller that needs the anchor
    /// caught up at a specific moment before the ledger itself goes away,
    /// before a backup, or in a test that must observe a fully flushed anchor.
    pub fn flush_anchor(&self) {
        // Anchor publication follows commit, so first let every write already
        // submitted commit and publish.
        self.read_barrier();
        if let Some(anchor) = &self.anchor {
            anchor.flush();
        }
    }

    /// Queue `op` and return the acknowledgement its caller awaits.
    fn submit<T>(&self, op: Op, map: fn(Done) -> Result<T, LedgerError>) -> LedgerAck<T> {
        if let Some(conn) = &self.inline {
            return LedgerAck::ready(self.apply_inline(conn, &op).and_then(map));
        }
        let (reply, rx) = futures_channel::oneshot::channel();
        match self.enqueue(op, Some(reply)) {
            Ok(()) => LedgerAck {
                state: AckState::Waiting { rx, map },
            },
            Err(e) => LedgerAck::ready(Err(e)),
        }
    }

    /// Queue `op` with nobody waiting for the outcome. For `Drop` only.
    fn submit_detached(&self, op: Op) {
        if let Some(conn) = &self.inline {
            let _ = self.apply_inline(conn, &op);
            return;
        }
        let _ = self.enqueue(op, None);
    }

    /// Apply one operation on the caller's thread: an in-memory ledger.
    fn apply_inline(&self, conn: &Mutex<Connection>, op: &Op) -> Result<Done, LedgerError> {
        let conn = conn.lock().unwrap_or_else(|p| p.into_inner());
        let result = guarded_batch(&conn, &self.tip, None, &[op], &mut |_| {})
            .pop()
            .unwrap_or(Err(LedgerError::Internal(
                "a batch of one returned nothing",
            )));
        self.progress.batches.fetch_add(1, Ordering::SeqCst);
        result
    }

    fn enqueue(
        &self,
        op: Op,
        reply: Option<futures_channel::oneshot::Sender<Result<Done, LedgerError>>>,
    ) -> Result<(), LedgerError> {
        let mut submitter = self.submitter.lock().unwrap_or_else(|p| p.into_inner());
        let ticket = submitter.next_ticket;
        let Some(tx) = &submitter.tx else {
            return Err(LedgerError::WriterStopped);
        };
        tx.send(Request { op, reply, ticket })
            .map_err(|_| LedgerError::WriterStopped)?;
        submitter.next_ticket += 1;
        Ok(())
    }

    /// Wait until every write submitted before this call has committed or been
    /// refused. Every read runs this first, so a read never misses a write this
    /// process submitted earlier, acknowledged or not.
    fn read_barrier(&self) {
        let last = {
            let submitter = self.submitter.lock().unwrap_or_else(|p| p.into_inner());
            submitter.next_ticket - 1
        };
        self.progress.wait_for(last);
    }

    /// Run `f` against a read connection, after the read barrier.
    fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        self.read_barrier();
        match &self.reads {
            Reads::Shared(conn) => {
                let conn = conn.lock().unwrap_or_else(|p| p.into_inner());
                f(&conn)
            }
            Reads::Pool {
                path,
                pool,
                returned,
            } => {
                let conn = {
                    let mut p = pool.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(conn) = p.idle.pop() {
                            break Some(conn);
                        }
                        if p.open < MAX_READERS {
                            // Reserve the slot, then open outside the lock.
                            p.open += 1;
                            break None;
                        }
                        p = returned.wait(p).unwrap_or_else(|e| e.into_inner());
                    }
                };
                let conn = match conn {
                    Some(conn) => conn,
                    None => match open_reader(path) {
                        Ok(conn) => conn,
                        Err(e) => {
                            pool.lock().unwrap_or_else(|p| p.into_inner()).open -= 1;
                            returned.notify_one();
                            return Err(e);
                        }
                    },
                };
                // Returned by `Drop`, so even a panic in `f` gives the slot back.
                let lent = LentReader {
                    conn: Some(conn),
                    pool,
                    returned,
                };
                let conn = lent
                    .conn
                    .as_ref()
                    .ok_or(LedgerError::Internal("no reader"))?;
                f(conn)
            }
        }
    }

    /// Open a call, writing its row and returning the permission to issue it.
    ///
    /// The row exists **before** the request goes out, so a process that dies
    /// mid-call leaves evidence that the call was attempted. It is also chained
    /// immediately (a `begin` event), so a call that vanishes entirely, its
    /// `calls` row deleted along with any later event, still leaves the `begin`
    /// event's sequence number as a hole `verify` can name.
    pub fn begin(
        &self,
        adapter: &'static str,
        protocol: Protocol,
        model: &str,
        agent_id: &str,
        task_id: &str,
    ) -> Result<CallToken<'_>, LedgerError> {
        self.begin_traced(
            adapter,
            protocol,
            model,
            agent_id,
            task_id,
            CallTrace::default(),
        )
    }

    /// [`Self::begin`] for an async caller. Submitted when called, like
    /// [`Self::begin_traced_async`].
    pub fn begin_async(
        &self,
        adapter: &'static str,
        protocol: Protocol,
        model: &str,
        agent_id: &str,
        task_id: &str,
    ) -> impl std::future::Future<Output = Result<CallToken<'_>, LedgerError>> + Send + '_ {
        self.begin_traced_async(
            adapter,
            protocol,
            model,
            agent_id,
            task_id,
            CallTrace::default(),
        )
    }

    /// Open a call carrying the trace fields: parent call, kind, role,
    /// and what the call was given (slice ids plus the prefix hash, never text).
    ///
    /// `ttft_ms` and `tool_rounds` are not arguments here because neither is known
    /// until after the call is issued; set them with
    /// [`CallToken::record_first_token_async`] and [`CallToken::record_tool_round_async`]
    /// before the token settles.
    pub fn begin_traced(
        &self,
        adapter: &'static str,
        protocol: Protocol,
        model: &str,
        agent_id: &str,
        task_id: &str,
        trace: CallTrace,
    ) -> Result<CallToken<'_>, LedgerError> {
        let id = self
            .begin_ack(adapter, protocol, model, agent_id, task_id, trace)
            .wait()?;
        Ok(self.token(id, adapter))
    }

    /// [`Self::begin_traced`] for an async caller.
    ///
    /// The `begin` is submitted when this is called. If the returned future is
    /// dropped before it resolves, the row is still written and, with no token
    /// to settle it, stays in flight, which [`Self::unreconciled`] reports, the
    /// same state a process that died mid-call leaves.
    pub fn begin_traced_async(
        &self,
        adapter: &'static str,
        protocol: Protocol,
        model: &str,
        agent_id: &str,
        task_id: &str,
        trace: CallTrace,
    ) -> impl std::future::Future<Output = Result<CallToken<'_>, LedgerError>> + Send + '_ {
        PendingBegin {
            ledger: self,
            adapter,
            ack: Some(self.begin_ack(adapter, protocol, model, agent_id, task_id, trace)),
        }
    }

    fn begin_ack(
        &self,
        adapter: &'static str,
        protocol: Protocol,
        model: &str,
        agent_id: &str,
        task_id: &str,
        trace: CallTrace,
    ) -> LedgerAck<CallId> {
        self.submit(
            Op::Begin {
                adapter,
                protocol,
                model: model.to_owned(),
                agent_id: agent_id.to_owned(),
                task_id: task_id.to_owned(),
                trace,
            },
            Done::call,
        )
    }

    fn token(&self, id: CallId, adapter: &'static str) -> CallToken<'_> {
        CallToken {
            ledger: self,
            id,
            settled: AtomicBool::new(false),
            adapter,
            stopped: AtomicBool::new(false),
            stop_probe: std::sync::OnceLock::new(),
        }
    }

    /// Resolve an unreconciled call by supplying the usage it turned out to have.
    ///
    /// Chained as its own `reconcile` event, distinct from the `settle` event
    /// that made the call unreconciled in the first place: a `UNIQUE(event,
    /// call_id)` index on `ledger_chain` means each of `begin`, `settle`,
    /// `reconcile` and `redact` can happen at most once per call, so a second
    /// reconciliation attempt is refused at the database layer rather than
    /// silently overwriting the first (this is also why the original
    /// single-event design broke here: it re-chained `settle`, retroactively
    /// invalidating the digest a prior verify had already accepted).
    pub fn reconcile(&self, id: &CallId, usage: &TokenUsage) -> Result<(), LedgerError> {
        self.submit(
            Op::Reconcile {
                id: id.clone(),
                usage: usage.clone(),
            },
            Done::unit,
        )
        .wait()
    }

    /// Remove a settled call's raw usage payload while keeping its `raw_hash`
    /// (needed to bind the redaction event to the pre-redaction content) and its
    /// place in the chain, and append a `redact` event naming why.
    pub fn redact(&self, id: &CallId, reason: &str) -> Result<(), LedgerError> {
        self.submit(
            Op::Redact {
                id: id.clone(),
                reason: reason.to_owned(),
            },
            Done::unit,
        )
        .wait()
    }

    /// Record one piece of MCP traffic (decision 0051: a remote MCP call is
    /// attributed as a provider call is). Writes an `mcp_calls` row and chains
    /// it with its own `mcp` event in the same transaction, so the row is
    /// covered by [`Self::verify`] exactly as a settled provider call is.
    ///
    /// **Why one event, not begin and settle.** A provider call is opened before
    /// the request leaves because it bills tokens and a process that dies
    /// mid-call must leave a trace of spend. MCP traffic costs no tokens
    /// directly; what 0051 asks for is that the egress is visible and
    /// attributed. The MCP client records each exchange once it has ended, with
    /// its outcome (success, protocol error, timeout, cancellation), so one
    /// immutable, chained row per exchange is the whole record.
    ///
    /// Returns the row's id.
    pub fn record_mcp_call(&self, call: &McpCall) -> Result<String, LedgerError> {
        self.record_mcp_call_async(call).wait()
    }

    /// [`Self::record_mcp_call`] for an async caller.
    pub fn record_mcp_call_async(&self, call: &McpCall) -> LedgerAck<String> {
        self.submit(Op::Mcp { call: call.clone() }, Done::mcp)
    }

    /// Whether a call is still marked `in_flight`. [`CallRecord`] reads such a row
    /// as unreconciled, so this is the one way to tell a call that may be live
    /// from one that ended without usage.
    pub fn is_in_flight(&self, id: &CallId) -> Result<bool, LedgerError> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM calls WHERE id = ?1 AND settlement = 'in_flight')",
                params![id.as_str()],
                |row| row.get(0),
            )?)
        })
    }

    /// One call by id.
    pub fn call(&self, id: &CallId) -> Result<Option<CallRecord>, LedgerError> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {CALL_COLUMNS} FROM calls WHERE id = ?1"),
                    params![id.as_str()],
                    record_from_row,
                )
                .optional()?)
        })
    }

    /// Every call, oldest first.
    ///
    /// A full scan. For a total or a per-group figure use the SQL aggregates
    /// ([`Self::spend_by`], [`Self::usage_by_task`],
    /// [`Self::spend_of`]) instead of folding this in Rust.
    pub fn calls(&self) -> Result<Vec<CallRecord>, LedgerError> {
        self.read(|conn| {
            let mut stmt =
                conn.prepare(&format!("SELECT {CALL_COLUMNS} FROM calls ORDER BY rowid"))?;
            let rows = stmt.query_map([], record_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// The median round time and median tokens per round for `route` (the
    /// catalog route id, e.g. "kimi", "kimi-anthropic": decision 0067, "The
    /// per-machine maximum", point 1's `T_r` and `tok_r`), for
    /// `kgraf-capacity::RouteInputs`.
    ///
    /// **Groups by the `route` column, not `adapter`** (fixed here): `adapter` is the wire
    /// adapter constant (for example `"openai-chat-completions"`), which
    /// several different catalog routes can share (Kimi and any other
    /// OpenAI-compatible vendor both speak it), so filtering by `adapter`
    /// mixed unrelated routes' round times together and, in practice, never
    /// matched any real route id at all (`route_capacity_inputs` was calling
    /// this with the route id, which the `adapter` column never contains).
    /// `route` is written by [`crate::ratelimit::RateLimited`] or
    /// [`crate::ratelimit::RouteRecorded`] once it knows its own route,
    /// separately from `begin`; a call made without going
    /// through either wrapper leaves it `NULL` and is correctly
    /// excluded here, the same as a route with too few rounds.
    ///
    /// **Approximation, stated plainly.** The design describes `T_r` as "the
    /// median time from one round's start to the next round's start for the
    /// route, taken from the ledger (`started_ms` along each worker's
    /// `parent_call` chain)". Walking the actual `parent_call` chain call by
    /// call would need a graph traversal this method does not do; instead,
    /// this groups every call by `agent_id` (one worker's own sequence of
    /// rounds already advances `started_ms` monotonically without needing
    /// `parent_call` to say so) and takes the median gap between
    /// consecutive `started_ms` values within each agent's own calls on this
    /// route, then the median of those per-agent medians across agents.
    /// Only settled, counted calls are read: a refused, failed or open call
    /// carries no usage and would drag both medians toward zero. A
    /// non-generative call (an embedding, rerank or decision request, told by
    /// its `call_kind`: `kgraf_core::is_non_generative_call_kind`) is left out
    /// too: it shares the route's limiter, and so its `route`, but it is not a
    /// round, and its short time and small token count would make the route
    /// look faster and cheaper per round than its workers are, which would let
    /// capacity admit more of them than the route can serve.
    /// `tok_r` is the median of `usage.uncached_input + usage.cache_read +
    /// usage.cache_write + usage.total_output()` across every counted call on
    /// the route. Both are
    /// `None` (design: "20 s is the default until then") until the route has
    /// at least [`ROUTE_STATS_MIN_ROUNDS`] calls.
    pub fn route_round_stats(&self, route: &str) -> Result<RouteRoundStats, LedgerError> {
        let calls = self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CALL_COLUMNS} FROM calls WHERE route = ?1 AND settlement = 'counted' ORDER BY agent_id, started_ms"
            ))?;
            let rows = stmt.query_map(rusqlite::params![route], record_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let calls: Vec<CallRecord> = calls
            .into_iter()
            .filter(|c| {
                !c.trace
                    .kind
                    .as_deref()
                    .is_some_and(kgraf_core::is_non_generative_call_kind)
            })
            .collect();
        let rounds = calls.len();
        if rounds < ROUTE_STATS_MIN_ROUNDS {
            return Ok(RouteRoundStats {
                round_ms: None,
                tokens_per_round: None,
                rounds,
            });
        }
        let mut per_agent_medians = Vec::new();
        let mut agent = None::<&str>;
        let mut gaps = Vec::new();
        let mut last_started: Option<i64> = None;
        for call in &calls {
            if agent != Some(call.agent_id.as_str()) {
                if gaps.len() >= 2 {
                    per_agent_medians.push(median_u64(&mut gaps));
                }
                gaps.clear();
                last_started = None;
                agent = Some(call.agent_id.as_str());
            }
            if let Some(prev) = last_started {
                let gap = (call.started_ms - prev).max(0) as u64;
                gaps.push(gap);
            }
            last_started = Some(call.started_ms);
        }
        if gaps.len() >= 2 {
            per_agent_medians.push(median_u64(&mut gaps));
        }
        let round_ms = (!per_agent_medians.is_empty()).then(|| median_u64(&mut per_agent_medians));
        let mut tokens: Vec<u64> = calls
            .iter()
            .map(|c| {
                c.usage.uncached_input
                    + c.usage.cache_read
                    + c.usage.cache_write
                    + c.usage.total_output()
            })
            .collect();
        let tokens_per_round = (!tokens.is_empty()).then(|| median_u64(&mut tokens));
        Ok(RouteRoundStats {
            round_ms,
            tokens_per_round,
            rounds,
        })
    }

    /// The latencies (`finished_ms - started_ms`, in milliseconds) of the
    /// newest `limit` settled, counted calls on `model`, newest first.
    /// `kinds: Some(list)` keeps only calls whose `call_kind` is in `list`;
    /// `None` keeps every generative call (a non-generative call kind, as in
    /// [`Self::route_round_stats`], is left out: an embedding's short time
    /// says nothing about how long a model takes to answer).
    ///
    /// A refused, failed or open call is left out: it has no full latency.
    /// Rows are read newest first and the read stops at `limit`, so the cost
    /// is bounded by how far back the newest `limit` matches lie.
    pub fn recent_latencies_ms(
        &self,
        model: &str,
        kinds: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<u64>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT call_kind, finished_ms - started_ms FROM calls
                 WHERE model = ?1 AND settlement = 'counted' AND finished_ms IS NOT NULL
                 ORDER BY started_ms DESC",
            )?;
            let rows = stmt.query_map(params![model], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                if out.len() >= limit {
                    break;
                }
                let (kind, ms) = row?;
                let keep = match kinds {
                    Some(list) => kind.as_deref().is_some_and(|k| list.contains(&k)),
                    None => !kind
                        .as_deref()
                        .is_some_and(kgraf_core::is_non_generative_call_kind),
                };
                if keep {
                    out.push(ms.max(0) as u64);
                }
            }
            Ok(out)
        })
    }

    /// The calls that finished at or after `since_ms` (or, unfinished, started
    /// then), oldest first. Reads through the `started_ms` index, with
    /// [`CALL_SPAN_SLACK_MS`] of slack for a call that started earlier and
    /// finished inside the window: the last hour's calls without a full scan.
    pub fn calls_finished_since(&self, since_ms: i64) -> Result<Vec<CallRecord>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CALL_COLUMNS} FROM calls
                 WHERE started_ms >= ?1 AND COALESCE(finished_ms, started_ms) >= ?2
                 ORDER BY rowid"
            ))?;
            let rows = stmt.query_map(
                rusqlite::params![since_ms - CALL_SPAN_SLACK_MS, since_ms],
                record_from_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// The calls attributed to `task` or to any task beneath it (`task/...`, the
    /// hierarchy an orchestrator run uses: its own calls carry the run id as the
    /// task, its workers' carry `run/contract`), plus, when `agent` is given,
    /// every call attributed to that agent. Oldest first.
    pub fn calls_under_task(
        &self,
        task: &str,
        agent: Option<&str>,
    ) -> Result<Vec<CallRecord>, LedgerError> {
        let prefix = format!("{task}/");
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CALL_COLUMNS} FROM calls
                 WHERE task_id = ?1
                    OR substr(task_id, 1, length(?2)) = ?2
                    OR (?3 IS NOT NULL AND agent_id = ?3)
                 ORDER BY rowid"
            ))?;
            let rows = stmt.query_map(params![task, prefix, agent], record_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Calls whose spend is unaccounted for.
    ///
    /// Surfaced rather than dropped, per the accounting rule and decision 0006's requirement
    /// that an interrupted stream be reconciled rather than lose its spend. Rows
    /// still in flight are included, because a process that died mid-call leaves
    /// exactly that state behind.
    pub fn unreconciled(&self) -> Result<Vec<CallRecord>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CALL_COLUMNS} FROM calls
                 WHERE settlement IN ('unreconciled', 'in_flight') ORDER BY rowid"
            ))?;
            let rows = stmt.query_map([], record_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Total usage across counted and cancelled calls, per model.
    ///
    /// Unreconciled calls are deliberately excluded from the total and reported
    /// separately: folding a zero into the sum would understate spend while looking
    /// complete.
    pub fn usage_by_model(&self) -> Result<Vec<(String, TokenUsage)>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT model,
                        SUM(uncached_input), SUM(cache_read), SUM(cache_write),
                        SUM(text_output), SUM(reasoning_output),
                        SUM(unsplit_reasoning_chars)
                 FROM calls WHERE settlement IN ('counted', 'cancelled')
                 GROUP BY model ORDER BY model",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, usage_from_sums(row, 1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Spend grouped by `key`, priced **per call** and summed (see
    /// [`Self::for_each_counted`] for why). A model the table cannot price is
    /// named in the group's `unpriced`, never costed at zero.
    ///
    /// **The groups need not sum to [`Self::total_cost`].** Each group's exact
    /// sum is rounded on its own, so a group that spent anything is never
    /// shown as $0. Rounding per group instead of once can put the groups'
    /// sum above the total by less than one micro-dollar per group, never
    /// below it by more than half a micro-dollar per group. A caller that needs
    /// shares summing to a total apportions the exact amounts instead
    /// (`kgraf_core::cost::ExactMoney::apportion`).
    pub fn spend_by(
        &self,
        key: GroupBy,
        prices: &PriceTable,
    ) -> Result<Vec<SpendGroup>, LedgerError> {
        let mut groups: BTreeMap<GroupKey, (SpendGroup, ExactMoney)> = BTreeMap::new();
        self.for_each_counted(group_key_sql(key), None, |k, model, route, usage| {
            let key = match key {
                GroupBy::Hour => GroupKey::HourStartMs(k.as_i64()?),
                GroupBy::Agent | GroupBy::Run => GroupKey::Name(k.as_str()?.to_owned()),
            };
            let (group, exact) = groups.entry(key.clone()).or_insert_with(|| {
                (
                    SpendGroup {
                        key,
                        cost: Money::ZERO,
                        calls: 0,
                        unpriced: Vec::new(),
                    },
                    ExactMoney::ZERO,
                )
            });
            group.calls += 1;
            match prices.cost_exact_on(route, model, usage) {
                Ok(cost) => *exact += cost,
                Err(_) => {
                    if !group.unpriced.iter().any(|m| m == model) {
                        group.unpriced.push(model.to_owned());
                    }
                }
            }
            Ok(())
        })?;
        Ok(groups
            .into_values()
            .map(|(mut group, exact)| {
                group.cost = exact.to_money();
                group
            })
            .collect())
    }

    /// Visit every counted or cancelled call (optionally only one agent's), with
    /// the value of `key_sql` for its row, its model and its usage.
    ///
    /// **The one rounding rule for spend.** Every total ([`Self::spend_of`],
    /// [`Self::total_cost`], [`Self::cost_for_agent`], [`Self::spend_by`])
    /// prices each call exactly in pico-dollars
    /// ([`PriceTable::cost_exact_on`]), sums those, and rounds once
    /// ([`ExactMoney::to_money`]). Each call is still priced at its own size
    /// (a tier applies per call). A total therefore never loses the calls too
    /// small to show one micro-dollar each: a thousand ten-token embedding
    /// calls total their real cost, not zero. A single call shown alone rounds
    /// up to one micro-dollar rather than to $0, so a list of such calls can
    /// sum to more than its total by under one micro-dollar per call. The
    /// rows are streamed from a read connection, never under the write lock.
    fn for_each_counted(
        &self,
        key_sql: &str,
        agent: Option<&str>,
        mut visit: impl FnMut(
            rusqlite::types::ValueRef<'_>,
            &str,
            Option<&str>,
            &TokenUsage,
        ) -> Result<(), rusqlite::types::FromSqlError>,
    ) -> Result<(), LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {key_sql}, model, route,
                        uncached_input, cache_read, cache_write, text_output, reasoning_output,
                        unsplit_reasoning_chars
                 FROM calls
                 WHERE settlement IN ('counted', 'cancelled') AND (?1 IS NULL OR agent_id = ?1)"
            ))?;
            let mut rows = stmt.query(params![agent])?;
            while let Some(row) = rows.next()? {
                let model: String = row.get(1)?;
                let route: Option<String> = row.get(2)?;
                let usage = usage_from_sums(row, 3)?;
                visit(row.get_ref(0)?, &model, route.as_deref(), &usage).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Null,
                        Box::new(e),
                    )
                })?;
            }
            Ok(())
        })
    }

    /// Per-task usage history, in SQL, for sizing: for every `(model, task)`
    /// the number of calls, the input of the first call (by start time), the
    /// largest input of any call, and the summed usage. Input is uncached plus
    /// cache read plus cache write. Every settlement is included, as a sizing
    /// history counts rounds, not spend. Calls whose agent starts with
    /// `exclude_agent_prefix` are left out.
    pub fn usage_by_task(
        &self,
        exclude_agent_prefix: Option<&str>,
    ) -> Result<Vec<TaskUsage>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "WITH w AS (
                    SELECT model, task_id, uncached_input, cache_read, cache_write,
                           text_output, reasoning_output, unsplit_reasoning_chars,
                           uncached_input + cache_read + cache_write AS input,
                           ROW_NUMBER() OVER (
                               PARTITION BY model, task_id ORDER BY started_ms, rowid
                           ) AS rn
                    FROM calls
                    WHERE ?1 IS NULL OR substr(agent_id, 1, length(?1)) <> ?1
                 )
                 SELECT model, task_id, COUNT(*),
                        MAX(CASE WHEN rn = 1 THEN input END), MAX(input),
                        SUM(uncached_input), SUM(cache_read), SUM(cache_write),
                        SUM(text_output), SUM(reasoning_output),
                        SUM(unsplit_reasoning_chars)
                 FROM w GROUP BY model, task_id ORDER BY model, task_id",
            )?;
            let rows = stmt.query_map(params![exclude_agent_prefix], |row| {
                Ok(TaskUsage {
                    model: row.get(0)?,
                    task_id: row.get(1)?,
                    calls: row.get::<_, i64>(2)? as u64,
                    first_input: row.get::<_, i64>(3)? as u64,
                    peak_input: row.get::<_, i64>(4)? as u64,
                    usage: usage_from_sums(row, 5)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Spend of many calls in one query. A counted call, or a cancelled one with
    /// usage, is priced; a failed call is zero; a call that is missing, still in
    /// flight, unreconciled, cancelled before any usage was reported, or on a
    /// model the table cannot price has spend that is **unknown**, and is named
    /// in `unknown` rather than counted as zero. An id given twice is counted
    /// twice, as the caller listed it.
    pub fn spend_of(&self, ids: &[CallId], prices: &PriceTable) -> Result<SpendOf, LedgerError> {
        let wanted = serde_json::to_string(&ids.iter().map(CallId::as_str).collect::<Vec<_>>())
            .map_err(|_| LedgerError::Internal("cannot encode the call ids"))?;
        type Row = (String, String, Option<String>, TokenUsage);
        let rows: BTreeMap<String, Row> = self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, settlement, model, route,
                        uncached_input, cache_read, cache_write, text_output, reasoning_output,
                        unsplit_reasoning_chars
                 FROM calls WHERE id IN (SELECT value FROM json_each(?1))",
            )?;
            let rows = stmt.query_map(params![wanted], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        usage_from_sums(row, 4)?,
                    ),
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
        })?;
        let mut out = SpendOf {
            total: Money::ZERO,
            unknown: Vec::new(),
        };
        let mut exact = ExactMoney::ZERO;
        for id in ids {
            match rows.get(id.as_str()) {
                Some((settlement, model, route, usage))
                    if settlement == "counted"
                        || (settlement == "cancelled" && !usage.is_empty()) =>
                {
                    match prices.cost_exact_on(route.as_deref(), model, usage) {
                        Ok(cost) => exact += cost,
                        Err(_) => out.unknown.push(id.clone()),
                    }
                }
                Some((settlement, _, _, _)) if settlement == "failed" => {}
                _ => out.unknown.push(id.clone()),
            }
        }
        out.total = exact.to_money();
        Ok(out)
    }

    /// Total spend, derived from a price table at read time, priced per call
    /// (see [`Self::for_each_counted`]).
    ///
    /// Returns the total and the models that could not be costed (sorted),
    /// rather than costing an unknown model at zero.
    pub fn total_cost(&self, prices: &PriceTable) -> Result<(Money, Vec<String>), LedgerError> {
        let mut total = ExactMoney::ZERO;
        let mut unpriced = std::collections::BTreeSet::new();
        self.for_each_counted("NULL", None, |_, model, route, usage| {
            match prices.cost_exact_on(route, model, usage) {
                Ok(cost) => total += cost,
                Err(_) => {
                    unpriced.insert(model.to_owned());
                }
            }
            Ok(())
        })?;
        Ok((total.to_money(), unpriced.into_iter().collect()))
    }

    /// Spend attributed to one agent, priced per call (see
    /// [`Self::for_each_counted`]). An unpriced model is an error, not zero.
    pub fn cost_for_agent(
        &self,
        agent_id: &str,
        prices: &PriceTable,
    ) -> Result<Money, LedgerError> {
        let mut total = ExactMoney::ZERO;
        let mut failed: Option<kgraf_core::cost::CostError> = None;
        self.for_each_counted("NULL", Some(agent_id), |_, model, route, usage| {
            if failed.is_none() {
                match prices.cost_exact_on(route, model, usage) {
                    Ok(cost) => total += cost,
                    Err(e) => failed = Some(e),
                }
            }
            Ok(())
        })?;
        match failed {
            Some(e) => Err(e.into()),
            None => Ok(total.to_money()),
        }
    }

    /// Every recorded MCP exchange, oldest first.
    pub fn mcp_calls(&self) -> Result<Vec<McpCallRecord>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {MCP_COLUMNS} FROM mcp_calls ORDER BY rowid"
            ))?;
            let rows = stmt.query_map([], mcp_record_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// One-time migration for a database that predates the chain: chain every
    /// already-settled row that has no chain event, in `rowid` order (the only
    /// order recorded before the chain existed), then advance `PRAGMA
    /// user_version` so this never runs again.
    ///
    /// Gated, not idempotent-by-inspection: the `atk_inserted_row_
    /// laundered_by_backfill` fixture inserts a forged row by hand between two
    /// opens specifically to see whether a reopen quietly re-chains it. The old
    /// code ran this unconditionally on every `open` and did exactly that; this
    /// version runs it once, ever, and after that a settled row with no chain
    /// event is something [`Self::verify`] reports as forged, not something a
    /// later `open` heals.
    fn migrate_chain_once(conn: &Connection) -> Result<(), LedgerError> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= 1 {
            return Ok(());
        }
        with_immediate(conn, || {
            let ids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT id FROM calls
                     WHERE id NOT IN (SELECT call_id FROM ledger_chain)
                     ORDER BY rowid",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut tip = read_chain_tail(conn)?;
            for id in ids {
                let id = CallId::new(id);
                // A legacy row predates `raw_hash`; derive it once from whatever
                // `raw_usage` it already holds, so its digest is computable the
                // same way a post-chain row's is.
                let mut fields = raw_row(conn, &id)?
                    .ok_or_else(|| LedgerError::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
                if fields.raw_hash.is_none() {
                    let derived = hash_raw_usage(fields.raw_usage.as_deref());
                    conn.execute(
                        "UPDATE calls SET raw_hash = ?2 WHERE id = ?1",
                        params![id.as_str(), derived],
                    )?;
                    fields.raw_hash = derived;
                }
                let event = if fields.settlement == "in_flight" {
                    "begin"
                } else {
                    "settle"
                };
                append_chain(conn, &mut tip, event, &id, &content_digest(&fields), None)?;
            }
            conn.execute("PRAGMA user_version = 1", [])?;
            Ok(())
        })
    }

    /// Verify the chain end to end. Reports the first broken row it finds, named
    /// by sequence number (fail loud), and separately names every redacted row
    /// (present or not) so a clean report never implies nothing was ever
    /// removed.
    ///
    /// Runs on a read connection inside one read transaction, so it checks one
    /// consistent snapshot and never holds the write lock. The on-disk anchor is
    /// read **before** the snapshot starts: the anchor is published only after
    /// its sequence commits, so the snapshot always contains what it names.
    ///
    /// See the module docs for exactly what this can and cannot prove.
    pub fn verify(&self) -> Result<VerifyReport, LedgerError> {
        let file_anchor = self.tip_path.as_deref().map(read_tip_file);
        self.read(|conn| {
            conn.execute_batch("BEGIN")?;
            let anchor = match &file_anchor {
                Some(anchor) => anchor.clone(),
                // In memory: `Reads::Shared` holds the one connection's mutex,
                // which the writer also holds while it commits and moves the
                // tip, so this tip matches what this read sees.
                None => Some(self.tip.lock().unwrap_or_else(|p| p.into_inner()).clone()),
            };
            let report = verify_snapshot(conn, anchor)?;
            conn.execute_batch("COMMIT")?;
            Ok(report)
        })
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        // Shutdown drains the queue: closing the channel lets the writer take
        // every operation already queued (a channel delivers everything sent
        // before it reports disconnection), commit it, and only then exit.
        self.submitter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .tx
            .take();
        if let Some(handle) = self.writer.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = handle.join();
        }
        // Then flush and stop the anchor writer thread before this value is
        // gone. Without this, a process that opens a ledger, settles a call, and
        // exits immediately could exit before the writer thread ever got to
        // run, leaving the anchor stale until the next write, survivable
        // (`verify` tolerates the lag), but there is no reason to leave it
        // stale when the ledger itself already knows it is closing.
        if let Some(anchor) = &self.anchor {
            anchor.shutdown();
        }
    }
}

/// The writer's configuration. Only fault injection, and only in tests (or
/// `test-support`).
#[derive(Clone, Copy, Default)]
struct WriterConfig {
    #[cfg(any(test, feature = "test-support"))]
    faults: WriterFaults,
}

/// The writer thread: drain, apply, commit once, acknowledge.
fn run_writer(
    rx: std::sync::mpsc::Receiver<Request>,
    conn: Arc<Mutex<Connection>>,
    tip: Arc<Mutex<Tip>>,
    anchor: Option<Arc<AnchorWriter>>,
    progress: Arc<Progress>,
    config: WriterConfig,
) {
    let _stop = StopOnExit(progress.clone());
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = config;
    let mut applied: u64 = 0;
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(request) => batch.push(request),
                Err(_) => break,
            }
        }
        let last_ticket = batch.last().map_or(0, |r| r.ticket);
        let results = {
            let conn = conn.lock().unwrap_or_else(|p| p.into_inner());
            let ops: Vec<&Op> = batch.iter().map(|r| &r.op).collect();
            let mut hook = |_n: usize| {
                applied += 1;
                #[cfg(any(test, feature = "test-support"))]
                {
                    if config.faults.abort_after_op == Some(applied) {
                        std::process::abort();
                    }
                    if config.faults.panic_after_op == Some(applied) {
                        panic!("injected writer fault after operation {applied}");
                    }
                }
            };
            guarded_batch(&conn, &tip, anchor.as_deref(), &ops, &mut hook)
        };
        progress.batches.fetch_add(1, Ordering::SeqCst);
        // A `begin` whose caller is gone (its future was dropped while this
        // batch committed: a cancel, a stopped worker, a time limit) minted a
        // row no token will ever settle. It is settled here, unreconciled, as
        // a dropped token settles its own (`CallToken::drop`), so no reserved
        // call stays in flight for good. This is the writer's half of
        // `PendingBegin`'s guard: a reply sent before the caller closed it is
        // settled on the caller's side instead.
        let mut orphaned = Vec::new();
        for (request, result) in batch.into_iter().zip(results) {
            if let Some(reply) = request.reply
                && let Err(Ok(Done::Call(id))) = reply.send(result)
                && matches!(request.op, Op::Begin { .. })
            {
                orphaned.push(Op::Settle {
                    id,
                    settlement: Settlement::Unreconciled,
                    usage: None,
                    request_id: None,
                });
            }
        }
        if !orphaned.is_empty() {
            let conn = conn.lock().unwrap_or_else(|p| p.into_inner());
            let ops: Vec<&Op> = orphaned.iter().collect();
            // A settle that fails leaves the row in flight, which
            // `unreconciled` reports, as for a dropped token's settle.
            let _ = guarded_batch(&conn, &tip, anchor.as_deref(), &ops, &mut |_| {});
            progress.batches.fetch_add(1, Ordering::SeqCst);
        }
        // After the replies and any orphan settle: a read whose barrier covers
        // this batch then also sees those settles.
        progress.advance(last_ticket);
    }
}

/// [`apply_batch`], with a panic contained: a panic inside one batch must not
/// leave the transaction open (a later reader of an in-memory ledger shares this
/// connection) nor strand the callers waiting on it. It fails that batch,
/// loudly, and the writer goes on serving the next one.
fn guarded_batch(
    conn: &Connection,
    tip: &Mutex<Tip>,
    anchor: Option<&AnchorWriter>,
    ops: &[&Op],
    after_op: &mut dyn FnMut(usize),
) -> Vec<Result<Done, LedgerError>> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply_batch(conn, tip, anchor, ops, after_op)
    })) {
        Ok(results) => results,
        Err(_) => {
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            ops.iter()
                .map(|_| {
                    Err(LedgerError::BatchFailed {
                        reason: "the ledger writer panicked mid-batch; nothing in the batch \
                                 was committed"
                            .to_owned(),
                    })
                })
                .collect()
        }
    }
}

/// Apply `ops` in order inside one `BEGIN IMMEDIATE ... COMMIT`.
///
/// Returns one result per op. An op's `Ok` is only returned if the commit
/// succeeded; if it failed, every op in the batch gets an error.
fn apply_batch(
    conn: &Connection,
    shared_tip: &Mutex<Tip>,
    anchor: Option<&AnchorWriter>,
    ops: &[&Op],
    after_op: &mut dyn FnMut(usize),
) -> Vec<Result<Done, LedgerError>> {
    let fail_all = |reason: String| -> Vec<Result<Done, LedgerError>> {
        ops.iter()
            .map(|_| {
                Err(LedgerError::BatchFailed {
                    reason: reason.clone(),
                })
            })
            .collect()
    };
    if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
        return fail_all(format!("cannot begin the write transaction: {e}"));
    }
    // Re-read inside the write lock: another process may have appended since
    // this one last committed.
    let mut tip = match read_chain_tail(conn) {
        Ok(tip) => tip,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return fail_all(format!("cannot read the chain tip: {e}"));
        }
    };
    let start_seq = tip.seq;
    let mut results = Vec::with_capacity(ops.len());
    for (n, op) in ops.iter().enumerate() {
        let before = tip.clone();
        let result = conn
            .prepare_cached("SAVEPOINT op")
            .and_then(|mut st| st.execute([]))
            .map(|_| ())
            .map_err(LedgerError::from)
            .and_then(|()| apply_op(conn, &mut tip, op));
        let error = match result {
            Ok(done) => match conn
                .prepare_cached("RELEASE op")
                .and_then(|mut st| st.execute([]))
            {
                Ok(_) => {
                    results.push(Ok(done));
                    None
                }
                Err(e) => Some(LedgerError::from(e)),
            },
            Err(e) => Some(e),
        };
        if let Some(error) = error {
            // On SQLITE_FULL, IOERR or NOMEM SQLite may roll back the whole
            // transaction, not just the statement. Then `ROLLBACK TO` fails, the
            // connection is back in autocommit, and every later op would commit
            // on its own, chained onto a tip that includes events that no longer
            // exist So: detect it, and fail the batch.
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK TO op; RELEASE op");
            }
            if conn.is_autocommit() {
                let reason =
                    format!("SQLite rolled back the whole batch transaction after: {error}");
                reload_tip(conn, shared_tip);
                let mut out: Vec<Result<Done, LedgerError>> = results
                    .into_iter()
                    .map(|_| {
                        Err(LedgerError::BatchFailed {
                            reason: reason.clone(),
                        })
                    })
                    .collect();
                out.push(Err(error));
                out.extend(ops[n + 1..].iter().map(|_| {
                    Err(LedgerError::BatchFailed {
                        reason: reason.clone(),
                    })
                }));
                return out;
            }
            tip = before;
            results.push(Err(error));
        }
        after_op(n);
    }
    if let Err(e) = conn.execute_batch("COMMIT") {
        if !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        reload_tip(conn, shared_tip);
        let reason = format!("the batch did not commit: {e}");
        return results
            .into_iter()
            .map(|r| {
                r.and(Err(LedgerError::BatchFailed {
                    reason: reason.clone(),
                }))
            })
            .collect();
    }
    // Published only after commit, so the anchor never names a sequence that
    // could still roll back. Once per batch: the anchor keeps only the latest
    // tip anyway.
    if tip.seq != start_seq
        && let Some(anchor) = anchor
    {
        anchor.publish(tip.clone());
    }
    *shared_tip.lock().unwrap_or_else(|p| p.into_inner()) = tip;
    results
}

/// After a batch that did not commit, set the shared tip from what the database
/// actually holds. The next batch re-reads it anyway; this keeps the in-memory
/// anchor [`Ledger::verify`] uses for an in-memory ledger truthful meanwhile.
fn reload_tip(conn: &Connection, shared_tip: &Mutex<Tip>) {
    if let Ok(tip) = read_chain_tail(conn) {
        *shared_tip.lock().unwrap_or_else(|p| p.into_inner()) = tip;
    }
}

fn apply_op(conn: &Connection, tip: &mut Tip, op: &Op) -> Result<Done, LedgerError> {
    match op {
        Op::Begin {
            adapter,
            protocol,
            model,
            agent_id,
            task_id,
            trace,
        } => apply_begin(
            conn, tip, adapter, *protocol, model, agent_id, task_id, trace,
        )
        .map(Done::Call),
        Op::Settle {
            id,
            settlement,
            usage,
            request_id,
        } => {
            let fields = apply_settlement(
                conn,
                id,
                *settlement,
                SettleFrom::InFlight,
                usage.as_ref(),
                request_id.as_deref(),
            )?;
            append_chain(conn, tip, "settle", id, &content_digest(&fields), None)?;
            Ok(Done::Unit)
        }
        Op::Reconcile { id, usage } => {
            let fields = apply_settlement(
                conn,
                id,
                Settlement::Counted,
                SettleFrom::Open,
                Some(usage),
                None,
            )?;
            append_chain(conn, tip, "reconcile", id, &content_digest(&fields), None)?;
            Ok(Done::Unit)
        }
        Op::Redact { id, reason } => {
            apply_redact(conn, tip, id, reason)?;
            Ok(Done::Unit)
        }
        Op::Ttft { id, ttft_ms } => {
            apply_ttft(conn, id, *ttft_ms)?;
            Ok(Done::Unit)
        }
        Op::Route { id, route } => {
            apply_route(conn, id, route)?;
            Ok(Done::Unit)
        }
        Op::ToolRound { id, tools } => {
            apply_tool_round(conn, id, tools)?;
            Ok(Done::Unit)
        }
        Op::Mcp { call } => apply_mcp(conn, tip, call).map(Done::Mcp),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_begin(
    conn: &Connection,
    tip: &mut Tip,
    adapter: &str,
    protocol: Protocol,
    model: &str,
    agent_id: &str,
    task_id: &str,
    trace: &CallTrace,
) -> Result<CallId, LedgerError> {
    let now = now_ms();
    // Inside the batch's transaction, so a `begin` earlier in the same batch is
    // already counted and two begins never share a sequence.
    let next: i64 = conn
        .prepare_cached("SELECT COALESCE(MAX(rowid), 0) + 1 FROM calls")?
        .query_row([], |row| row.get(0))?;
    let id = CallId::new(kgraf_core::ids::stable_id(
        "call",
        &[
            adapter,
            model,
            agent_id,
            task_id,
            &now.to_string(),
            &next.to_string(),
        ],
    ));
    let slice_ids = json_string_array(&trace.given_slice_ids);
    let parent_call = trace.parent_call.as_ref().map(CallId::as_str);
    conn.prepare_cached(
        "INSERT INTO calls(id, adapter, protocol, model, agent_id, task_id, settlement,
                           uncached_input, cache_read, cache_write, text_output,
                           reasoning_output, raw_usage, raw_hash, request_id, started_ms,
                           finished_ms, parent_call, call_kind, call_role, ttft_ms,
                           tool_rounds, given_slice_ids, given_prefix_hash, redacted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'in_flight', 0, 0, 0, 0, 0, NULL, NULL, NULL, ?7,
                 NULL, ?8, ?9, ?10, NULL, '[]', ?11, ?12, 0)",
    )?
    .execute(params![
        id.as_str(),
        adapter,
        protocol.as_str(),
        model,
        agent_id,
        task_id,
        now,
        parent_call,
        trace.kind,
        trace.role,
        slice_ids,
        trace.given_prefix_hash,
    ])?;
    // Computed from the values just written, in hand, rather than a `SELECT`
    // re-reading what this same statement just inserted (the reuse rule).
    let fields = RawFields {
        id: id.as_str().to_owned(),
        adapter: adapter.to_owned(),
        protocol: protocol.as_str().to_owned(),
        model: model.to_owned(),
        agent_id: agent_id.to_owned(),
        task_id: task_id.to_owned(),
        settlement: "in_flight".to_owned(),
        uncached_input: 0,
        cache_read: 0,
        cache_write: 0,
        text_output: 0,
        reasoning_output: 0,
        raw_usage: None,
        raw_hash: None,
        request_id: None,
        started_ms: now,
        finished_ms: None,
        parent_call: parent_call.map(str::to_owned),
        call_kind: trace.kind.clone(),
        call_role: trace.role.clone(),
        ttft_ms: None,
        tool_rounds: "[]".to_owned(),
        given_slice_ids: slice_ids,
        given_prefix_hash: trace.given_prefix_hash.clone(),
        redacted: 0,
        unsplit_reasoning_chars: None,
    };
    append_chain(conn, tip, "begin", &id, &content_digest(&fields), None)?;
    Ok(id)
}

/// Set the time to first token for a call that has not settled yet. Refused
/// (rather than silently ignored) once the row is no longer `in_flight`: see
/// [`CallToken::record_first_token_async`] for why the caller still treats that
/// refusal as best-effort.
fn apply_ttft(conn: &Connection, id: &CallId, ttft_ms: i64) -> Result<(), LedgerError> {
    let changed = conn
        .prepare_cached(
            "UPDATE calls SET ttft_ms = ?2
         WHERE id = ?1 AND ttft_ms IS NULL AND settlement = 'in_flight'",
        )?
        .execute(params![id.as_str(), ttft_ms])?;
    if changed == 0 {
        return Err(LedgerError::NotInFlight {
            call: id.to_string(),
            what: "time to first token",
        });
    }
    Ok(())
}

/// Record (or correct) the catalog route id a call was made on. Unlike
/// [`apply_ttft`], re-setting the same value is not an error (a rebuilt
/// provider for one route rebinding its `RouteLimiter`, decision 0072,
/// records the same route on every call it wraps, not once).
fn apply_route(conn: &Connection, id: &CallId, route: &str) -> Result<(), LedgerError> {
    let changed = conn
        .prepare_cached("UPDATE calls SET route = ?2 WHERE id = ?1 AND settlement = 'in_flight'")?
        .execute(params![id.as_str(), route])?;
    if changed == 0 {
        return Err(LedgerError::NotInFlight {
            call: id.to_string(),
            what: "route",
        });
    }
    Ok(())
}

/// Append one round's ordered tool list to a call that has not settled yet.
///
/// Fails loud rather than silently on a corrupt existing `tool_rounds`
/// column (the fail-loud and test-the-mechanism rules): the naive
/// `unwrap_or_default` this replaced would have overwritten a corrupted
/// column with a single-round array, destroying whatever real history
/// preceded the corruption without a trace.
fn apply_tool_round(conn: &Connection, id: &CallId, tools: &[String]) -> Result<(), LedgerError> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT tool_rounds, settlement FROM calls WHERE id = ?1",
            params![id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((existing, settlement)) = row else {
        return Err(LedgerError::NotInFlight {
            call: id.to_string(),
            what: "a tool round (no such call)",
        });
    };
    if settlement != "in_flight" {
        return Err(LedgerError::NotInFlight {
            call: id.to_string(),
            what: "a tool round",
        });
    }
    let mut rounds: Vec<Vec<String>> =
        serde_json::from_str(&existing).map_err(|_| LedgerError::CorruptTrace {
            call: id.to_string(),
        })?;
    rounds.push(tools.to_vec());
    let updated = serde_json::to_string(&rounds).map_err(|_| LedgerError::CorruptTrace {
        call: id.to_string(),
    })?;
    let changed = conn
        .prepare_cached(
            "UPDATE calls SET tool_rounds = ?2 WHERE id = ?1 AND settlement = 'in_flight'",
        )?
        .execute(params![id.as_str(), updated])?;
    if changed == 0 {
        return Err(LedgerError::NotInFlight {
            call: id.to_string(),
            what: "a tool round",
        });
    }
    Ok(())
}

/// Apply a settlement's field values to `calls` and return the row's new raw
/// fields. The identity and trace fields are unchanged by a settlement and not
/// in hand here, so the row is read back once (the accepted remainder).
/// The states a settlement write may start from. The UPDATE carries the
/// predicate, so a resolve racing a live settle cannot overwrite it.
#[derive(Clone, Copy)]
enum SettleFrom {
    /// A live call settling: only an `in_flight` row.
    InFlight,
    /// A reconcile: an `unreconciled` row, or an `in_flight` one the caller has
    /// decided is abandoned.
    Open,
}

fn apply_settlement(
    conn: &Connection,
    id: &CallId,
    settlement: Settlement,
    from: SettleFrom,
    usage: Option<&TokenUsage>,
    request_id: Option<&str>,
) -> Result<RawFields, LedgerError> {
    let empty = TokenUsage::default();
    let usage = usage.unwrap_or(&empty);
    let raw_usage = usage.raw.as_ref().map(|r| r.to_string());
    let raw_hash = hash_raw_usage(raw_usage.as_deref());
    let finished_ms = now_ms();

    let changed = conn
        .prepare_cached(
            "UPDATE calls SET settlement = ?2,
                          uncached_input = ?3, cache_read = ?4, cache_write = ?5,
                          text_output = ?6, reasoning_output = ?7,
                          raw_usage = ?8, raw_hash = ?9,
                          request_id = COALESCE(?10, request_id),
                          finished_ms = ?11,
                          unsplit_reasoning_chars = ?14
         WHERE id = ?1 AND settlement IN (?12, ?13)",
        )?
        .execute(params![
            id.as_str(),
            settlement.as_str(),
            usage.uncached_input as i64,
            usage.cache_read as i64,
            usage.cache_write as i64,
            usage.text_output as i64,
            usage.reasoning_output as i64,
            raw_usage,
            raw_hash,
            request_id,
            finished_ms,
            "in_flight",
            match from {
                SettleFrom::InFlight => "in_flight",
                SettleFrom::Open => "unreconciled",
            },
            usage.unsplit_reasoning_chars.map(|n| n as i64),
        ])?;
    if changed == 0 {
        let state = raw_row(conn, id)?.map(|r| r.settlement);
        return Err(match state {
            None => LedgerError::NotRedactable {
                call: id.to_string(),
                reason: "no such call",
            },
            Some(state) => LedgerError::WrongState {
                call: id.to_string(),
                state,
                wanted: match from {
                    SettleFrom::InFlight => "only an in-flight call can be settled",
                    SettleFrom::Open => "only an open call can be reconciled",
                },
            },
        });
    }

    // The request id may have been `COALESCE`d against the prior value; the
    // read-back reflects that.
    raw_row(conn, id)?.ok_or_else(|| LedgerError::NotRedactable {
        call: id.to_string(),
        reason: "vanished mid-settlement",
    })
}

fn apply_redact(
    conn: &Connection,
    tip: &mut Tip,
    id: &CallId,
    reason: &str,
) -> Result<(), LedgerError> {
    let existing = raw_row(conn, id)?.ok_or_else(|| LedgerError::NotRedactable {
        call: id.to_string(),
        reason: "no such call",
    })?;
    if existing.settlement == "in_flight" {
        return Err(LedgerError::NotRedactable {
            call: id.to_string(),
            reason: "not settled yet, so it has no chain event to preserve",
        });
    }
    if existing.redacted != 0 {
        return Err(LedgerError::AlreadyRedacted {
            call: id.to_string(),
        });
    }
    let has_event: bool = conn
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM ledger_chain WHERE call_id = ?1)")?
        .query_row(params![id.as_str()], |row| row.get(0))?;
    if !has_event {
        return Err(LedgerError::NotRedactable {
            call: id.to_string(),
            reason: "not yet chained",
        });
    }

    conn.prepare_cached("UPDATE calls SET raw_usage = NULL, redacted = 1 WHERE id = ?1")?
        .execute(params![id.as_str()])?;
    let fields = RawFields {
        raw_usage: None,
        redacted: 1,
        ..existing
    };
    append_chain(
        conn,
        tip,
        "redact",
        id,
        &content_digest(&fields),
        Some(reason),
    )
}

fn apply_mcp(conn: &Connection, tip: &mut Tip, call: &McpCall) -> Result<String, LedgerError> {
    let recorded_ms = now_ms();
    let seq: i64 = conn
        .prepare_cached("SELECT COALESCE(MAX(rowid), 0) + 1 FROM mcp_calls")?
        .query_row([], |row| row.get(0))?;
    let id = format!(
        "mcp-{}",
        kgraf_core::ids::stable_id(
            "mcp",
            &[
                &call.server,
                call.tool.as_deref().unwrap_or(""),
                &call.kind,
                call.agent_id.as_deref().unwrap_or(""),
                call.task_id.as_deref().unwrap_or(""),
                &recorded_ms.to_string(),
                &seq.to_string(),
            ],
        )
    );
    let record = McpCallRecord {
        id: id.clone(),
        recorded_ms,
        call: call.clone(),
    };
    conn.prepare_cached(
        "INSERT INTO mcp_calls(id, server, tool, kind, agent_id, task_id, duration_ms,
                               outcome, tool_is_error, bytes_in, bytes_out, recorded_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?
    .execute(params![
        id,
        call.server,
        call.tool,
        call.kind,
        call.agent_id,
        call.task_id,
        call.duration_ms,
        call.outcome,
        call.tool_is_error.map(i64::from),
        call.bytes_in,
        call.bytes_out,
        recorded_ms,
    ])?;
    append_chain(
        conn,
        tip,
        "mcp",
        &CallId::new(id.clone()),
        &mcp_digest(&record),
        None,
    )?;
    Ok(id)
}

/// Read the chain's current tail directly from `ledger_chain`: at open, and at
/// the start of every writer batch, inside its write lock.
fn read_chain_tail(conn: &Connection) -> Result<Tip, LedgerError> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq, chain_hash FROM ledger_chain ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((seq, chain_hash)) => Tip { seq, chain_hash },
        None => Tip::genesis(),
    })
}

/// Append one `ledger_chain` row, chaining it onto `tip`, and advance `tip`.
///
/// Always called inside a write transaction whose `tip` was read from the
/// database under that same write lock (the writer's batch, or the one-time
/// back-fill), so it cannot chain onto a stale tail left by another process
/// (verified by `two_connections_settling_different_calls_concurrently_both_
/// succeed`).
fn append_chain(
    conn: &Connection,
    tip: &mut Tip,
    event: &str,
    call_id: &CallId,
    digest: &str,
    reason: Option<&str>,
) -> Result<(), LedgerError> {
    let mut h = blake3::Hasher::new();
    hash_str(&mut h, &tip.chain_hash);
    hash_str(&mut h, digest);
    let chain_hash = h.finalize().to_hex().to_string();
    conn.prepare_cached(
        "INSERT INTO ledger_chain(event, call_id, digest, chain_hash, reason, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        event,
        call_id.as_str(),
        digest,
        chain_hash,
        reason,
        now_ms()
    ])?;
    tip.seq = conn.last_insert_rowid();
    tip.chain_hash = chain_hash;
    Ok(())
}

/// The SQL expression a [`GroupBy`] groups on.
fn group_key_sql(key: GroupBy) -> &'static str {
    match key {
        GroupBy::Agent => "agent_id",
        GroupBy::Run => {
            "CASE WHEN instr(task_id, '/') > 0
                  THEN substr(task_id, 1, instr(task_id, '/') - 1)
                  ELSE task_id END"
        }
        GroupBy::Hour => "started_ms - (started_ms % 3600000)",
    }
}

/// A read-only connection to an on-disk ledger's WAL database.
fn open_reader(path: &Path) -> Result<Connection, LedgerError> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// Six summed usage columns starting at `first`, as a `TokenUsage`. The sixth
/// is the unsplit reasoning characters. A `SUM` over no rows is `NULL`, read as
/// zero; the sixth stays `None` when no row had a mark.
fn usage_from_sums(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<TokenUsage> {
    let at = |i: usize| -> rusqlite::Result<u64> {
        Ok(row.get::<_, Option<i64>>(first + i)?.unwrap_or(0) as u64)
    };
    Ok(TokenUsage {
        uncached_input: at(0)?,
        cache_read: at(1)?,
        cache_write: at(2)?,
        text_output: at(3)?,
        reasoning_output: at(4)?,
        unsplit_reasoning_chars: row.get::<_, Option<i64>>(first + 5)?.map(|n| n as u64),
        raw: None,
    })
}

/// The four passes of [`Ledger::verify`], over one snapshot.
fn verify_snapshot(conn: &Connection, anchor: Option<Tip>) -> Result<VerifyReport, LedgerError> {
    let mut report = VerifyReport {
        ok: true,
        rows_checked: 0,
        first_broken: None,
        redacted: Vec::new(),
    };
    // Pass 1: walk the whole chain in sequence order, checking contiguity and
    // the chain-hash arithmetic (using each row's own stored digest). This
    // pass catches deletion, reordering or hand-editing of `ledger_chain`
    // itself. Also collects each call's latest event.
    let mut stmt = conn
        .prepare("SELECT seq, event, call_id, digest, chain_hash FROM ledger_chain ORDER BY seq")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut tail = chain_genesis();
    let mut latest: BTreeMap<String, (i64, String, String)> = BTreeMap::new();
    for (index, (seq, event, call_id, digest, chain_hash)) in rows.into_iter().enumerate() {
        let expected_seq = index as i64 + 1;
        if seq != expected_seq {
            report.ok = false;
            report.first_broken = Some(BrokenRow {
                sequence: expected_seq,
                reason: format!(
                    "sequence {expected_seq} is missing (chain jumps to {seq}); a row was deleted"
                ),
            });
            return Ok(report);
        }
        report.rows_checked += 1;

        let mut h = blake3::Hasher::new();
        hash_str(&mut h, &tail);
        hash_str(&mut h, &digest);
        let expected_chain_hash = h.finalize().to_hex().to_string();
        if expected_chain_hash != chain_hash {
            report.ok = false;
            report.first_broken = Some(BrokenRow {
                sequence: seq,
                reason: "chain hash does not follow from the previous row and this row's digest"
                    .to_owned(),
            });
            return Ok(report);
        }

        if event == "redact" {
            report.redacted.push(seq);
        }
        latest.insert(call_id, (seq, event, digest));
        tail = chain_hash;
    }

    // Pass 2: for every call the chain knows about, the live `calls` row
    // must exist, and, once settled, must equal the fold of its events
    // (in practice: match its *latest* event's digest, since every event
    // here stores a full-row digest rather than a partial diff).
    for (call_id, (seq, event, digest)) in &latest {
        if event == "mcp" {
            // An MCP exchange: one immutable row in `mcp_calls`, chained once.
            let live = mcp_row(conn, call_id)?;
            let reason = match live {
                None => Some(format!(
                    "MCP call `{call_id}` no longer exists in `mcp_calls`"
                )),
                Some(record) if &mcp_digest(&record) != digest => Some(format!(
                    "MCP call `{call_id}` was edited: its live fields no longer match its `mcp` event"
                )),
                Some(_) => None,
            };
            if let Some(reason) = reason {
                report.ok = false;
                report.first_broken = Some(BrokenRow {
                    sequence: *seq,
                    reason,
                });
                return Ok(report);
            }
            continue;
        }
        let id = CallId::new(call_id.clone());
        let live = raw_row(conn, &id)?;
        let Some(live) = live else {
            report.ok = false;
            report.first_broken = Some(BrokenRow {
                sequence: *seq,
                reason: format!("call `{call_id}` no longer exists in `calls`"),
            });
            return Ok(report);
        };
        if event == "begin" {
            // The chain has not seen a settle/reconcile/redact event for this
            // call yet, so it is still legitimately mutable (trace fields may
            // change between `begin` and settlement), skip the content
            // check. Deliberately keyed on the *chain's* event, not on the
            // live `settlement` column: trusting the live column would let an
            // attacker reset an already-chained row's `settlement` back to
            // `'in_flight'` specifically to escape every check below
            // (`atk_garbage_raw_and_settlement`).
            continue;
        }

        if live.redacted != 0 {
            if event != "redact" {
                report.ok = false;
                report.first_broken = Some(BrokenRow {
                    sequence: *seq,
                    reason: format!(
                        "call `{call_id}` is flagged redacted but has no matching redaction event"
                    ),
                });
                return Ok(report);
            }
            if live.raw_usage.is_some() {
                report.ok = false;
                report.first_broken = Some(BrokenRow {
                    sequence: *seq,
                    reason: format!(
                        "call `{call_id}` is flagged redacted but still carries its raw payload"
                    ),
                });
                return Ok(report);
            }
        } else {
            // Not redacted: `raw_hash` must still match a fresh hash of the
            // live payload. Without this check, an attacker could forge new
            // payload bytes and recompute a matching `raw_hash` by hand,
            // which would otherwise sail through the digest check below
            // (the whole reason `raw_hash`, not `raw_usage`, feeds the
            // digest is to let redaction clear the payload without also
            // discarding the record's binding to its pre-redaction
            // content, that binding has to be checked somewhere, and this
            // is where).
            let expected_hash = hash_raw_usage(live.raw_usage.as_deref());
            if expected_hash != live.raw_hash {
                report.ok = false;
                report.first_broken = Some(BrokenRow {
                    sequence: *seq,
                    reason: format!(
                        "call `{call_id}`'s raw usage payload does not match its recorded hash"
                    ),
                });
                return Ok(report);
            }
        }

        let recomputed = content_digest(&live);
        if &recomputed != digest {
            report.ok = false;
            report.first_broken = Some(BrokenRow {
                sequence: *seq,
                reason: format!(
                    "call `{call_id}` was edited: its live fields no longer match its `{event}` event"
                ),
            });
            return Ok(report);
        }
    }

    // Pass 3: any settled call with *no* chain event at all is forged,
    // never backfilled after the one-time migration (see
    // `migrate_chain_once`).
    {
        let mut stmt = conn.prepare(
            "SELECT id FROM calls
             WHERE settlement != 'in_flight'
               AND id NOT IN (SELECT call_id FROM ledger_chain)
             ORDER BY rowid LIMIT 1",
        )?;
        let forged: Option<String> = stmt.query_row([], |row| row.get(0)).optional()?;
        if let Some(forged) = forged {
            report.ok = false;
            report.first_broken = Some(BrokenRow {
                sequence: report.rows_checked + 1,
                reason: format!(
                    "call `{forged}` is settled but has no chain event at all; it was never chained"
                ),
            });
            return Ok(report);
        }
        // The same rule for MCP rows: every one is chained in the transaction
        // that writes it, so a row with no `mcp` event was inserted by hand.
        let mut stmt = conn.prepare(
            "SELECT id FROM mcp_calls
             WHERE id NOT IN (SELECT call_id FROM ledger_chain WHERE event = 'mcp')
             ORDER BY rowid LIMIT 1",
        )?;
        let forged: Option<String> = stmt.query_row([], |row| row.get(0)).optional()?;
        if let Some(forged) = forged {
            report.ok = false;
            report.first_broken = Some(BrokenRow {
                sequence: report.rows_checked + 1,
                reason: format!(
                    "MCP call `{forged}` has no chain event at all; it was never chained"
                ),
            });
            return Ok(report);
        }
    }

    // Pass 4: the external anchor. This is what defeats truncating the
    // chain's own tail (which leaves no gap for pass 1 to find) and
    // defeats rebuilding a self-consistent replacement chain after
    // resetting `sqlite_sequence` (which passes passes 1 through 3 on its
    // own terms). See the module docs for what this anchor does and does
    // not cover.
    //
    // The anchor is written off the settle path by `AnchorWriter` and may
    // therefore *lag* the chain: `anchor.seq` can be less than the
    // chain's current tail if events landed after the last completed
    // anchor write. That is accepted, not treated as broken, the
    // `Some(hash) if hash == anchor.chain_hash` arm below matches whether
    // or not the anchor is the exact tail, as long as the row it names
    // still has the hash it named. Only two things are rejected: the
    // anchor naming a sequence that no longer exists at all (`None`,
    // truncation, whether that removed the exact tail or rows beyond
    // where the anchor had reached), and the anchor naming a sequence
    // whose hash has changed (`Some(_)` mismatch, a rewrite). The
    // uncovered window is exactly what `AnchorWriter`'s doc comment
    // states: events chained after the last write the anchor thread
    // finished are not yet covered by it, until the next write lands or
    // `Ledger::flush_anchor`/`Drop` forces one.
    if let Some(anchor) = anchor
        && anchor.seq > 0
    {
        let at_anchor: Option<String> = conn
            .query_row(
                "SELECT chain_hash FROM ledger_chain WHERE seq = ?1",
                params![anchor.seq],
                |row| row.get(0),
            )
            .optional()?;
        match at_anchor {
            Some(hash) if hash == anchor.chain_hash => {}
            Some(_) => {
                report.ok = false;
                report.first_broken = Some(BrokenRow {
                    sequence: anchor.seq,
                    reason: "the chain at the anchored sequence does not match the \
                                 persisted tip; the chain was rewritten"
                        .to_owned(),
                });
            }
            None => {
                report.ok = false;
                report.first_broken = Some(BrokenRow {
                    sequence: anchor.seq,
                    reason: "the anchored sequence no longer exists in the chain; \
                                 the tail was truncated"
                        .to_owned(),
                });
            }
        }
    }

    Ok(report)
}

/// Run `f` inside `BEGIN IMMEDIATE ... COMMIT`, rolling back on error.
///
/// `IMMEDIATE` (rather than the default deferred transaction) takes the write
/// lock up front, so two processes racing to settle different calls block on
/// SQLite's own lock instead of one discovering a write conflict after doing
/// the work.
fn with_immediate<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, LedgerError>,
) -> Result<T, LedgerError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(value) => {
            conn.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// Retry `f` while it fails with `SQLITE_BUSY`/`SQLITE_LOCKED`, up to a bound.
/// For the one operation in this module (switching into WAL mode) that a
/// connection-level `busy_timeout` was measured not to reliably cover.
fn retry_on_busy<T>(mut f: impl FnMut() -> Result<T, LedgerError>) -> Result<T, LedgerError> {
    const ATTEMPTS: u32 = 200;
    for attempt in 0..ATTEMPTS {
        match f() {
            Ok(value) => return Ok(value),
            Err(LedgerError::Sqlite(rusqlite::Error::SqliteFailure(err, _)))
                if attempt + 1 < ATTEMPTS
                    && matches!(
                        err.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(other) => return Err(other),
        }
    }
    unreachable!("the loop above always returns on its last attempt")
}

/// The outcome of [`Ledger::verify`].
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Whether the whole chain, as far as it was checked, is intact.
    pub ok: bool,
    /// How many chain rows passed the sequential walk (pass 1) before stopping.
    pub rows_checked: i64,
    /// The first row found broken, if any (the fail-loud rule: named, not just flagged).
    pub first_broken: Option<BrokenRow>,
    /// Sequence numbers of `redact` events. Reported even when `ok` is true: a
    /// valid chain can still contain redactions, and `verify` names them rather
    /// than letting a clean report imply nothing was ever removed.
    pub redacted: Vec<i64>,
}

/// One broken link in the chain, named by sequence number per the fail-loud rule.
#[derive(Debug, Clone)]
pub struct BrokenRow {
    pub sequence: i64,
    pub reason: String,
}

/// The `calls` columns, in the fixed order every `SELECT` and
/// [`record_from_row`] agree on. One list rather than four copies, so adding a
/// column is one edit instead of a search for every `SELECT`.
const CALL_COLUMNS: &str = "id, adapter, protocol, model, agent_id, task_id, settlement,
     uncached_input, cache_read, cache_write, text_output, reasoning_output,
     raw_usage, request_id, started_ms, finished_ms,
     parent_call, call_kind, call_role, ttft_ms, tool_rounds,
     given_slice_ids, given_prefix_hash, redacted, route, unsplit_reasoning_chars";

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallRecord> {
    let protocol: String = row.get(2)?;
    let settlement: String = row.get(6)?;
    let raw: Option<String> = row.get(12)?;
    let tool_rounds: String = row.get(20)?;
    let slice_ids: String = row.get(21)?;
    Ok(CallRecord {
        id: CallId::new(row.get::<_, String>(0)?),
        adapter: row.get(1)?,
        protocol: if protocol == "additive" {
            Protocol::Additive
        } else {
            Protocol::Inclusive
        },
        model: row.get(3)?,
        agent_id: row.get(4)?,
        task_id: row.get(5)?,
        // `in_flight` is not a `Settlement`: a row still in flight has not settled.
        // It reads as unreconciled, which is the state a caller must act on.
        settlement: Settlement::parse(&settlement).unwrap_or(Settlement::Unreconciled),
        usage: TokenUsage {
            uncached_input: row.get::<_, i64>(7)? as u64,
            cache_read: row.get::<_, i64>(8)? as u64,
            cache_write: row.get::<_, i64>(9)? as u64,
            text_output: row.get::<_, i64>(10)? as u64,
            reasoning_output: row.get::<_, i64>(11)? as u64,
            unsplit_reasoning_chars: row.get::<_, Option<i64>>(25)?.map(|n| n as u64),
            raw: raw.and_then(|r| serde_json::from_str(&r).ok()),
        },
        request_id: row.get(13)?,
        started_ms: row.get(14)?,
        finished_ms: row.get(15)?,
        trace: CallTrace {
            parent_call: row.get::<_, Option<String>>(16)?.map(CallId::new),
            kind: row.get(17)?,
            role: row.get(18)?,
            ttft_ms: row.get(19)?,
            tool_rounds: serde_json::from_str(&tool_rounds).unwrap_or_default(),
            given_slice_ids: serde_json::from_str(&slice_ids).unwrap_or_default(),
            given_prefix_hash: row.get(22)?,
        },
        redacted: row.get::<_, i64>(23)? != 0,
        route: row.get(24)?,
    })
}

/// The columns [`Ledger::verify`] and the chaining logic need, fetched and kept
/// as **raw** stored values (never parsed into `Settlement`, `Protocol` or
/// JSON) so that an unparsable or garbage value hashes to something distinct
/// from every legitimate value instead of collapsing onto a shared fallback.
#[derive(Clone, Debug)]
struct RawFields {
    id: String,
    adapter: String,
    protocol: String,
    model: String,
    agent_id: String,
    task_id: String,
    settlement: String,
    uncached_input: i64,
    cache_read: i64,
    cache_write: i64,
    text_output: i64,
    reasoning_output: i64,
    raw_usage: Option<String>,
    raw_hash: Option<String>,
    request_id: Option<String>,
    started_ms: i64,
    finished_ms: Option<i64>,
    parent_call: Option<String>,
    call_kind: Option<String>,
    call_role: Option<String>,
    ttft_ms: Option<i64>,
    tool_rounds: String,
    given_slice_ids: String,
    given_prefix_hash: Option<String>,
    redacted: i64,
    unsplit_reasoning_chars: Option<i64>,
}

const RAW_COLUMNS: &str = "id, adapter, protocol, model, agent_id, task_id, settlement,
     uncached_input, cache_read, cache_write, text_output, reasoning_output,
     raw_usage, raw_hash, request_id, started_ms, finished_ms,
     parent_call, call_kind, call_role, ttft_ms, tool_rounds,
     given_slice_ids, given_prefix_hash, redacted, unsplit_reasoning_chars";

fn raw_fields_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawFields> {
    Ok(RawFields {
        id: row.get(0)?,
        adapter: row.get(1)?,
        protocol: row.get(2)?,
        model: row.get(3)?,
        agent_id: row.get(4)?,
        task_id: row.get(5)?,
        settlement: row.get(6)?,
        uncached_input: row.get(7)?,
        cache_read: row.get(8)?,
        cache_write: row.get(9)?,
        text_output: row.get(10)?,
        reasoning_output: row.get(11)?,
        raw_usage: row.get(12)?,
        raw_hash: row.get(13)?,
        request_id: row.get(14)?,
        started_ms: row.get(15)?,
        finished_ms: row.get(16)?,
        parent_call: row.get(17)?,
        call_kind: row.get(18)?,
        call_role: row.get(19)?,
        ttft_ms: row.get(20)?,
        tool_rounds: row.get(21)?,
        given_slice_ids: row.get(22)?,
        given_prefix_hash: row.get(23)?,
        redacted: row.get(24)?,
        unsplit_reasoning_chars: row.get(25)?,
    })
}

fn raw_row(conn: &Connection, id: &CallId) -> Result<Option<RawFields>, LedgerError> {
    Ok(conn
        .prepare_cached(&format!("SELECT {RAW_COLUMNS} FROM calls WHERE id = ?1"))?
        .query_row(params![id.as_str()], raw_fields_from_row)
        .optional()?)
}

/// `blake3` of the raw usage blob's exact bytes, or `None` when there is none.
/// `None` and `Some(hash-of-empty-string)` are distinct by construction (an
/// empty blob would still be `Some("")`, which hashes to a real, different
/// value), so "no usage recorded" and "usage recorded as empty" never collide.
fn hash_raw_usage(raw_usage: Option<&str>) -> Option<String> {
    raw_usage.map(|bytes| blake3::hash(bytes.as_bytes()).to_hex().to_string())
}

/// The digest of every field decision 0059 covers **except** `raw_usage`
/// itself (`raw_hash` stands in for it, so redaction can clear the payload
/// without invalidating the digest) and except `redacted` (flipped exactly
/// once, by a `redact` event, deliberately after this same content already
/// has a valid digest from `settle` or `reconcile`).
fn content_digest(f: &RawFields) -> String {
    let mut h = blake3::Hasher::new();
    hash_str(&mut h, &f.id);
    hash_str(&mut h, &f.adapter);
    hash_str(&mut h, &f.protocol);
    hash_str(&mut h, &f.model);
    hash_str(&mut h, &f.agent_id);
    hash_str(&mut h, &f.task_id);
    hash_str(&mut h, &f.settlement);
    hash_i64(&mut h, f.uncached_input);
    hash_i64(&mut h, f.cache_read);
    hash_i64(&mut h, f.cache_write);
    hash_i64(&mut h, f.text_output);
    hash_i64(&mut h, f.reasoning_output);
    hash_opt_str(&mut h, f.raw_hash.as_deref());
    hash_opt_str(&mut h, f.request_id.as_deref());
    hash_i64(&mut h, f.started_ms);
    hash_opt_i64(&mut h, f.finished_ms);
    hash_opt_str(&mut h, f.parent_call.as_deref());
    hash_opt_str(&mut h, f.call_kind.as_deref());
    hash_opt_str(&mut h, f.call_role.as_deref());
    hash_opt_i64(&mut h, f.ttft_ms);
    hash_str(&mut h, &f.tool_rounds);
    hash_str(&mut h, &f.given_slice_ids);
    hash_opt_str(&mut h, f.given_prefix_hash.as_deref());
    // Hashed only when set, so every digest written before the column existed
    // still verifies.
    if let Some(chars) = f.unsplit_reasoning_chars {
        hash_str(&mut h, "unsplit_reasoning_chars");
        hash_i64(&mut h, chars);
    }
    h.finalize().to_hex().to_string()
}

/// Length-prefixed so no two field lists hash alike by concatenation (same
/// construction as `kgraf_tools::approval::field`). Hashes the raw bytes
/// directly rather than going through `format!("{:?}", ...)`: cheaper (no
/// intermediate allocation per field) and it is what "hash the stored bytes
/// exactly" means taken literally.
fn hash_str(h: &mut blake3::Hasher, s: &str) {
    h.update(&(s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}

fn hash_opt_str(h: &mut blake3::Hasher, s: Option<&str>) {
    match s {
        Some(s) => {
            h.update(&[1]);
            hash_str(h, s);
        }
        None => {
            h.update(&[0]);
        }
    }
}

fn hash_i64(h: &mut blake3::Hasher, v: i64) {
    h.update(&v.to_le_bytes());
}

fn hash_opt_i64(h: &mut blake3::Hasher, v: Option<i64>) {
    match v {
        Some(v) => {
            h.update(&[1]);
            h.update(&v.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn json_string_array(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_owned())
}

/// The ledger schema.
///
/// Six independent usage columns plus the raw blob, and no cost column. Three of
/// the eight protocols surveyed changed shape within a year, and the raw blob is the
/// only way to re-derive cost after a pricing or schema change.
///
/// The hash-chain and trace columns are added by [`migrate`] rather than listed
/// here, so that a database created before this feature existed goes through the
/// same `ALTER TABLE` path as a fresh one and picks up identical column defaults
/// either way.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS calls (
    id               TEXT PRIMARY KEY,
    adapter          TEXT NOT NULL,
    -- The usage convention, not the vendor. One vendor ships both behind a single
    -- key, so a provider name cannot decide the arithmetic.
    protocol         TEXT NOT NULL,
    model            TEXT NOT NULL,
    agent_id         TEXT NOT NULL,
    task_id          TEXT NOT NULL,
    settlement       TEXT NOT NULL,
    uncached_input   INTEGER NOT NULL,
    cache_read       INTEGER NOT NULL,
    cache_write      INTEGER NOT NULL,
    text_output      INTEGER NOT NULL,
    reasoning_output INTEGER NOT NULL,
    -- The provider's usage object verbatim.
    raw_usage        TEXT,
    request_id       TEXT,
    started_ms       INTEGER NOT NULL,
    finished_ms      INTEGER
);
CREATE INDEX IF NOT EXISTS calls_agent      ON calls(agent_id);
CREATE INDEX IF NOT EXISTS calls_task       ON calls(task_id);
CREATE INDEX IF NOT EXISTS calls_settlement ON calls(settlement);
CREATE INDEX IF NOT EXISTS calls_started    ON calls(started_ms);
"#;

/// The longest a call can run: a provider request's timeout is 600 s
/// (`ProviderConfig`), so a call that finished within a window started at most
/// this much before it. Slack for [`Ledger::calls_finished_since`]'s index scan.
const CALL_SPAN_SLACK_MS: i64 = 15 * 60 * 1_000;

/// Columns added after the original schema: name and the DDL fragment that
/// creates it, applied only to a `calls` table that does not already have it.
///
/// SQLite's `ALTER TABLE ... ADD COLUMN` has **no** `IF NOT EXISTS` clause,
/// that exists for `CREATE TABLE` and `CREATE INDEX`, not this statement, and
/// SQLite rejects it as a syntax error. So this migration checks
/// `pragma_table_info` itself and adds only what is missing, which is also what
/// makes it safe to run on every `open` rather than once (unlike the chain
/// back-fill in [`Ledger::migrate_chain_once`], which is explicitly gated).
const MIGRATION_COLUMNS: &[(&str, &str)] = &[
    ("parent_call", "TEXT"),
    ("call_kind", "TEXT"),
    ("call_role", "TEXT"),
    ("ttft_ms", "INTEGER"),
    ("tool_rounds", "TEXT NOT NULL DEFAULT '[]'"),
    ("given_slice_ids", "TEXT NOT NULL DEFAULT '[]'"),
    ("given_prefix_hash", "TEXT"),
    ("raw_hash", "TEXT"),
    ("redacted", "INTEGER NOT NULL DEFAULT 0"),
    // Decision 0067: the catalog route id (`kgraf
    // providers`, e.g. "kimi", "kimi-anthropic"), distinct from `adapter`
    // (the wire adapter constant, e.g. "openai-chat-completions", which
    // several different routes can share). `Ledger::route_round_stats`
    // groups by this column instead of `adapter` so two routes on one wire
    // adapter get separate statistics. Deliberately **not** part of
    // `content_digest`'s hash (unlike the trace columns above it): this
    // column is written by `RateLimited::admit` after `begin` already
    // minted the row, not always known at `begin` time (a call made without
    // going through a `RateLimited` wrapper, most test fixtures included,
    // never sets it), and folding a "sometimes absent, filled in later"
    // column into the content hash would make an ordinary, expected
    // `route = NULL` on an unwrapped call indistinguishable from tampering.
    ("route", "TEXT"),
    // Characters of reasoning text in a response whose usage gave no reasoning
    // token count. Set means those tokens are inside `text_output` and the
    // split is unknown. NULL means the split is as reported. Part of
    // `content_digest` when set.
    ("unsplit_reasoning_chars", "INTEGER"),
];

/// The hash chain, append-only. One row per mutating event: `begin`, `settle`,
/// `reconcile`, `redact`, and `mcp` (one per recorded MCP exchange, whose
/// `call_id` is the `mcp_calls` row id). `UNIQUE(event, call_id)` means each of those can
/// happen at most once per call, enforced by SQLite itself rather than by
/// application logic a future edit could bypass.
const CHAIN_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ledger_chain (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    event       TEXT NOT NULL,
    call_id     TEXT NOT NULL,
    digest      TEXT NOT NULL,
    chain_hash  TEXT NOT NULL,
    reason      TEXT,
    created_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ledger_chain_call ON ledger_chain(call_id);
CREATE UNIQUE INDEX IF NOT EXISTS ledger_chain_event_call ON ledger_chain(event, call_id);
"#;

/// MCP traffic (decision 0051), one immutable row per exchange, each chained by
/// its own `mcp` event in the transaction that writes it. A separate table
/// rather than rows in `calls`: an MCP exchange has no tokens, no settlement and
/// no provider protocol, and folding it into `calls` would put zero-usage rows
/// into every spend query.
const MCP_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS mcp_calls (
    id            TEXT PRIMARY KEY,
    server        TEXT NOT NULL,
    tool          TEXT,
    kind          TEXT NOT NULL,
    agent_id      TEXT,
    task_id       TEXT,
    duration_ms   INTEGER NOT NULL,
    outcome       TEXT NOT NULL,
    tool_is_error INTEGER,
    bytes_in      INTEGER NOT NULL,
    bytes_out     INTEGER NOT NULL,
    recorded_ms   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS mcp_calls_agent ON mcp_calls(agent_id);
CREATE INDEX IF NOT EXISTS mcp_calls_task  ON mcp_calls(task_id);
"#;

const MCP_COLUMNS: &str = "id, server, tool, kind, agent_id, task_id, duration_ms, outcome, \
     tool_is_error, bytes_in, bytes_out, recorded_ms";

fn mcp_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpCallRecord> {
    Ok(McpCallRecord {
        id: row.get(0)?,
        recorded_ms: row.get(11)?,
        call: McpCall {
            server: row.get(1)?,
            tool: row.get(2)?,
            kind: row.get(3)?,
            agent_id: row.get(4)?,
            task_id: row.get(5)?,
            duration_ms: row.get(6)?,
            outcome: row.get(7)?,
            tool_is_error: row.get::<_, Option<i64>>(8)?.map(|v| v != 0),
            bytes_in: row.get(9)?,
            bytes_out: row.get(10)?,
        },
    })
}

fn mcp_row(conn: &Connection, id: &str) -> Result<Option<McpCallRecord>, LedgerError> {
    Ok(conn
        .query_row(
            &format!("SELECT {MCP_COLUMNS} FROM mcp_calls WHERE id = ?1"),
            params![id],
            mcp_record_from_row,
        )
        .optional()?)
}

/// The digest of every stored MCP field, domain-separated from
/// [`content_digest`] by a leading tag so an MCP row's digest can never equal a
/// provider call's for any choice of field values.
fn mcp_digest(r: &McpCallRecord) -> String {
    let mut h = blake3::Hasher::new();
    hash_str(&mut h, "mcp");
    hash_str(&mut h, &r.id);
    hash_str(&mut h, &r.call.server);
    hash_opt_str(&mut h, r.call.tool.as_deref());
    hash_str(&mut h, &r.call.kind);
    hash_opt_str(&mut h, r.call.agent_id.as_deref());
    hash_opt_str(&mut h, r.call.task_id.as_deref());
    hash_i64(&mut h, r.call.duration_ms);
    hash_str(&mut h, &r.call.outcome);
    hash_opt_i64(&mut h, r.call.tool_is_error.map(i64::from));
    hash_i64(&mut h, r.call.bytes_in);
    hash_i64(&mut h, r.call.bytes_out);
    hash_i64(&mut h, r.recorded_ms);
    h.finalize().to_hex().to_string()
}

fn migrate(conn: &Connection) -> Result<(), LedgerError> {
    // Its own `BEGIN IMMEDIATE`, separate from `migrate_chain_once`'s: SQLite
    // refuses a `BEGIN` inside an already-open transaction, so these cannot
    // share one. Without this transaction, two connections opening the same
    // on-disk database at once can both see a column missing and both try to
    // add it, and the second `ALTER TABLE` fails outright rather than being a
    // silent no-op (verified by
    // `two_opens_migrating_the_same_old_database_concurrently_both_succeed`,
    // which failed with "duplicate column name" before this wrapped the check
    // and the add in one locked transaction).
    with_immediate(conn, || {
        let mut existing: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('calls')")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        existing.sort();
        for (name, ddl) in MIGRATION_COLUMNS {
            if existing.binary_search(&(*name).to_owned()).is_err() {
                conn.execute(&format!("ALTER TABLE calls ADD COLUMN {name} {ddl}"), [])?;
            }
        }
        conn.execute_batch(CHAIN_SCHEMA)?;
        conn.execute_batch(MCP_SCHEMA)?;
        Ok(())
    })?;
    Ledger::migrate_chain_once(conn)?;
    Ok(())
}

/// The chain's starting hash: 64 hex zeroes, the width of a blake3 digest.
/// Fixed and public rather than derived from anything, exactly as git's empty
/// tree hash is fixed: a genesis value need only be agreed on, not secret. A
/// function rather than a string literal so its length is computed, not
/// counted by eye.
fn chain_genesis() -> String {
    "0".repeat(64)
}

/// Read `path`'s persisted `(seq, chain_hash)` anchor. `None` on any I/O or
/// parse failure, including "file does not exist yet", which is the normal
/// state before the first event is ever appended.
fn read_tip_file(path: &Path) -> Option<Tip> {
    let text = std::fs::read_to_string(path).ok()?;
    let (seq, hash) = text.trim().split_once('\t')?;
    Some(Tip {
        seq: seq.parse().ok()?,
        chain_hash: hash.to_owned(),
    })
}

/// Write `tip` to `path` durably: a temp file, fsynced, then renamed over the
/// real path. The rename is what makes the update atomic, a reader never sees
/// a half-written anchor, and the fsync on the temp file is what makes the
/// content survive a crash between the write and the rename.
fn write_tip_file(path: &Path, tip: &Tip) -> Result<(), LedgerError> {
    let tmp = path.with_extension("tip.tmp");
    std::fs::write(&tmp, format!("{}\t{}\n", tip.seq, tip.chain_hash)).map_err(sqlite_io_error)?;
    {
        // Opened for writing: Windows refuses to flush a read-only handle.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .map_err(sqlite_io_error)?;
        fsync_as_the_database_does(&file).map_err(sqlite_io_error)?;
    }
    std::fs::rename(&tmp, path).map_err(sqlite_io_error)?;
    // The rename is an update to the directory; without flushing the directory
    // a crash can leave the old anchor (or none) in place. Windows cannot open
    // a directory as a file (and NTFS journals the rename), so it skips this.
    #[cfg(not(windows))]
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })
        .map_err(sqlite_io_error)?;
        fsync_as_the_database_does(&dir).map_err(sqlite_io_error)?;
    }
    Ok(())
}

/// Flush `file` with the same call SQLite uses for this ledger's own commits.
///
/// On Apple platforms `File::sync_all` issues `F_FULLFSYNC`, a drive-cache
/// flush that stalls other I/O on the device, while the ledger's commits use
/// plain `fsync(2)` (`synchronous = FULL` with `PRAGMA fullfsync` off, SQLite's
/// default). Measured 2026-09-26 with 16 tasks settling on disk: the anchor's
/// `F_FULLFSYNC` every coalescing window stalled the writer's commits and set the
/// p99 settle latency near 5.5 ms; with the anchor writer idle it was under
/// 2 ms. Using the database's own flush also keeps the anchor exactly as durable
/// as the chain it names, never more: an anchor that outlives the commits it
/// anchors reads as a truncation.
fn fsync_as_the_database_does(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `fsync` takes a file descriptor and no memory; the
        // descriptor stays open for the call because `file` is borrowed.
        let rc = unsafe { libc::fsync(file.as_raw_fd()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_all()
    }
}

/// Wrap a `std::io::Error` as a `LedgerError` without adding a variant just for
/// the two filesystem calls the tip file needs; a failure here is exactly as
/// serious as a database I/O failure and is handled the same way (surfaced,
/// never swallowed).
fn sqlite_io_error(err: std::io::Error) -> LedgerError {
    LedgerError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
        Some(err.to_string()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::ModelKind;
    use kgraf_core::cost::ModelPrice;
    use serde_json::json;

    fn usage() -> TokenUsage {
        TokenUsage {
            uncached_input: 100,
            cache_read: 900,
            cache_write: 50,
            text_output: 20,
            reasoning_output: 5,
            unsplit_reasoning_chars: None,
            raw: Some(json!({"input_tokens": 100})),
        }
    }

    /// `begin` + `settle` chains two events (`begin`, `settle`); every test
    /// below that counts chain rows accounts for both.
    fn begin_and_settle(l: &Ledger) -> CallId {
        let t = l
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let id = t.id().clone();
        t.settle(&usage(), None).expect("settle");
        id
    }

    fn exec(l: &Ledger, sql: &str) {
        l.conn
            .lock()
            .expect("lock")
            .execute_batch(sql)
            .expect("exec");
    }

    #[test]
    fn a_call_is_recorded_before_it_is_issued() {
        // The row must exist before the request goes out, so a process that dies
        // mid-call leaves evidence the call was attempted.
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "agent-1", "task-1")
            .expect("begin");
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.model, "m");
        assert_eq!(record.agent_id, "agent-1");
        assert!(record.finished_ms.is_none());
        token.fail().expect("settle");
    }

    #[test]
    fn a_settled_call_carries_all_six_fields_and_the_raw_blob() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        token.settle(&usage(), Some("req-1")).expect("settle");
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.usage, usage());
        assert_eq!(record.request_id.as_deref(), Some("req-1"));
        assert_eq!(record.settlement, Settlement::Counted);
    }

    #[test]
    fn a_token_dropped_without_settling_leaves_the_call_unreconciled() {
        // The interrupted-stream case. A provider states explicitly that an
        // interrupted stream may never deliver its usage chunk, and a request that
        // billed without being counted violates the accounting rule. Losing the row silently is
        // the failure; this asserts it does not happen.
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token.id().clone()
        };
        let record = ledger.call(&id).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Unreconciled);
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
    }

    /// The raw `settlement` column of a row: `CallRecord` reads `in_flight`
    /// as unreconciled, so it cannot tell the two apart.
    fn raw_settlement(ledger: &Ledger, id: &CallId) -> String {
        ledger.read_barrier();
        let conn = ledger.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row(
            "SELECT settlement FROM calls WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .expect("row")
    }

    /// The only row of a ledger.
    fn only_call(ledger: &Ledger) -> CallId {
        let calls = ledger.calls().expect("calls");
        assert_eq!(calls.len(), 1, "{calls:?}");
        calls[0].id.clone()
    }

    /// A caller cancelled while its `begin` is waiting on the ledger (a
    /// worker stopped, a steer, a time limit) drops the begin future after the
    /// row was submitted and before any token exists to settle it. The row
    /// must still settle, as unreconciled, never stay in flight for good.
    ///
    /// Here the caller is gone before the writer replies: the writer is held
    /// off (its connection locked) until the future is dropped, so its reply
    /// finds the receiver closed and the writer settles the row.
    #[test]
    fn a_begin_dropped_before_the_writer_replies_is_settled_by_the_writer() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("l.sqlite")).expect("open");
        {
            let _writer_held = ledger.conn.lock().expect("lock");
            drop(ledger.begin_async("test", Protocol::Inclusive, "m", "a", "t"));
        }
        let id = only_call(&ledger);
        assert_eq!(raw_settlement(&ledger, &id), "unreconciled");
        assert!(ledger.verify().expect("verify").ok);
    }

    /// The other order: the writer committed the row and sent its id, and the
    /// future is dropped without being polled again, so the id sits unread in
    /// the reply. The future's own guard reads it and settles the row.
    #[test]
    fn a_begin_dropped_after_the_writer_replied_is_settled_by_the_future() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("l.sqlite")).expect("open");
        let begin = ledger.begin_async("test", Protocol::Inclusive, "m", "a", "t");
        // The barrier returns once the writer has committed and replied.
        ledger.read_barrier();
        drop(begin);
        let id = only_call(&ledger);
        assert_eq!(raw_settlement(&ledger, &id), "unreconciled");
        assert!(ledger.verify().expect("verify").ok);
    }

    /// An in-memory ledger writes on the caller's thread, so the id is known
    /// before the future is ever polled; dropped unpolled, it still settles.
    #[test]
    fn an_in_memory_begin_dropped_unpolled_still_settles_the_row() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        drop(ledger.begin_async("test", Protocol::Inclusive, "m", "a", "t"));
        let id = only_call(&ledger);
        assert_eq!(raw_settlement(&ledger, &id), "unreconciled");
        assert!(ledger.verify().expect("verify").ok);
    }

    /// The same, for a begin future polled once (waiting on the writer) and
    /// then dropped, the shape a cancelled `select!` arm leaves.
    #[test]
    fn a_begin_future_dropped_while_pending_still_settles_the_row() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("l.sqlite")).expect("open");
        let mut begin = Box::pin(ledger.begin_async("test", Protocol::Inclusive, "m", "a", "t"));
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        // Usually pending (the writer has not committed yet). If it is ready,
        // the token is dropped here, and its own guard settles the row.
        let _ = std::future::Future::poll(begin.as_mut(), &mut cx);
        drop(begin);
        let id = only_call(&ledger);
        assert_eq!(raw_settlement(&ledger, &id), "unreconciled");
    }

    #[test]
    fn an_unreconciled_call_can_be_reconciled_later() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token
                .leave_unreconciled(Some("req-dropped"))
                .expect("leave");
            token.id().clone()
        };
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
        let recovered = ledger.call(&id).expect("read").expect("present");
        assert_eq!(
            recovered.request_id.as_deref(),
            Some("req-dropped"),
            "the handle for reconciling against the provider's record must survive"
        );

        ledger.reconcile(&id, &usage()).expect("reconcile");
        assert!(ledger.unreconciled().expect("query").is_empty());
        assert_eq!(
            ledger.call(&id).expect("read").expect("present").usage,
            usage()
        );
        assert!(
            ledger.verify().expect("verify").ok,
            "a legitimate reconcile must still verify"
        );
    }

    /// A truncated stream from a provider that reports usage in its first
    /// chunk (`message_start`, for one vendor surveyed): the request id is
    /// known and a real, partial usage figure was observed before the stream
    /// broke. Neither `leave_unreconciled` (drops the partial figure to zero)
    /// nor `cancel` (finalizes as billed, with no request id field) fits.
    #[test]
    fn leave_unreconciled_with_a_partial_usage_and_request_id_is_visible_and_reconcilable() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let partial = TokenUsage {
            uncached_input: 1_200,
            cache_read: 0,
            cache_write: 0,
            text_output: 40,
            reasoning_output: 0,
            unsplit_reasoning_chars: None,
            raw: None,
        };
        let id = {
            let token = ledger
                .begin("anthropic", Protocol::Inclusive, "m", "a", "t")
                .expect("begin");
            token
                .leave_unreconciled_with(Some(&partial), Some("req-truncated"))
                .expect("leave with partial usage");
            token.id().clone()
        };

        // Visible in `unreconciled`, carrying both the partial figure (a
        // stated lower bound, not a spend total) and the request id.
        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].settlement, Settlement::Unreconciled);
        assert_eq!(open[0].usage.uncached_input, 1_200);
        assert_eq!(open[0].usage.text_output, 40);
        assert_eq!(open[0].request_id.as_deref(), Some("req-truncated"));

        // Excluded from every total, a lower bound must not be folded into a
        // figure that then looks complete, exactly like a zero-usage
        // unreconciled row already is (decision 0006).
        let by_model = ledger.usage_by_model().expect("usage");
        assert!(
            by_model.is_empty(),
            "a partial, unreconciled figure must not appear in the totals: {by_model:?}"
        );
        let mut prices = PriceTable::new();
        prices.set(
            "m",
            ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        let (total, _) = ledger.total_cost(&prices).expect("cost");
        assert_eq!(
            total,
            Money::ZERO,
            "the lower bound must not be billed as spend"
        );

        assert!(ledger.verify().expect("verify").ok);

        // Reconciling replaces the partial figure with the provider's final
        // one and moves the call out of `unreconciled`.
        let final_usage = TokenUsage {
            uncached_input: 1_200,
            cache_read: 0,
            cache_write: 0,
            text_output: 87,
            reasoning_output: 0,
            unsplit_reasoning_chars: None,
            raw: None,
        };
        ledger.reconcile(&id, &final_usage).expect("reconcile");
        assert!(ledger.unreconciled().expect("query").is_empty());
        let reconciled = ledger.call(&id).expect("read").expect("present");
        assert_eq!(
            reconciled.usage.text_output, 87,
            "the partial figure was replaced"
        );
        assert_eq!(reconciled.settlement, Settlement::Counted);
        assert!(ledger.verify().expect("verify").ok);
    }

    #[test]
    fn an_unreconciled_call_is_excluded_from_the_total_rather_than_counted_as_zero() {
        // Folding a zero into the sum would look complete while understating spend.
        let ledger = Ledger::open_in_memory().expect("ledger");
        {
            let counted = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            counted.settle(&usage(), None).expect("settle");
        }
        {
            let _dropped = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
        }
        let by_model = ledger.usage_by_model().expect("usage");
        assert_eq!(by_model.len(), 1);
        assert_eq!(
            by_model[0].1.uncached_input, 100,
            "one call's worth, not two"
        );
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
    }

    /// A token its caller stopped on purpose, then dropped unsettled, is
    /// cancelled; one dropped without the mark stays unreconciled.
    #[test]
    fn a_dropped_token_is_cancelled_only_when_its_caller_stopped_it() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let stopped = {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token.stop_on_purpose();
            token.id().clone()
        };
        let probed = {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token.stop_when(|| true);
            token.id().clone()
        };
        let dropped = {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token.stop_when(|| false);
            token.id().clone()
        };
        let read = |id: &CallId| ledger.call(id).expect("read").expect("present");
        assert_eq!(read(&stopped).settlement, Settlement::Cancelled);
        assert_eq!(read(&stopped).usage, TokenUsage::default());
        assert_eq!(read(&probed).settlement, Settlement::Cancelled);
        assert_eq!(read(&dropped).settlement, Settlement::Unreconciled);
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
        assert!(ledger.verify().expect("verify").ok);
    }

    #[test]
    fn a_cancelled_call_keeps_the_usage_it_had_already_incurred() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let partial = TokenUsage {
            uncached_input: 40,
            text_output: 3,
            ..TokenUsage::default()
        };
        token.cancel(Some(&partial)).expect("cancel");
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Cancelled);
        assert_eq!(record.usage.uncached_input, 40);
        // And it counts toward spend, because it billed.
        assert_eq!(
            ledger.usage_by_model().expect("usage")[0].1.uncached_input,
            40
        );
    }

    /// A reconcile only closes an open row, and a settle only an in-flight one.
    /// The UPDATE carries the predicate, so neither overwrites a settled figure.
    #[test]
    fn settlement_writes_are_refused_from_the_wrong_state() {
        let ledger = Ledger::open_in_memory().expect("ledger");

        // Reconcile of a counted row fails and leaves the counted figures.
        let counted = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        counted.settle(&usage(), None).expect("settle");
        let counted_id = counted.id().clone();
        let err = ledger
            .reconcile(&counted_id, &TokenUsage::default())
            .expect_err("counted is not open");
        assert!(matches!(err, LedgerError::WrongState { .. }), "{err:?}");
        assert_eq!(
            ledger
                .call(&counted_id)
                .expect("read")
                .expect("row")
                .usage
                .cache_read,
            900
        );

        // A settle that arrives after a reconcile fails, and the reconciled
        // figures stay. The token is dropped without settling, so the row is
        // in flight until the reconcile.
        let live = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let live_id = live.id().clone();
        assert!(ledger.is_in_flight(&live_id).expect("state"));
        ledger
            .reconcile(&live_id, &usage())
            .expect("reconcile in flight");
        assert!(!ledger.is_in_flight(&live_id).expect("state"));
        let late = live
            .settle(&TokenUsage::default(), None)
            .expect_err("late settle must fail");
        assert!(matches!(late, LedgerError::WrongState { .. }), "{late:?}");
        let row = ledger.call(&live_id).expect("read").expect("row");
        assert_eq!(row.usage.cache_read, 900);
        assert_eq!(row.settlement, Settlement::Counted);
        assert!(ledger.verify().expect("verify").ok);
    }

    #[test]
    fn settling_twice_is_refused() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        token.settle(&usage(), None).expect("first");
        let err = token.settle(&usage(), None).expect_err("second must fail");
        assert!(matches!(err, LedgerError::AlreadySettled { .. }));
    }

    /// The same model id on two routes is costed by each call's own route.
    #[test]
    fn a_route_specific_price_applies_to_calls_made_on_that_route() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        for route in ["metered", "subscription"] {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token.record_route_async(route).wait().expect("route");
            token.settle(&usage(), None).expect("settle");
        }
        let base = ModelPrice {
            uncached_input: 3_000_000,
            cache_read: 300_000,
            cache_write: 3_750_000,
            output: 15_000_000,
            reasoning: 15_000_000,
        };
        let mut prices = PriceTable::new();
        prices.set("m", base);
        let (both, _) = ledger.total_cost(&prices).expect("cost");
        prices.set(
            PriceTable::route_key("m", "subscription"),
            ModelPrice {
                uncached_input: 0,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        let (one, unpriced) = ledger.total_cost(&prices).expect("cost");
        assert!(unpriced.is_empty());
        // Totals are summed exactly and rounded once.
        let exact = prices
            .cost_exact_on(Some("metered"), "m", &usage())
            .expect("x");
        assert_eq!(one, exact.to_money());
        assert_eq!(both, (exact + exact).to_money());
    }

    #[test]
    fn cost_is_derived_at_read_time_from_a_price_table() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token.settle(&usage(), None).expect("settle");
        }
        let mut prices = PriceTable::new();
        prices.set(
            "m",
            ModelPrice {
                uncached_input: 3_000_000,
                cache_read: 300_000,
                cache_write: 3_750_000,
                output: 15_000_000,
                reasoning: 15_000_000,
            },
        );
        let (total, unpriced) = ledger.total_cost(&prices).expect("cost");
        assert!(unpriced.is_empty());
        assert!(total.micros() > 0);

        // Repricing re-derives history rather than invalidating it, which is why
        // cost is not a stored column.
        let mut cheaper = PriceTable::new();
        cheaper.set(
            "m",
            ModelPrice {
                uncached_input: 1_500_000,
                cache_read: 150_000,
                cache_write: 1_875_000,
                output: 7_500_000,
                reasoning: 7_500_000,
            },
        );
        let (halved, _) = ledger.total_cost(&cheaper).expect("cost");
        // Within rounding: each of the five priced fields rounds to the nearest
        // micro-dollar independently, so halving every rate and doubling the result
        // can differ by up to one micro-dollar per field. A micro-dollar is a
        // millionth of a dollar; the point is that the same stored rows reprice,
        // not that the arithmetic is associative across a rate change.
        assert!(
            (halved.micros() * 2 - total.micros()).abs() <= 5,
            "repriced {} against {}",
            halved.micros() * 2,
            total.micros()
        );
    }

    #[test]
    fn an_unpriced_model_is_reported_not_costed_at_zero() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        {
            let token = ledger
                .begin("test", Protocol::Additive, "mystery", "a", "t")
                .expect("begin");
            token.settle(&usage(), None).expect("settle");
        }
        let (total, unpriced) = ledger.total_cost(&PriceTable::new()).expect("cost");
        assert_eq!(total, Money::ZERO);
        assert_eq!(unpriced, vec!["mystery".to_owned()]);
    }

    #[test]
    fn spend_is_attributed_per_agent() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        for (agent, input) in [("agent-a", 100u64), ("agent-b", 300)] {
            let token = ledger
                .begin("test", Protocol::Additive, "m", agent, "t")
                .expect("begin");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: input,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
        }
        let mut prices = PriceTable::new();
        prices.set(
            "m",
            ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        let a = ledger.cost_for_agent("agent-a", &prices).expect("cost");
        let b = ledger.cost_for_agent("agent-b", &prices).expect("cost");
        assert_eq!(b.micros(), a.micros() * 3);
    }

    #[test]
    fn verify_passes_on_a_freshly_settled_chain_and_names_no_redactions() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        for _ in 0..3 {
            begin_and_settle(&ledger);
        }
        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(report.rows_checked, 6, "begin + settle per call");
        assert!(report.redacted.is_empty());
    }

    #[test]
    fn verify_names_a_row_whose_field_was_edited_directly_in_sqlite() {
        // Bypassing the ledger's own API entirely: the same thing a corrupted
        // file or a hand edit would do. This is the tamper `verify` exists to
        // catch (the fail-loud rule, the test-the-mechanism rule: the mechanism, not the setting).
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = begin_and_settle(&ledger);
        assert!(ledger.verify().expect("verify").ok, "clean before tamper");

        exec(
            &ledger,
            &format!(
                "UPDATE calls SET text_output = 999999 WHERE id = '{}'",
                id.as_str()
            ),
        );

        let report = ledger.verify().expect("verify");
        assert!(!report.ok);
        let broken = report.first_broken.expect("a broken row");
        assert!(
            broken.reason.contains(id.as_str()),
            "names the tampered call: {}",
            broken.reason
        );
    }

    #[test]
    fn verify_catches_a_deleted_chain_row_as_a_sequence_gap() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(begin_and_settle(&ledger));
        }
        assert!(ledger.verify().expect("verify").ok, "clean before deletion");

        // Delete only the *settle* event of the middle call, leaving its `begin`
        // event and its `calls` row untouched, so this is purely a mid-chain gap
        // (not the tail-truncation case, which is covered separately below).
        exec(
            &ledger,
            &format!(
                "DELETE FROM ledger_chain WHERE call_id = '{}' AND event = 'settle'",
                ids[1].as_str()
            ),
        );

        let report = ledger.verify().expect("verify");
        assert!(!report.ok);
        let broken = report.first_broken.expect("a broken row");
        assert_eq!(broken.sequence, 4, "the deleted row's sequence number");
        assert_eq!(
            report.rows_checked, 3,
            "the rows before the gap still check out"
        );
    }

    #[test]
    fn redacting_a_row_removes_its_payload_but_the_chain_still_verifies_and_names_it() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = begin_and_settle(&ledger);
        assert!(
            ledger
                .call(&id)
                .expect("read")
                .expect("present")
                .usage
                .raw
                .is_some(),
            "the raw blob is present before redaction"
        );

        ledger
            .redact(&id, "captured a secret by accident")
            .expect("redact");

        let redacted = ledger.call(&id).expect("read").expect("present");
        assert!(redacted.redacted);
        assert!(redacted.usage.raw.is_none(), "the raw payload is gone");

        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(report.rows_checked, 3, "begin, settle, redact");
        assert_eq!(report.redacted, vec![3]);
    }

    #[test]
    fn redacting_twice_is_refused() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = begin_and_settle(&ledger);
        ledger.redact(&id, "first reason").expect("first redact");
        let err = ledger
            .redact(&id, "second reason")
            .expect_err("second redact must fail");
        assert!(matches!(err, LedgerError::AlreadyRedacted { .. }));
    }

    #[test]
    fn redacting_an_unsettled_call_is_refused() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let id = token.id().clone();
        let err = ledger
            .redact(&id, "too early")
            .expect_err("an in-flight call has no chain event to preserve");
        assert!(matches!(err, LedgerError::NotRedactable { .. }));
        token
            .fail()
            .expect("settle so drop does not race the assertion");
    }

    /// The pre-chain schema, frozen here exactly as it read before decision 0059,
    /// so the migration test exercises the real upgrade path rather than a
    /// same-version no-op.
    const OLD_SCHEMA: &str = r#"
    CREATE TABLE calls (
        id               TEXT PRIMARY KEY,
        adapter          TEXT NOT NULL,
        protocol         TEXT NOT NULL,
        model            TEXT NOT NULL,
        agent_id         TEXT NOT NULL,
        task_id          TEXT NOT NULL,
        settlement       TEXT NOT NULL,
        uncached_input   INTEGER NOT NULL,
        cache_read       INTEGER NOT NULL,
        cache_write      INTEGER NOT NULL,
        text_output      INTEGER NOT NULL,
        reasoning_output INTEGER NOT NULL,
        raw_usage        TEXT,
        request_id       TEXT,
        started_ms       INTEGER NOT NULL,
        finished_ms      INTEGER
    );
    "#;

    #[test]
    fn opening_an_old_database_back_fills_the_chain_for_its_existing_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("old-ledger.sqlite");
        {
            let conn = Connection::open(&path).expect("create old db");
            conn.execute_batch(OLD_SCHEMA).expect("old schema");
            conn.execute(
                "INSERT INTO calls(id, adapter, protocol, model, agent_id, task_id, settlement,
                                    uncached_input, cache_read, cache_write, text_output,
                                    reasoning_output, raw_usage, request_id, started_ms, finished_ms)
                 VALUES ('call-1', 'legacy', 'additive', 'm', 'a', 't', 'counted',
                         10, 0, 0, 5, 0, '{}', 'req-1', 1000, 1100)",
                [],
            )
            .expect("insert legacy row 1");
            conn.execute(
                "INSERT INTO calls(id, adapter, protocol, model, agent_id, task_id, settlement,
                                    uncached_input, cache_read, cache_write, text_output,
                                    reasoning_output, raw_usage, request_id, started_ms, finished_ms)
                 VALUES ('call-2', 'legacy', 'additive', 'm', 'a', 't', 'failed',
                         0, 0, 0, 0, 0, NULL, NULL, 1200, 1200)",
                [],
            )
            .expect("insert legacy row 2");
        }

        // Opening through the ledger runs `migrate` and the one-time chain back-fill.
        let ledger = Ledger::open(&path).expect("open and migrate");
        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(report.rows_checked, 2, "both legacy rows were back-filled");

        // A new call chains on top of the back-filled tail rather than
        // restarting from genesis.
        begin_and_settle(&ledger);
        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(report.rows_checked, 4);
    }

    #[test]
    fn the_protocol_is_stored_per_call_not_inferred_from_the_adapter() {
        // One vendor ships both conventions behind one key, so the same adapter
        // name must be able to carry either.
        let ledger = Ledger::open_in_memory().expect("ledger");
        for protocol in [Protocol::Additive, Protocol::Inclusive] {
            let token = ledger
                .begin("one-vendor", protocol, "m", "a", "t")
                .expect("begin");
            token.settle(&usage(), None).expect("settle");
            assert_eq!(
                ledger
                    .call(token.id())
                    .expect("read")
                    .expect("present")
                    .protocol,
                protocol
            );
        }
    }

    // --- Regression tests ported from the `atk_*`
    // fixtures. Each one previously defeated the single-digest-per-row design;
    // every one now asserts the expected outcome instead of merely
    // printing it.

    #[test]
    fn atk_reconcile_breaks_chain() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let id = token.id().clone();
        token.leave_unreconciled(None).expect("leave");
        ledger.reconcile(&id, &usage()).expect("reconcile");
        let report = ledger.verify().expect("verify");
        assert!(
            report.ok,
            "a legitimate reconcile must not break verify: {:?}",
            report.first_broken
        );
    }

    #[test]
    fn atk_redacted_flag_hides_edit() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = begin_and_settle(&ledger);
        exec(
            &ledger,
            &format!(
                "UPDATE calls SET redacted = 1, text_output = 0, uncached_input = 0 WHERE id = '{}'",
                id.as_str()
            ),
        );
        let report = ledger.verify().expect("verify");
        assert!(
            !report.ok,
            "a flag-only redaction (no matching event) must be caught"
        );
    }

    #[test]
    fn atk_redact_then_edit() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = begin_and_settle(&ledger);
        ledger.redact(&id, "x").expect("redact");
        exec(
            &ledger,
            &format!(
                "UPDATE calls SET text_output = 0 WHERE id = '{}'",
                id.as_str()
            ),
        );
        let report = ledger.verify().expect("verify");
        assert!(
            !report.ok,
            "an edit made after a legitimate redaction must still be caught"
        );
    }

    #[test]
    fn atk_tail_truncation() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        begin_and_settle(&ledger);
        begin_and_settle(&ledger);
        let id = begin_and_settle(&ledger);
        exec(
            &ledger,
            &format!(
                "DELETE FROM calls WHERE id = '{0}'; DELETE FROM ledger_chain WHERE call_id = '{0}'",
                id.as_str()
            ),
        );
        let report = ledger.verify().expect("verify");
        assert!(
            !report.ok,
            "truncating the chain's own tail must be caught by the external anchor"
        );
    }

    #[test]
    fn atk_garbage_raw_and_settlement() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let id = token.id().clone();
        token.leave_unreconciled(None).expect("leave");
        exec(
            &ledger,
            &format!(
                "UPDATE calls SET settlement = 'in_flight', raw_usage = 'not json', \
                 protocol = 'additive' WHERE id = '{}'",
                id.as_str()
            ),
        );
        let report = ledger.verify().expect("verify");
        assert!(
            !report.ok,
            "a lossy parse of settlement/raw_usage must not hide the edit"
        );
    }

    #[test]
    fn atk_inserted_row_laundered_by_backfill() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("l.sqlite");
        {
            let ledger = Ledger::open(&path).expect("open");
            begin_and_settle(&ledger);
            exec(
                &ledger,
                "INSERT INTO calls(id, adapter, protocol, model, agent_id, task_id, settlement,
                                    uncached_input, cache_read, cache_write, text_output,
                                    reasoning_output, started_ms)
                 VALUES ('forged', 'x', 'additive', 'm', 'a', 't', 'counted', 1, 1, 1, 1, 1, 0)",
            );
            let before = ledger.verify().expect("verify");
            assert!(
                !before.ok,
                "the forged row has no chain event even before a reopen"
            );
        }
        let ledger = Ledger::open(&path).expect("reopen");
        let report = ledger.verify().expect("verify");
        assert!(
            !report.ok,
            "a reopen must not silently back-fill (launder) a hand-inserted row"
        );
    }

    #[test]
    fn an_on_disk_tail_truncation_is_caught_after_a_reopen_by_the_persisted_anchor_file() {
        // The in-memory `atk_tail_truncation` above proves the mechanism works
        // within one process. This proves the part that only an on-disk ledger
        // can prove: the anchor survives being read by a *different* `Ledger`
        // value entirely, i.e. it defends against truncation discovered only
        // after the application restarts, not just against the live connection
        // that did the tampering.
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("truncate.sqlite");
        let id = {
            let ledger = Ledger::open(&path).expect("open");
            begin_and_settle(&ledger);
            let id = begin_and_settle(&ledger);
            // Explicit, not relying on `Drop` to flush for us: this is the
            // "tail truncation after a *flushed* anchor" case, as distinct from the lag-tolerant case
            // below.
            ledger.flush_anchor();
            assert!(
                ledger.verify().expect("verify").ok,
                "clean before truncation"
            );
            id
        };
        // Tamper with the file directly, as a different process or a text
        // editor would, well after the `Ledger` that wrote it is gone.
        {
            let conn = Connection::open(&path).expect("reopen raw");
            conn.execute_batch(&format!(
                "DELETE FROM calls WHERE id = '{0}';
                 DELETE FROM ledger_chain WHERE call_id = '{0}';",
                id.as_str()
            ))
            .expect("truncate");
        }
        let reopened = Ledger::open(&path).expect("reopen through the ledger");
        let report = reopened.verify().expect("verify");
        assert!(
            !report.ok,
            "a truncation made between two opens must still be caught by the .tip file"
        );
    }

    #[test]
    fn a_lagging_anchor_against_an_unmodified_chain_still_verifies() {
        // The anchor writer runs off the settle path: it may legitimately be behind the chain's current tail.
        // Constructed deterministically here (no timing dependency): write an
        // anchor by hand that names an *earlier*, still-correct point in the
        // chain, exactly what the writer's own coalescing would produce if it
        // had not yet caught up to the latest of several rapid settles.
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("lag.sqlite");
        let ledger = Ledger::open(&path).expect("open");
        for _ in 0..3 {
            begin_and_settle(&ledger);
        }
        ledger.flush_anchor();
        assert!(
            ledger.verify().expect("verify").ok,
            "clean before backdating the anchor"
        );

        // seq 3 is the `begin` event of the second call: a real, unmodified
        // point in the chain, just not the tail (seq 6).
        let lagging_hash: String = ledger
            .conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT chain_hash FROM ledger_chain WHERE seq = 3",
                [],
                |row| row.get(0),
            )
            .expect("read a real chain_hash");
        let mut tip_path = path.clone();
        tip_path.set_extension("tip");
        write_tip_file(
            &tip_path,
            &Tip {
                seq: 3,
                chain_hash: lagging_hash,
            },
        )
        .expect("backdate the anchor file");

        let report = ledger.verify().expect("verify");
        assert!(
            report.ok,
            "a lagging-but-correct anchor must not be reported as broken: {:?}",
            report.first_broken
        );
    }

    #[test]
    fn settle_does_not_block_on_a_deliberately_slow_anchor_writer() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("slow.sqlite");
        // The `begin` below publishes a tip, so the anchor writer is inside
        // this delay when the settle runs. A settle that waited on the anchor
        // would take about `DELAY`; one that does not takes a commit. The bound
        // is half the injected delay, not a fixed time, so load cannot fail it.
        const DELAY: Duration = Duration::from_secs(2);
        let ledger =
            Ledger::open_with_anchor_delay(&path, DELAY).expect("open with a slow anchor writer");

        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let start = std::time::Instant::now();
        token.settle(&usage(), None).expect("settle");
        let elapsed = start.elapsed();
        assert!(
            elapsed < DELAY / 2,
            "settle took {elapsed:?} against an anchor delay of {DELAY:?}, which means \
             it waited on the anchor writer"
        );

        // The slow write does eventually land, once asked to catch up.
        ledger.flush_anchor();
        assert!(ledger.verify().expect("verify").ok);
    }

    #[test]
    fn atk_recompute_all() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = begin_and_settle(&ledger);
        begin_and_settle(&ledger);
        exec(
            &ledger,
            &format!(
                "UPDATE calls SET text_output = 0 WHERE id = '{0}';
                 DELETE FROM ledger_chain;
                 DELETE FROM sqlite_sequence WHERE name = 'ledger_chain';
                 UPDATE calls SET redacted = 0",
                id.as_str()
            ),
        );
        // Force a full, self-consistent rechain from the (already edited) live
        // data, exactly as an attacker with code-level access could.
        {
            let conn = ledger.conn.lock().expect("lock");
            let mut tip = Tip::genesis();
            let mut stmt = conn
                .prepare("SELECT id FROM calls ORDER BY rowid")
                .expect("prep");
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect");
            drop(stmt);
            for id in ids {
                let id = CallId::new(id);
                let fields = raw_row(&conn, &id).expect("raw").expect("present");
                append_chain(
                    &conn,
                    &mut tip,
                    "settle",
                    &id,
                    &content_digest(&fields),
                    None,
                )
                .expect("append");
            }
        }
        let report = ledger.verify().expect("verify");
        assert!(
            !report.ok,
            "a self-consistent rechain built after resetting sqlite_sequence must still be \
             caught by the external anchor, which was never touched"
        );
    }

    #[test]
    fn atk_trace_after_settle() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        token.settle(&usage(), None).expect("settle");
        assert!(matches!(
            token.record_tool_round_async(&["late".to_owned()]).wait(),
            Err(LedgerError::NotInFlight { .. })
        ));
        assert!(matches!(
            token.record_first_token_async(5).wait(),
            Err(LedgerError::NotInFlight { .. })
        ));
        let report = ledger.verify().expect("verify");
        assert!(
            report.ok,
            "a refused post-settlement trace write must leave the row (and the chain) alone: {:?}",
            report.first_broken
        );
        let row = ledger.call(token.id()).expect("read").expect("present");
        assert!(
            row.trace.tool_rounds.is_empty(),
            "the late round must not have been written"
        );
    }

    #[test]
    fn record_tool_round_fails_loud_on_a_corrupt_column_rather_than_defaulting_to_empty() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let id = token.id().clone();
        exec(
            &ledger,
            &format!(
                "UPDATE calls SET tool_rounds = 'not json' WHERE id = '{}'",
                id.as_str()
            ),
        );
        let err = token
            .record_tool_round_async(&["x".to_owned()])
            .wait()
            .expect_err("corrupt tool_rounds must be refused, not defaulted to empty");
        assert!(matches!(err, LedgerError::CorruptTrace { .. }));
        let still_corrupt: String = ledger
            .conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT tool_rounds FROM calls WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .expect("read back");
        assert_eq!(
            still_corrupt, "not json",
            "a refused write must leave the column exactly as it was, not overwrite it"
        );
        token.fail().expect("settle so drop does not race");
    }

    #[test]
    fn two_connections_settling_different_calls_concurrently_both_succeed() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("concurrent.sqlite");
        let a = Ledger::open(&path).expect("open a");
        let b = Ledger::open(&path).expect("open b");

        let ta = a
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin a");
        let tb = b
            .begin("test", Protocol::Additive, "m", "b", "t")
            .expect("begin b");
        // `std::thread::scope` rather than `spawn`: the tokens borrow their
        // ledgers with a non-'static lifetime, and a scoped thread is allowed
        // to borrow across the join instead of requiring 'static.
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| tb.settle(&usage(), None));
            ta.settle(&usage(), None).expect("settle a");
            handle.join().expect("thread").expect("settle b");
        });

        let report = a.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(report.rows_checked, 4, "begin+settle for each of two calls");
    }

    #[test]
    fn two_opens_migrating_the_same_old_database_concurrently_both_succeed() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).expect("create");
            conn.execute_batch(OLD_SCHEMA).expect("schema");
            conn.execute(
                "INSERT INTO calls(id, adapter, protocol, model, agent_id, task_id, settlement,
                                    uncached_input, cache_read, cache_write, text_output,
                                    reasoning_output, raw_usage, request_id, started_ms, finished_ms)
                 VALUES ('legacy-1', 'x', 'additive', 'm', 'a', 't', 'counted', 1, 0, 0, 1, 0, NULL, NULL, 0, 1)",
                [],
            )
            .expect("insert");
        }
        let path_a = path.clone();
        let path_b = path.clone();
        let a = std::thread::spawn(move || Ledger::open(&path_a));
        let b = std::thread::spawn(move || Ledger::open(&path_b));
        let ledger_a = a.join().expect("thread a").expect("open a");
        let _ledger_b = b.join().expect("thread b").expect("open b");

        let report = ledger_a.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(
            report.rows_checked, 1,
            "the legacy row was back-filled exactly once"
        );
    }

    fn batch_connection() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .expect("pragma");
        conn.execute_batch(SCHEMA).expect("schema");
        migrate(&conn).expect("migrate");
        conn
    }

    #[test]
    fn a_batch_whose_transaction_sqlite_rolled_back_fails_every_op_and_keeps_the_count() {
        let conn = batch_connection();
        let tip = Mutex::new(read_chain_tail(&conn).expect("tail"));
        let first = Op::Mcp {
            call: mcp("list_tools", None),
        };
        // No such row, so the op is refused.
        let refused = Op::Ttft {
            id: CallId::new("no-such-call"),
            ttft_ms: 1,
        };
        let last = Op::Mcp {
            call: mcp("list_tools", None),
        };
        let mut rolled_back = |n: usize| {
            if n == 0 {
                // What SQLite does on SQLITE_FULL: it drops the whole transaction.
                conn.execute_batch("ROLLBACK").expect("rollback");
            }
        };
        let out = apply_batch(
            &conn,
            &tip,
            None,
            &[&first, &refused, &last],
            &mut rolled_back,
        );
        assert_eq!(out.len(), 3, "one result per op, no more");
        assert!(matches!(out[0], Err(LedgerError::BatchFailed { .. })));
        assert!(
            matches!(&out[1], Err(e) if !matches!(e, LedgerError::BatchFailed { .. })),
            "the refused op carries its own error"
        );
        assert!(matches!(out[2], Err(LedgerError::BatchFailed { .. })));
        assert!(conn.is_autocommit(), "no transaction left open");
        let on_disk = read_chain_tail(&conn).expect("tail").seq;
        assert_eq!(tip.lock().expect("tip").seq, on_disk);
        assert_eq!(on_disk, 0, "nothing from the batch was committed");
    }

    #[test]
    fn a_commit_that_fails_rolls_the_transaction_back_and_fails_every_op() {
        let conn = batch_connection();
        let tip = Mutex::new(read_chain_tail(&conn).expect("tail"));
        let op = Op::Mcp {
            call: mcp("list_tools", None),
        };
        let mut violate_at_commit = |_: usize| {
            conn.execute_batch(
                "CREATE TABLE parent(id INTEGER PRIMARY KEY); \
                 CREATE TABLE child(p INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED); \
                 INSERT INTO child VALUES (1);",
            )
            .expect("deferred violation");
        };
        let out = apply_batch(&conn, &tip, None, &[&op], &mut violate_at_commit);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(LedgerError::BatchFailed { .. })));
        assert!(
            conn.is_autocommit(),
            "the failed commit left no open transaction"
        );
        assert_eq!(read_chain_tail(&conn).expect("tail").seq, 0);
        assert_eq!(tip.lock().expect("tip").seq, 0);
    }

    fn mcp(kind: &str, tool: Option<&str>) -> McpCall {
        McpCall {
            server: "echoserver".into(),
            tool: tool.map(str::to_owned),
            kind: kind.into(),
            agent_id: Some("w1".into()),
            task_id: Some("t1".into()),
            duration_ms: 12,
            outcome: "success".into(),
            tool_is_error: tool.map(|_| false),
            bytes_in: 40,
            bytes_out: 90,
        }
    }

    #[test]
    fn an_mcp_call_is_stored_with_its_attribution_and_chained() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        begin_and_settle(&ledger);
        let connect = ledger
            .record_mcp_call(&mcp("connect", None))
            .expect("connect");
        let call = ledger
            .record_mcp_call(&mcp("call_tool", Some("echo")))
            .expect("call");
        assert_ne!(connect, call);
        let rows = ledger.mcp_calls().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id, call);
        assert_eq!(rows[1].call, mcp("call_tool", Some("echo")));
        let events: Vec<(String, String)> = {
            let conn = ledger.conn.lock().expect("lock");
            let mut stmt = conn
                .prepare("SELECT event, call_id FROM ledger_chain ORDER BY seq")
                .expect("prepare");
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        assert_eq!(
            events[2..],
            [("mcp".to_owned(), connect), ("mcp".to_owned(), call)]
        );
        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(report.rows_checked, 4, "begin, settle, two mcp events");
    }

    #[test]
    fn verify_names_an_edited_mcp_row() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        ledger
            .record_mcp_call(&mcp("connect", None))
            .expect("connect");
        let id = ledger
            .record_mcp_call(&mcp("call_tool", Some("echo")))
            .expect("call");
        exec(
            &ledger,
            &format!("UPDATE mcp_calls SET bytes_out = 1 WHERE id = '{id}'"),
        );
        let report = ledger.verify().expect("verify");
        assert!(!report.ok);
        let broken = report.first_broken.expect("broken");
        assert_eq!(
            broken.sequence, 2,
            "the second chain row is the edited call"
        );
        assert!(broken.reason.contains(&id), "{}", broken.reason);
    }

    #[test]
    fn verify_names_a_deleted_or_a_forged_mcp_row() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let id = ledger
            .record_mcp_call(&mcp("list_tools", None))
            .expect("list");
        exec(&ledger, &format!("DELETE FROM mcp_calls WHERE id = '{id}'"));
        let report = ledger.verify().expect("verify");
        let broken = report.first_broken.expect("deleted row is named");
        assert_eq!(broken.sequence, 1);
        assert!(
            broken.reason.contains("no longer exists"),
            "{}",
            broken.reason
        );

        let clean = Ledger::open_in_memory().expect("ledger");
        clean
            .record_mcp_call(&mcp("list_tools", None))
            .expect("list");
        exec(
            &clean,
            "INSERT INTO mcp_calls(id, server, tool, kind, agent_id, task_id, duration_ms,
                                   outcome, tool_is_error, bytes_in, bytes_out, recorded_ms)
             VALUES ('mcp-forged', 's', NULL, 'call_tool', 'w', 't', 1, 'success', 0, 1, 1, 1)",
        );
        let report = clean.verify().expect("verify");
        let broken = report.first_broken.expect("forged row is named");
        assert!(broken.reason.contains("mcp-forged"), "{}", broken.reason);
    }

    // ---------------------------------------------------------------------
    // The writer thread: batching, durability, shutdown, reads.

    /// Non-round rates, so pricing a sum and summing per-call prices differ
    /// unless every path uses the same rule.
    fn price_m() -> PriceTable {
        let mut prices = PriceTable::new();
        prices.set(
            "m",
            ModelPrice {
                uncached_input: 1_234_567,
                cache_read: 123_457,
                cache_write: 3_703_703,
                output: 2_345_679,
                reasoning: 2_345_679,
            },
        );
        prices
    }

    #[test]
    fn every_spend_total_prices_per_call_and_agrees_with_spend_of() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut prices = price_m();
        // 0.333333 micro-dollars per token: one token per call is under half a
        // micro-dollar, which a total must neither drop nor round up per call.
        prices.set(
            "tiny",
            ModelPrice {
                uncached_input: 333_333,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        let mut ids = Vec::new();
        for i in 0..10u64 {
            let (model, usage) = if i % 2 == 0 {
                (
                    "tiny",
                    TokenUsage {
                        uncached_input: 1,
                        ..TokenUsage::default()
                    },
                )
            } else {
                (
                    "m",
                    TokenUsage {
                        uncached_input: 7 + i,
                        cache_read: 311 * i,
                        cache_write: 13,
                        text_output: 3 + i,
                        reasoning_output: 5,
                        unsplit_reasoning_chars: None,
                        raw: None,
                    },
                )
            };
            let t = ledger
                .begin("test", Protocol::Additive, model, "w1", "r/c1")
                .expect("begin");
            t.settle(&usage, None).expect("settle");
            ids.push(t.id().clone());
        }
        let per_call = ledger.spend_of(&ids, &prices).expect("spend_of");
        assert!(per_call.unknown.is_empty());
        let (total, unpriced) = ledger.total_cost(&prices).expect("total");
        assert!(unpriced.is_empty());
        assert_eq!(total, per_call.total);
        assert_eq!(
            ledger.cost_for_agent("w1", &prices).expect("agent"),
            per_call.total
        );
        for key in [GroupBy::Agent, GroupBy::Run, GroupBy::Hour] {
            let sum = ledger
                .spend_by(key, &prices)
                .expect("spend_by")
                .iter()
                .fold(Money::ZERO, |a, g| a + g.cost);
            assert_eq!(sum, per_call.total, "{key:?}");
        }
        // The five tiny calls total their exact 1.67 micro-dollars, rounded
        // once to 2: not 0 (each dropped) and not 5 (each shown alone, where
        // a priced call reads as one micro-dollar rather than $0).
        let tiny: Vec<CallId> = ids.iter().step_by(2).cloned().collect();
        assert_eq!(
            ledger.spend_of(&tiny, &prices).expect("tiny").total,
            Money::from_micros(2)
        );
        let one_tiny = TokenUsage {
            uncached_input: 1,
            ..TokenUsage::default()
        };
        assert_eq!(
            prices.cost("tiny", &one_tiny).expect("one"),
            Money::from_micros(1)
        );
    }

    /// The number of `ledger_chain` rows, read on the write connection.
    fn chain_len(l: &Ledger) -> i64 {
        l.conn
            .lock()
            .expect("lock")
            .query_row("SELECT COUNT(*) FROM ledger_chain", [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn concurrent_settles_from_many_tasks_produce_one_verifiable_chain() {
        const TASKS: usize = 16;
        const CALLS: usize = 25;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        let ledger = Arc::new(Ledger::open(&path).expect("ledger"));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .expect("runtime");
        let ids = runtime.block_on(async {
            let mut handles = Vec::new();
            for t in 0..TASKS {
                let ledger = ledger.clone();
                handles.push(tokio::spawn(async move {
                    let agent = format!("agent-{t}");
                    let mut ids = Vec::new();
                    for c in 0..CALLS {
                        let token = ledger
                            .begin_async("test", Protocol::Additive, "m", &agent, "t")
                            .await
                            .expect("begin");
                        ids.push(token.id().clone());
                        // Half the tasks mix in the blocking API, which must
                        // interleave with the async one on the same chain.
                        if t % 2 == 0 && c % 2 == 0 {
                            tokio::task::block_in_place(|| token.settle(&usage(), None))
                                .expect("settle");
                        } else {
                            token.settle_async(&usage(), None).await.expect("settle");
                        }
                    }
                    ids
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.await.expect("task"));
            }
            all
        });
        assert_eq!(ids.len(), TASKS * CALLS);
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "every call got its own id");
        let calls = ledger.calls().expect("calls");
        assert_eq!(calls.len(), TASKS * CALLS);
        assert!(calls.iter().all(|c| c.settlement == Settlement::Counted));
        assert_eq!(chain_len(&ledger), (2 * TASKS * CALLS) as i64);
        ledger.flush_anchor();
        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        assert_eq!(report.rows_checked, (2 * TASKS * CALLS) as i64);
        // Group commit happened: fewer transactions than operations.
        let batches = ledger.progress.batches.load(Ordering::SeqCst);
        assert!(
            batches < (2 * TASKS * CALLS) as u64,
            "{batches} transactions for {} operations",
            2 * TASKS * CALLS
        );
        drop(ledger);
        let reopened = Ledger::open(&path).expect("reopen");
        assert!(reopened.verify().expect("verify").ok);
    }

    /// Hold the write connection while `first` is submitted, so the writer
    /// dequeues it alone and then blocks; everything `rest` submits queues up
    /// behind it and is taken as one batch once the lock is released.
    fn stall_then<A, B>(l: &Ledger, first: impl FnOnce() -> A, rest: impl FnOnce() -> B) -> (A, B) {
        let held = l.conn.lock().expect("lock");
        let a = first();
        // Give the writer time to dequeue `first` alone.
        std::thread::sleep(Duration::from_millis(50));
        let b = rest();
        drop(held);
        (a, b)
    }

    #[test]
    fn queued_operations_share_one_commit_and_are_chained_in_queue_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("ledger");
        let before = ledger.progress.batches.load(Ordering::SeqCst);
        let (first, rest) = stall_then(
            &ledger,
            || ledger.record_mcp_call_async(&mcp("call_tool", Some("t0"))),
            || {
                (1..100)
                    .map(|i| {
                        ledger.record_mcp_call_async(&mcp("call_tool", Some(&format!("t{i}"))))
                    })
                    .collect::<Vec<_>>()
            },
        );
        let ids: Vec<String> = std::iter::once(first)
            .chain(rest)
            .map(|a| a.wait().expect("recorded"))
            .collect();
        let batches = ledger.progress.batches.load(Ordering::SeqCst) - before;
        assert_eq!(batches, 2, "100 queued operations took {batches} commits");
        // Chain order is queue order.
        let chained: Vec<String> = {
            let conn = ledger.conn.lock().expect("lock");
            let mut stmt = conn
                .prepare("SELECT call_id FROM ledger_chain ORDER BY seq")
                .expect("prep");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect")
        };
        assert_eq!(chained, ids);
        assert!(ledger.verify().expect("verify").ok);
    }

    #[test]
    fn a_refused_operation_does_not_sink_the_rest_of_its_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("ledger");
        let token = ledger
            .begin("test", Protocol::Additive, "m", "a", "t")
            .expect("begin");
        let (first, (refused, ok)) = stall_then(
            &ledger,
            || ledger.record_mcp_call_async(&mcp("list_tools", None)),
            // In one batch: a redaction the ledger refuses (the row is still in
            // flight), then a legitimate settle of the same row.
            || {
                (
                    ledger.submit(
                        Op::Redact {
                            id: token.id().clone(),
                            reason: "x".into(),
                        },
                        Done::unit,
                    ),
                    token.settle_async(&usage(), None),
                )
            },
        );
        first.wait().expect("first");
        assert!(matches!(
            refused.wait(),
            Err(LedgerError::NotRedactable { .. })
        ));
        ok.wait()
            .expect("the settle in the same batch still commits");
        assert_eq!(
            ledger
                .call(token.id())
                .expect("read")
                .expect("row")
                .settlement,
            Settlement::Counted
        );
        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
    }

    #[test]
    fn a_writer_panic_mid_batch_fails_that_batch_and_loses_no_acknowledged_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        // Ops 1-4: two begin+settle pairs. Op 5: one MCP record, alone. Ops
        // 6-8: one batch of three, which panics after applying op 7.
        let ledger = Ledger::open_with_faults(
            &path,
            WriterFaults {
                panic_after_op: Some(7),
                abort_after_op: None,
            },
        )
        .expect("ledger");
        let acked = [begin_and_settle(&ledger), begin_and_settle(&ledger)];
        let (alone, batch) = stall_then(
            &ledger,
            || ledger.record_mcp_call_async(&mcp("list_tools", None)),
            || {
                (0..3)
                    .map(|i| {
                        ledger.record_mcp_call_async(&mcp("call_tool", Some(&format!("b{i}"))))
                    })
                    .collect::<Vec<_>>()
            },
        );
        let alone = alone.wait().expect("op 5 committed before the fault");
        for ack in batch {
            assert!(
                matches!(ack.wait(), Err(LedgerError::BatchFailed { .. })),
                "no operation in the faulted batch may be acknowledged"
            );
        }
        // The writer survived the panic and goes on.
        let after = begin_and_settle(&ledger);
        let mcp = ledger.mcp_calls().expect("mcp");
        assert_eq!(
            mcp.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![alone],
            "nothing from the faulted batch is on disk"
        );
        drop(ledger);
        let reopened = Ledger::open(&path).expect("reopen");
        for id in acked.iter().chain([&after]) {
            let row = reopened.call(id).expect("read").expect("acknowledged row");
            assert_eq!(row.settlement, Settlement::Counted);
        }
        let report = reopened.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
    }

    /// A real crash: a child process runs this test with the environment
    /// variable set, acknowledges some settles, and then its writer calls
    /// `abort()` in the middle of a batch. The parent reopens the database and
    /// finds every row the child printed as acknowledged.
    #[test]
    fn a_writer_crash_mid_batch_loses_no_acknowledged_row() {
        const CHILD: &str = "KGRAF_LEDGER_CRASH_CHILD_DB";
        const ACKED: u64 = 20;
        if let Ok(path) = std::env::var(CHILD) {
            use std::io::Write;
            // 2 ops per begin+settle, then one MCP record alone (op 2*ACKED+1),
            // then a batch of three that aborts after its second op.
            let ledger = Ledger::open_with_faults(
                Path::new(&path),
                WriterFaults {
                    panic_after_op: None,
                    abort_after_op: Some(2 * ACKED + 3),
                },
            )
            .expect("ledger");
            let mut out = std::io::stdout();
            for _ in 0..ACKED {
                let id = begin_and_settle(&ledger);
                writeln!(out, "ACKED call {id}").expect("print");
                out.flush().expect("flush");
            }
            let (alone, _batch) = stall_then(
                &ledger,
                || ledger.record_mcp_call_async(&mcp("list_tools", None)),
                || {
                    (0..3)
                        .map(|i| {
                            ledger.record_mcp_call_async(&mcp("call_tool", Some(&format!("b{i}"))))
                        })
                        .collect::<Vec<_>>()
                },
            );
            if let Ok(id) = alone.wait() {
                writeln!(out, "ACKED mcp {id}").expect("print");
                out.flush().expect("flush");
            }
            // The writer aborts the process while applying the batch.
            std::thread::sleep(Duration::from_secs(10));
            panic!("the writer should have aborted the process");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        let output = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args([
                "accounting::tests::a_writer_crash_mid_batch_loses_no_acknowledged_row",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, &path)
            .output()
            .expect("run child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !output.status.success(),
            "the child must have crashed, not finished: {:?}\n{stdout}",
            output.status
        );
        let calls: Vec<CallId> = stdout
            .lines()
            .filter_map(|l| l.split_once("ACKED call ").map(|(_, id)| id.trim()))
            .map(|id| CallId::new(id.to_owned()))
            .collect();
        let mcps: Vec<String> = stdout
            .lines()
            .filter_map(|l| l.split_once("ACKED mcp ").map(|(_, id)| id.trim()))
            .map(str::to_owned)
            .collect();
        assert_eq!(calls.len() as u64, ACKED, "child stdout: {stdout}");

        let reopened = Ledger::open(&path).expect("reopen after the crash");
        for id in &calls {
            let row = reopened
                .call(id)
                .expect("read")
                .expect("acknowledged row survives");
            assert_eq!(row.settlement, Settlement::Counted);
        }
        let on_disk = reopened.mcp_calls().expect("mcp");
        for id in &mcps {
            assert!(
                on_disk.iter().any(|r| &r.id == id),
                "acknowledged {id} survives"
            );
        }
        assert!(
            on_disk.iter().all(|r| r.call.kind == "list_tools"),
            "nothing from the batch that crashed is on disk"
        );
        let report = reopened.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
    }

    #[test]
    fn shutdown_drains_the_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        let ledger = Ledger::open(&path).expect("ledger");
        let acks: Vec<_> = (0..200)
            .map(|i| ledger.record_mcp_call_async(&mcp("call_tool", Some(&format!("q{i}")))))
            .collect();
        // Dropped with (very likely) most of the 200 still queued.
        drop(ledger);
        for ack in acks {
            ack.wait()
                .expect("a queued write is committed at shutdown, not dropped");
        }
        let reopened = Ledger::open(&path).expect("reopen");
        assert_eq!(reopened.mcp_calls().expect("mcp").len(), 200);
        assert!(reopened.verify().expect("verify").ok);
        // After the channel closes, a write fails loud rather than vanishing.
        let closed = Ledger::open(&dir.path().join("closed.db")).expect("ledger");
        closed.submitter.lock().expect("lock").tx.take();
        assert!(matches!(
            closed.record_mcp_call(&mcp("list_tools", None)),
            Err(LedgerError::WriterStopped)
        ));
    }

    #[test]
    fn reads_see_committed_rows_and_writes_submitted_before_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("ledger");
        // A pooled reader opened before the writes.
        assert!(ledger.calls().expect("calls").is_empty());
        let id = begin_and_settle(&ledger);
        let row = ledger
            .call(&id)
            .expect("read")
            .expect("committed row is visible");
        assert_eq!(row.settlement, Settlement::Counted);
        // A token dropped unsettled submits without waiting; the very next read
        // still sees the settlement, not the in-flight row.
        let dropped = {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            token.id().clone()
        };
        let row = ledger.call(&dropped).expect("read").expect("row");
        assert_eq!(row.settlement, Settlement::Unreconciled);
        assert!(row.finished_ms.is_some(), "settled, not still in flight");
        // Reads do not take the write connection: a read completes while a
        // test holds it.
        let _held = ledger.conn.lock().expect("lock");
        assert_eq!(ledger.calls().expect("calls").len(), 2);
    }

    // Holding the write connection across an await is the point: it stalls the
    // writer so the settle stays pending while another task must still run.
    #[allow(clippy::await_holding_lock)]
    #[test]
    fn an_async_settle_does_not_block_the_runtime_thread_while_it_waits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Arc::new(Ledger::open(&dir.path().join("ledger.db")).expect("ledger"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let token = ledger
                .begin_async("test", Protocol::Additive, "m", "a", "t")
                .await
                .expect("begin");
            // Stall the writer, then settle. The one runtime thread must stay
            // free to run another task while the settle waits.
            let held = ledger.conn.lock().expect("lock");
            let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let ticker = {
                let ticks = ticks.clone();
                tokio::spawn(async move {
                    for _ in 0..5 {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        ticks.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };
            let settle = token.settle_async(&usage(), None);
            ticker.await.expect("ticker");
            assert_eq!(
                ticks.load(Ordering::SeqCst),
                5,
                "another task ran while the settle was pending"
            );
            drop(held);
            settle.await.expect("settle");
        });
        assert_eq!(
            ledger.calls().expect("calls")[0].settlement,
            Settlement::Counted
        );
    }

    #[test]
    fn spend_of_many_ids_is_one_query_and_matches_a_per_call_fold() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let prices = price_m();
        let counted = begin_and_settle(&ledger);
        let failed = {
            let t = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("b");
            t.fail().expect("fail");
            t.id().clone()
        };
        let open = {
            let t = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("b");
            t.leave_unreconciled(Some("req")).expect("leave");
            t.id().clone()
        };
        let unpriced = {
            let t = ledger
                .begin("test", Protocol::Additive, "other", "a", "t")
                .expect("b");
            t.settle(&usage(), None).expect("settle");
            t.id().clone()
        };
        let missing = CallId::new("call-nope".to_owned());
        let ids = vec![
            counted.clone(),
            failed,
            open.clone(),
            unpriced.clone(),
            missing.clone(),
            counted.clone(),
        ];
        let spend = ledger.spend_of(&ids, &prices).expect("spend");
        let one = prices.cost_exact_on(None, "m", &usage()).expect("price");
        assert_eq!(
            spend.total,
            (one + one).to_money(),
            "a listed-twice id is counted twice"
        );
        assert_eq!(spend.unknown, vec![open, unpriced, missing]);
        assert_eq!(
            ledger.spend_of(&[], &prices).expect("empty"),
            SpendOf {
                total: Money::ZERO,
                unknown: vec![]
            }
        );
    }

    /// A call cancelled before any usage was reported has unknown spend, not
    /// `$0`; one cancelled with partial usage is priced at that usage.
    #[test]
    fn a_call_cancelled_without_usage_has_unknown_spend() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let prices = price_m();
        let cancel = |partial: Option<&TokenUsage>| {
            let t = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("b");
            t.cancel(partial).expect("cancel");
            t.id().clone()
        };
        let cut = cancel(None);
        let with_usage = cancel(Some(&usage()));
        let spend = ledger
            .spend_of(&[cut.clone(), with_usage.clone()], &prices)
            .expect("spend");
        assert_eq!(spend.unknown, vec![cut.clone()]);
        let one = prices.cost_exact_on(None, "m", &usage()).expect("price");
        assert_eq!(spend.total, one.to_money());
        let read = |id: &CallId| ledger.call(id).expect("read").expect("present");
        assert!(read(&cut).cancelled_without_usage());
        assert!(!read(&with_usage).cancelled_without_usage());
    }

    #[test]
    fn tiny_priced_calls_total_their_real_cost_not_zero() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut prices = PriceTable::new();
        // $0.02 per million input tokens.
        prices.set(
            "e",
            kgraf_core::ModelPrice {
                uncached_input: 20_000,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        let ten = TokenUsage {
            uncached_input: 10,
            ..TokenUsage::default()
        };
        let mut ids = Vec::new();
        for _ in 0..1_000 {
            let t = ledger
                .begin("test", Protocol::Inclusive, "e", "w1", "r1/c1")
                .expect("begin");
            t.settle(&ten, None).expect("settle");
            ids.push(t.id().clone());
        }
        // 1,000 x 10 tokens x $0.02 per million = $0.0002 = 200 micro-dollars.
        let expected = Money::from_micros(200);
        assert_eq!(ledger.total_cost(&prices).expect("total").0, expected);
        assert_eq!(ledger.spend_of(&ids, &prices).expect("of").total, expected);
        assert_eq!(
            ledger.cost_for_agent("w1", &prices).expect("agent"),
            expected
        );
        let groups = ledger.spend_by(GroupBy::Agent, &prices).expect("by");
        assert_eq!(groups[0].cost, expected);
        // One such call alone is priced, so it is never shown as $0.
        assert_eq!(prices.cost("e", &ten).expect("one"), Money::from_micros(1));
    }

    #[test]
    fn groups_each_show_their_small_spend_and_exceed_the_total_by_under_a_micro_each() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut prices = PriceTable::new();
        prices.set(
            "e",
            ModelPrice {
                uncached_input: 20_000,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        // Ten tokens at $0.02 per million: a fifth of a micro-dollar per call.
        let ten = TokenUsage {
            uncached_input: 10,
            ..TokenUsage::default()
        };
        for agent in ["a", "b", "c"] {
            let t = ledger
                .begin("test", Protocol::Inclusive, "e", agent, "r/t")
                .expect("begin");
            t.settle(&ten, None).expect("settle");
        }
        let groups = ledger.spend_by(GroupBy::Agent, &prices).expect("by");
        assert!(groups.iter().all(|g| g.cost == Money::from_micros(1)));
        let summed: i64 = groups.iter().map(|g| g.cost.micros()).sum();
        let total = ledger.total_cost(&prices).expect("total").0.micros();
        assert_eq!(total, 1, "0.6 micro-dollars rounds once to one");
        assert!(summed >= total && summed - total < groups.len() as i64);
        // Apportioned, the same exact amounts sum to the total.
        let exact = prices.cost_exact_on(None, "e", &ten).expect("price");
        let shares = ExactMoney::apportion(&[exact, exact, exact]);
        assert_eq!(shares.iter().map(|m| m.micros()).sum::<i64>(), total);
    }

    #[test]
    fn grouped_aggregates_match_a_fold_over_every_row() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let prices = price_m();
        for (agent, task, n) in [
            ("orchestrator:r1", "r1", 1u64),
            ("w1", "r1/c1", 2),
            ("w2", "r1/c2", 3),
            ("w1", "r2/c1", 4),
        ] {
            for _ in 0..n {
                let t = ledger
                    .begin("test", Protocol::Additive, "m", agent, task)
                    .expect("begin");
                t.settle(&usage(), None).expect("settle");
            }
        }
        // Excluded from spend: unreconciled and in flight.
        let t = ledger
            .begin("test", Protocol::Additive, "m", "w1", "r1/c1")
            .expect("b");
        t.leave_unreconciled_with(Some(&usage()), None)
            .expect("leave");
        let _in_flight = ledger
            .begin("test", Protocol::Additive, "m", "w9", "r3")
            .expect("b");

        let one = prices.cost_exact_on(None, "m", &usage()).expect("price");
        let times = |n: u64| (0..n).fold(ExactMoney::ZERO, |acc, _| acc + one).to_money();
        let by_run = ledger.spend_by(GroupBy::Run, &prices).expect("run");
        assert_eq!(
            by_run
                .iter()
                .map(|g| (g.key.clone(), g.cost, g.calls))
                .collect::<Vec<_>>(),
            vec![
                (GroupKey::Name("r1".into()), times(6), 6),
                (GroupKey::Name("r2".into()), times(4), 4),
            ]
        );
        let by_agent = ledger.spend_by(GroupBy::Agent, &prices).expect("agent");
        let w1 = by_agent
            .iter()
            .find(|g| g.key == GroupKey::Name("w1".into()))
            .expect("w1");
        assert_eq!((w1.cost, w1.calls), (times(6), 6));
        assert_eq!(w1.cost, ledger.cost_for_agent("w1", &prices).expect("cost"));
        let by_hour = ledger.spend_by(GroupBy::Hour, &prices).expect("hour");
        let hours: u64 = by_hour.iter().map(|g| g.calls).sum();
        assert_eq!(hours, 10);
        let total: Money = by_hour.iter().fold(Money::ZERO, |a, g| a + g.cost);
        assert_eq!(total, ledger.total_cost(&prices).expect("total").0);
        for g in &by_hour {
            let GroupKey::HourStartMs(ms) = g.key else {
                panic!("an hour key")
            };
            assert_eq!(ms % 3_600_000, 0);
        }

        let under = ledger
            .calls_under_task("r1", Some("orchestrator:r1"))
            .expect("under");
        assert_eq!(under.len(), 7, "1 own + 2 + 3 + the unreconciled one");
        assert!(
            under
                .iter()
                .all(|c| c.task_id == "r1" || c.task_id.starts_with("r1/"))
        );
        assert!(
            ledger
                .calls_under_task("r", None)
                .expect("prefix is by segment")
                .is_empty(),
            "`r` is not a parent of `r1/...`"
        );
    }

    #[test]
    fn usage_by_task_matches_a_fold_over_every_row() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        for (agent, task, inputs) in [
            ("w1", "t1", vec![400u64, 900, 700]),
            ("w2", "t2", vec![100]),
            ("orchestrator:r", "r", vec![5_000]),
        ] {
            for input in inputs {
                let t = ledger
                    .begin("test", Protocol::Additive, "m", agent, task)
                    .expect("begin");
                t.settle(
                    &TokenUsage {
                        uncached_input: input,
                        cache_read: 10,
                        text_output: 3,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
            }
        }
        let tasks = ledger.usage_by_task(Some("orchestrator")).expect("tasks");
        assert_eq!(tasks.len(), 2);
        let t1 = &tasks[0];
        assert_eq!(
            (t1.task_id.as_str(), t1.calls, t1.first_input, t1.peak_input),
            ("t1", 3, 410, 910)
        );
        assert_eq!(t1.usage.uncached_input, 2_000);
        assert_eq!(t1.usage.text_output, 9);
        assert_eq!(ledger.usage_by_task(None).expect("all").len(), 3);
    }

    /// When the disk fills mid-batch SQLite may roll
    /// back the whole transaction. No op may then be reported failed while its
    /// row is committed, or reported recorded while it is not, and the chain
    /// must still verify. Ported from a probe: `max_page_count` at
    /// the current page count plus 3, then a batch of a small op, a 200 KB op
    /// and a small op.
    #[test]
    fn a_full_disk_mid_batch_reports_exactly_what_was_committed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("ledger");
        let before = begin_and_settle(&ledger);
        {
            let conn = ledger.conn.lock().expect("lock");
            let pages: i64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .expect("pages");
            conn.execute_batch(&format!("PRAGMA max_page_count = {}", pages + 3))
                .expect("cap");
        }
        let big = McpCall {
            server: "x".repeat(200 * 1024),
            ..mcp("call_tool", Some("b"))
        };
        let (first, batch) = stall_then(
            &ledger,
            || ledger.record_mcp_call_async(&mcp("list_tools", None)),
            || {
                vec![
                    ledger.record_mcp_call_async(&mcp("call_tool", Some("a"))),
                    ledger.record_mcp_call_async(&big),
                    ledger.record_mcp_call_async(&mcp("call_tool", Some("c"))),
                ]
            },
        );
        let mut results = vec![first.wait()];
        results.extend(batch.into_iter().map(LedgerAck::wait));
        assert!(
            results[2].is_err(),
            "the 200 KB row cannot fit under the page cap: {:?}",
            results[2]
        );
        {
            let conn = ledger.conn.lock().expect("lock");
            conn.execute_batch("PRAGMA max_page_count = 1073741823")
                .expect("uncap");
        }
        let on_disk: Vec<String> = ledger
            .mcp_calls()
            .expect("mcp")
            .into_iter()
            .map(|r| r.id)
            .collect();
        let recorded: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().ok().cloned())
            .collect();
        assert_eq!(
            on_disk, recorded,
            "exactly the acknowledged rows are on disk: {results:?}"
        );
        let c = ledger.conn.lock().expect("lock").query_row(
            "SELECT COUNT(*) FROM mcp_calls WHERE tool = 'c'",
            [],
            |r| r.get::<_, i64>(0),
        );
        assert_eq!(
            c.expect("count") == 1,
            results[3].is_ok(),
            "op c's report matches its row"
        );
        // The writer recovers: the next batch chains onto the real tip.
        let after = begin_and_settle(&ledger);
        ledger.flush_anchor();
        let report = ledger.verify().expect("verify");
        assert!(report.ok, "{:?}", report.first_broken);
        for id in [&before, &after] {
            assert!(ledger.call(id).expect("read").is_some());
        }
    }

    /// The reader pool never opens more than `MAX_READERS` connections; a
    /// reader beyond that waits for one to come back, and every read completes.
    #[test]
    fn the_reader_pool_is_bounded_and_waiting_readers_proceed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Arc::new(Ledger::open(&dir.path().join("ledger.db")).expect("ledger"));
        begin_and_settle(&ledger);
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let threads: Vec<_> = (0..4 * MAX_READERS)
            .map(|_| {
                let ledger = ledger.clone();
                let peak = peak.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let n = ledger
                            .read(|conn| {
                                if let Reads::Pool { pool, .. } = &ledger.reads {
                                    let open = pool.lock().expect("pool").open;
                                    peak.fetch_max(open, Ordering::SeqCst);
                                }
                                // Hold the connection a moment so readers overlap.
                                std::thread::sleep(Duration::from_millis(1));
                                Ok(conn.query_row("SELECT COUNT(*) FROM calls", [], |r| {
                                    r.get::<_, i64>(0)
                                })?)
                            })
                            .expect("read");
                        assert_eq!(n, 1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("reader thread");
        }
        let peak = peak.load(Ordering::SeqCst);
        assert!(peak >= 2, "readers overlapped (peak {peak})");
        assert!(peak <= MAX_READERS, "peak {peak} exceeds {MAX_READERS}");
        // A panic inside a read gives its connection back.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = ledger.read(|_| -> Result<(), LedgerError> { panic!("inside a read") });
        }));
        assert!(panicked.is_err());
        if let Reads::Pool { pool, .. } = &ledger.reads {
            let p = pool.lock().expect("pool");
            assert_eq!(
                p.idle.len(),
                p.open,
                "every open connection is back in the pool"
            );
        }
    }

    /// Below `ROUTE_STATS_MIN_ROUNDS` calls, both figures are `None` (design:
    /// "20 s is the default until then").
    #[test]
    fn route_round_stats_is_none_below_the_minimum_round_count() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        for _ in 0..ROUTE_STATS_MIN_ROUNDS - 1 {
            let token = ledger
                .begin("r", Protocol::Additive, "m", "agent-a", "task")
                .expect("begin");
            token.record_route_async("r").wait().expect("record route");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: 10,
                        text_output: 1,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let stats = ledger.route_round_stats("r").expect("stats");
        assert_eq!(stats.round_ms, None);
        assert_eq!(stats.tokens_per_round, None);
        assert_eq!(stats.rounds, ROUTE_STATS_MIN_ROUNDS - 1);
    }

    /// At or above the minimum, `round_ms` and `tokens_per_round` are the
    /// medians the design asks for, computed only from calls on the named
    /// route (a different adapter's calls do not leak in).
    #[test]
    fn route_round_stats_reports_medians_once_the_route_has_enough_rounds() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        for i in 0..ROUTE_STATS_MIN_ROUNDS {
            let token = ledger
                .begin("r", Protocol::Additive, "m", "agent-a", "task")
                .expect("begin");
            token.record_route_async("r").wait().expect("record route");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: 100 + i as u64,
                        text_output: 1,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // A different route's calls must not contaminate "r"'s statistics.
        let other = ledger
            .begin("other-route", Protocol::Additive, "m", "agent-a", "task")
            .expect("begin");
        other
            .record_route_async("other-route")
            .wait()
            .expect("record route");
        other
            .settle(
                &TokenUsage {
                    uncached_input: 999_999,
                    ..TokenUsage::default()
                },
                None,
            )
            .expect("settle");

        let stats = ledger.route_round_stats("r").expect("stats");
        assert_eq!(stats.rounds, ROUTE_STATS_MIN_ROUNDS);
        let round_ms = stats.round_ms.expect("enough rounds for a median");
        // Each round slept 2ms before the next began; some jitter is normal,
        // but it must be in that neighborhood, not zero and not huge.
        assert!((1..500).contains(&round_ms), "{round_ms}");
        // Token usage per round was 101, 102, ..., 120 (uncached_input) + 1
        // (text_output) each: median of 102..121 is 111 or 112 depending on
        // rounding; assert the tight range rather than one exact value, since
        // the median index depends on how many rounds the loop above ran.
        let tokens = stats.tokens_per_round.expect("enough rounds for a median");
        assert!((100..=121).contains(&tokens), "{tokens}");
    }

    /// Refused, failed and open calls carry no usage and are not rounds; a
    /// counted call's cache writes are tokens the route processed.
    #[test]
    fn route_round_stats_reads_only_counted_calls_and_includes_cache_writes() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        for _ in 0..ROUTE_STATS_MIN_ROUNDS {
            let token = ledger
                .begin("r", Protocol::Additive, "m", "agent-a", "task")
                .expect("begin");
            token.record_route_async("r").wait().expect("record route");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: 10,
                        cache_write: 1_000,
                        text_output: 1,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Many refused or failed calls and one open call on the same route.
        for _ in 0..(ROUTE_STATS_MIN_ROUNDS * 3) {
            let token = ledger
                .begin("r", Protocol::Additive, "m", "agent-a", "task")
                .expect("begin");
            token.record_route_async("r").wait().expect("record route");
            token.fail().expect("fail");
        }
        let open = ledger
            .begin("r", Protocol::Additive, "m", "agent-a", "task")
            .expect("begin");
        open.record_route_async("r").wait().expect("record route");
        open.leave_unreconciled(None).expect("leave open");

        let stats = ledger.route_round_stats("r").expect("stats");
        assert_eq!(stats.rounds, ROUTE_STATS_MIN_ROUNDS, "counted calls only");
        assert_eq!(stats.tokens_per_round, Some(1_011), "cache write included");
    }

    /// The recent latencies of one model: newest first, counted calls only,
    /// filtered by call kind (or every generative kind), and capped.
    #[test]
    fn recent_latencies_read_the_newest_counted_calls_of_one_model_by_kind() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let call = |model: &str, kind: &str, started: i64, latency: i64, counted: bool| {
            let token = ledger
                .begin_traced(
                    "a",
                    Protocol::Inclusive,
                    model,
                    "orchestrator:r",
                    "r",
                    CallTrace {
                        kind: Some(kind.to_owned()),
                        ..CallTrace::default()
                    },
                )
                .expect("begin");
            let id = token.id().clone();
            if counted {
                token.settle(&usage(), None).expect("settle");
            } else {
                token.fail().expect("fail");
            }
            exec(
                &ledger,
                &format!(
                    "UPDATE calls SET started_ms = {started}, finished_ms = {} WHERE id = '{}'",
                    started + latency,
                    id.as_str()
                ),
            );
        };
        call("m", "answer_question", 1_000, 40_000, true);
        call("m", "worker_round", 2_000, 90_000, true);
        call("m", "answer_question", 3_000, 120_000, true);
        call("m", "answer_question", 4_000, 5, false);
        call("other", "answer_question", 5_000, 1, true);
        call("m", ModelKind::EMBEDDING.call_kind(), 6_000, 2, true);
        assert_eq!(
            ledger
                .recent_latencies_ms("m", Some(&["answer_question"]), 10)
                .expect("read"),
            vec![120_000, 40_000],
            "newest first, counted, this model and kind only"
        );
        assert_eq!(
            ledger.recent_latencies_ms("m", None, 10).expect("read"),
            vec![120_000, 90_000, 40_000],
            "every generative kind; the embedding is left out"
        );
        assert_eq!(
            ledger.recent_latencies_ms("m", None, 1).expect("read"),
            vec![120_000],
            "capped at the limit"
        );
        assert!(
            ledger
                .recent_latencies_ms("none", None, 10)
                .expect("read")
                .is_empty()
        );
    }

    /// An embedding, rerank or decision call on a route shares its limiter and
    /// its `route`, but it is not a round: counted with the route's worker
    /// rounds, a flood of small fast ones would pull both medians down and
    /// make capacity admit more workers than the route can serve.
    #[test]
    fn route_round_stats_leaves_out_non_generative_calls_on_the_route() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let settle = |kind: &str, input: u64| {
            let token = ledger
                .begin_traced(
                    "r",
                    Protocol::Inclusive,
                    "m",
                    "agent-a",
                    "task",
                    CallTrace {
                        kind: Some(kind.to_owned()),
                        ..CallTrace::default()
                    },
                )
                .expect("begin");
            token.record_route_async("r").wait().expect("record route");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: input,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
        };
        for _ in 0..ROUTE_STATS_MIN_ROUNDS {
            settle("worker_round", 1_000);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for kind in [
            ModelKind::EMBEDDING,
            ModelKind::RERANK,
            ModelKind::TYPED_DECISION,
        ] {
            for _ in 0..(ROUTE_STATS_MIN_ROUNDS * 2) {
                settle(kind.call_kind(), 5);
            }
        }
        let stats = ledger.route_round_stats("r").expect("stats");
        assert_eq!(stats.rounds, ROUTE_STATS_MIN_ROUNDS, "worker rounds only");
        assert_eq!(stats.tokens_per_round, Some(1_000));
        assert!(stats.round_ms.is_some_and(|ms| ms >= 1), "{stats:?}");
    }

    /// Decision 0067: two catalog routes that speak
    /// the same wire adapter (Kimi's own route and another OpenAI-compatible
    /// vendor both use `"openai-chat-completions"`) must get *separate*
    /// round statistics, not one conflated set. Before this fix,
    /// `route_round_stats` filtered by `adapter`, which would have mixed
    /// these two routes' rounds together (and, in production, matched
    /// neither: `route_capacity_inputs` calls this with the route id, which
    /// the `adapter` column never contains at all).
    #[test]
    fn two_routes_on_one_wire_adapter_get_separate_stats() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        const ADAPTER: &str = "openai-chat-completions";
        // Route "kimi": `ROUTE_STATS_MIN_ROUNDS` rounds, small token usage.
        for _ in 0..ROUTE_STATS_MIN_ROUNDS {
            let token = ledger
                .begin(ADAPTER, Protocol::Inclusive, "m", "agent-a", "task")
                .expect("begin");
            token
                .record_route_async("kimi")
                .wait()
                .expect("record route");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: 10,
                        text_output: 1,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Route "other-vendor": same adapter, fewer rounds (below the
        // minimum) and much larger token usage, so a conflated result would
        // be obviously wrong in either direction (rounds or tokens).
        for _ in 0..3 {
            let token = ledger
                .begin(ADAPTER, Protocol::Inclusive, "m", "agent-b", "task")
                .expect("begin");
            token
                .record_route_async("other-vendor")
                .wait()
                .expect("record route");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: 999_999,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
        }

        let kimi = ledger.route_round_stats("kimi").expect("stats");
        assert_eq!(
            kimi.rounds, ROUTE_STATS_MIN_ROUNDS,
            "kimi's own rounds only, not the other route's 3 more"
        );
        assert!(kimi.round_ms.is_some(), "enough rounds for a median");
        let kimi_tokens = kimi.tokens_per_round.expect("enough rounds for a median");
        assert!(
            kimi_tokens < 100,
            "kimi's small usage must not be inflated by the other route's: {kimi_tokens}"
        );

        let other = ledger.route_round_stats("other-vendor").expect("stats");
        assert_eq!(
            other.rounds, 3,
            "other-vendor's own rounds only, not kimi's 20 more"
        );
        assert_eq!(
            other.round_ms, None,
            "below the minimum round count on its own"
        );

        let unrelated = ledger.route_round_stats(ADAPTER).expect("stats");
        assert_eq!(
            unrelated.rounds, 0,
            "the adapter name itself is not a route id and must match nothing"
        );
    }
}
