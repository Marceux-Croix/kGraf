//! Scheduled runs (decision 0052).
//!
//! This crate is the scheduling **core**: recurrence parsing and next-occurrence
//! computation, IANA time zones, deterministic jitter, the atomic SQLite claim table,
//! and the missed-run-on-wake policy. It does not start runs itself; see
//! [`runner::RunStarter`] for the seam the orchestrator implements.
//!
//! Schedules live on work nodes in the graph, with their run history
//! ([`node_store`], decision 0052 section 3). The claim table stays its own SQLite
//! file (`store`).
//!
//! Reuse (decision 0055): `croner` (MIT) for recurrence; the LibreChat jitter
//! idea (MIT, ported, not copied, per
//! the decision 0055 research notes) for spread
//! without instability; the claim table is kGraf's own, because no reviewed
//! candidate claims atomically in SQLite.
//!
//! ## IANA time zone data
//!
//! Time zones (`timezone::parse_time_zone`, and every DST calculation `recurrence`
//! does) are resolved through `chrono-tz` 0.10.4, which bundles **IANA tzdb version
//! 2025b** (`chrono_tz::IANA_TZDB_VERSION`, checked by
//! `timezone::tests::the_bundled_tzdb_version_is_recorded_here`). A future `chrono-tz`
//! upgrade that bumps the bundled tzdb should update this note and re-verify the DST
//! tests in `recurrence`, since a tzdb release can change a specific transition date.
//!
//! ## Claiming is at-least-once, with fencing, not exactly-once
//!
//! See the `store` module doc for the full explanation. In short: an atomic `UPDATE`
//! guarantees only one connection can win a given claim, but a slow, still-alive
//! claimant can still outlive its own lease and lose the claim to someone else. The
//! `generation` fencing token on every claim is what keeps that from corrupting shared
//! state (a double-counted failure, an overwritten outcome) - it does not, and cannot,
//! prevent the underlying work itself from running more than once.

pub mod definition;
pub mod error;
pub mod failure;
pub mod jitter;
pub mod node_store;
pub mod recurrence;
pub mod runner;
pub mod schedule;
pub mod storage;
pub mod store;
pub mod timezone;

pub use definition::{DefinitionKind, ScheduleDefinition};
pub use error::ScheduleError;
pub use failure::{DisableReason, FailureClass, TransientFailurePolicy};
pub use jitter::{DEFAULT_JITTER_WINDOW, deterministic_jitter, jittered_due_at};
pub use node_store::{
    EventKind, Migration, NodeChoice, NodeSchedule, OwnedRunRecord, Reconciler, RunRecord,
    ScheduleView, created_node_id,
};
pub use recurrence::{DEFAULT_EXPANSION_BUDGET, RecurrenceRule};
pub use runner::{RunStartOutcome, RunStarter};
pub use schedule::{
    HARD_MAX_SCAN, MissedCount, MissedRunPolicy, Schedule, ScheduleKind, WakeResolution,
    to_whole_millis,
};
pub use storage::{
    INDEX_KEY, SCHEDULE_PREFIX, schedule_ids_from_meta_keys, schedule_key, validate_schedule_id,
};
pub use store::{
    ClaimOutcome, DeferralOutcome, ExpiredClaim, HELD_FOR_MANUAL_RUN, ManualClaim,
    OccurrenceRecord, RunOutcome, ScheduleStatus, ScheduleStore, TickDecision,
};
pub use timezone::parse_time_zone;
