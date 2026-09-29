//! The seam the orchestrator implements.
//!
//! This crate owns recurrence, timezones, jitter and the claim table. It does not own
//! starting a run: decision 0052 makes a scheduled run "an orchestrator run started by
//! the clock", and the orchestrator is what knows how to start one, check
//! readiness, take leases, and record ledger spend. The task for this crate is to
//! expose the trait the orchestrator implements later, not to depend on
//! `kgraf-orchestrator` itself, which would invert the dependency of scheduling on
//! the orchestrator (scheduling depends on the orchestrator and nothing is built before
//! it exists).

use chrono::{DateTime, Utc};

use crate::error::ScheduleError;
use crate::failure::FailureClass;

/// What starting a claimed occurrence produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStartOutcome {
    /// The run started (or, for a fast readiness failure, never needed to: decision
    /// 0052's "readiness before capacity" already ran inside `start_run`).
    Started,
    /// The occurrence did not run, classified so the caller can update the claim
    /// table's failure bookkeeping via [`crate::store::ScheduleStore::record_outcome`].
    Failed(FailureClass),
}

/// Implemented by the orchestrator to turn a claimed occurrence into a running task.
///
/// A `ScheduleStore::claim_occurrence` success is the caller's license to invoke this;
/// this trait itself has no opinion on claiming, leases, or the SQLite table above it.
#[async_trait::async_trait]
pub trait RunStarter: Send + Sync {
    /// Start the run for `schedule_id`'s occurrence due at `due_at`.
    ///
    /// Decision 0052's admission order applies inside the implementation: readiness
    /// (credentials, MCP reachability, complete tool discovery) before capacity (lease
    /// acquisition, a concurrency slot), so a readiness failure never occupies a
    /// worker slot another schedule could have used.
    async fn start_run(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
    ) -> Result<RunStartOutcome, ScheduleError>;
}
