//! The provider seam.
//!
//! Decision 0006 settles the shape: hand-rolled clients behind a **small
//! mandatory trait** plus **separate opt-in capability traits**, never a wide
//! interface with capability booleans. Provider types never escape their adapter
//! crate, which is why nothing in this crate names a vendor.
//!
//! Two invariants are enforced by the type system here rather than by discipline.
//!
//! **No code path can reach a provider without being counted** (every call is attributed to the ledger).
//! [`Provider::complete`] requires a [`CallToken`], and a `CallToken` can only be
//! minted by [`Ledger::begin`], which has already written the call's row. There
//! is no way to call a provider and forget to account for it, because there is no
//! way to construct the argument.
//!
//! **A call that billed but was not counted is surfaced, not dropped.** The token
//! is a guard: dropping it without settling marks the call unreconciled in the
//! ledger. An interrupted stream that never delivered its usage event therefore
//! shows up as an explicit hole with its request id, which is what
//! [`Ledger::unreconciled`] reports, rather than as spend that silently vanished.

pub mod accounting;
pub mod call_errors;
pub mod capability;
pub mod crosslimit;
pub mod error;
pub mod kinds;
pub mod limits;
pub mod message;
pub mod provider;
pub mod ratelimit;
pub mod reasoning;
pub mod streaming;

pub use accounting::{
    CallRecord, CallToken, CallTrace, GroupBy, GroupKey, Ledger, LedgerAck, LedgerError, McpCall,
    McpCallRecord, ROUTE_STATS_MIN_ROUNDS, RouteRoundStats, Settlement, SpendGroup, SpendOf,
    TaskUsage,
};
pub use call_errors::{
    CallError, CallErrorLog, Drained as DrainedCallErrors, ERROR_KEEP_BYTES, bounded_error_text,
};
pub use capability::{CacheControl, Capabilities, Reasoning, StreamEvent, Streaming};
pub use crosslimit::{CrossLimitError, SharedLimiterStore, default_store_path, key_id};
pub use error::{Billing, MAX_MALFORMED_OUTPUT_RETRIES, ProviderError};
pub use kinds::{
    DEFAULT_RERANK_MAX_DOCUMENTS, EmbedLimits, Embedder, Embeddings, ModelRoute, ModelRoutes,
    Ranked, Ranking, Reranker, cosine_similarity, rank_by_similarity,
};
pub use limits::{
    HeaderGet, ObservedLimits, last_429, parse_anthropic_style, parse_openai_style, record_429,
    record_429_now,
};
pub use message::{
    Content, Message, ReasoningEffort, Request, Response, Role, Signature, StopReason,
    check_reasoning_off, tool_arguments_object, tool_arguments_text,
};
pub use provider::{Provider, ProviderConfig};
pub use ratelimit::{
    LimiterStats, NoticeSink, RateLimit, RateLimitError, RateLimited, RouteLimiter, RouteRecorded,
    set_notice_sink, take_rate_limit_notices,
};
pub use reasoning::{
    ReasoningAssignment, ReasoningGroup, ReasoningLevel, ReasoningLevels, ReasoningProcess,
    ReasoningScope,
};
pub use streaming::{SseDecoder, StreamGuard};
