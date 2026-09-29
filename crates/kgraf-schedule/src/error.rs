//! Typed failures for the scheduling core (fail loud, never substitute).

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Every way a schedule, a recurrence, or a claim can fail.
///
/// Every variant is named for what actually happened. None of them collapse into a
/// generic "scheduling error" string, because a caller (the orchestrator, later) needs
/// to tell a disabling failure from a transient one (decision 0052).
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    /// The time zone string was empty, unknown to the IANA database, or malformed.
    /// This is refused, never replaced with the local zone or UTC.
    #[error("unknown or invalid IANA time zone: {zone:?}")]
    InvalidTimeZone { zone: String },

    /// The cron pattern did not parse, does not match any occurrence at all (an
    /// unsatisfiable expression such as 31 February, caught at construction time -
    /// see `RecurrenceRule::with_budget`), or evaluating it failed.
    /// `croner::errors::CronError` implements `std::error::Error` and is `Send`, so the
    /// source is preserved.
    #[error("invalid recurrence rule {pattern:?}: {source}")]
    InvalidRecurrence {
        pattern: String,
        #[source]
        source: croner::errors::CronError,
    },

    /// A lease or jitter offset given as a `std::time::Duration` does not fit in a
    /// `chrono::Duration` (the input was, in practice, deliberately absurd: chrono's
    /// range is hundreds of millions of years). Refused rather than silently clamped to
    /// zero (an out-of-range lease that silently became "expires immediately"
    /// was exactly the defect this variant replaces).
    #[error("duration {duration:?} does not fit in a chrono::Duration ({context})")]
    DurationOutOfRange {
        duration: Duration,
        context: &'static str,
    },

    /// The claim table could not be reached or a statement failed.
    #[error("schedule store error: {0}")]
    Store(#[from] rusqlite::Error),

    /// A schedule id used in a claim or lookup does not exist in the store.
    #[error("unknown schedule id: {0}")]
    UnknownSchedule(String),

    /// A claim operation (`record_outcome`, `renew_lease`) was fenced off: the
    /// `(claimant, generation)` pair given does not match the occurrence's current
    /// claim, either because the claim moved on to a new claimant (this caller's lease
    /// lapsed and someone else claimed), the occurrence is not currently claimed at all
    /// (already recorded, or never claimed), or the caller never held a claim on it.
    /// This is the fencing token check: without it, a slow, still-alive claimant could
    /// overwrite another claimant's already-recorded outcome, or record a failure
    /// against a `done` occurrence.
    #[error(
        "stale or unknown claim for schedule {schedule_id:?} due {due_at}: \
         claimant {claimant:?} generation {generation} no longer holds this claim"
    )]
    StaleClaim {
        schedule_id: String,
        due_at: DateTime<Utc>,
        claimant: String,
        generation: i64,
    },

    /// A millisecond timestamp read back from the claim table does not form a valid
    /// `DateTime<Utc>`. Every value this store writes comes from a real `DateTime<Utc>`
    /// in the first place, so this means the file was corrupted or hand-edited outside
    /// this store - reported, never silently coerced.
    #[error("stored timestamp {millis} in {column} is not a valid instant")]
    CorruptTimestamp { millis: i64, column: &'static str },

    /// A schedule id failed [`crate::storage::validate_schedule_id`].
    #[error("invalid schedule id {id:?}: {reason}")]
    InvalidScheduleId { id: String, reason: &'static str },

    /// The graph store, which holds schedules on work nodes (decision 0052 section
    /// 3, `node_store`), refused a read or a write.
    #[error("graph store: {0}")]
    Graph(#[from] kgraf_graph::GraphError),

    /// A stored schedule definition is not valid JSON, or one could not be encoded.
    #[error("schedule `{id}`: {context}: {source}")]
    Json {
        id: String,
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },

    /// `--node` named a work node the graph does not hold.
    #[error(
        "no work node `{0}` is stored; give an existing node id, or omit --node to create one from the instruction"
    )]
    UnknownWorkNode(String),

    /// A schedule with this id already exists, on the named work node.
    #[error("a schedule named `{id}` already exists, on work node `{node}`; remove it first")]
    ScheduleExists { id: String, node: String },
}

impl kgraf_diag::DiagCode for ScheduleError {
    fn diag_code(&self) -> &'static str {
        match self {
            ScheduleError::InvalidTimeZone { .. } => "KG-0701",
            ScheduleError::StaleClaim { .. } => "KG-0702",
            _ => kgraf_diag::catch_all_for("kgraf-schedule").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-schedule"
    }
}

impl ScheduleError {
    /// Whether this is SQLite reporting the database busy or locked by another
    /// connection (after its busy timeout). A caller writing many rows stops at the
    /// first one instead of waiting the timeout again for each.
    pub fn is_busy(&self) -> bool {
        let sqlite = match self {
            ScheduleError::Store(e) => Some(e),
            ScheduleError::Graph(kgraf_graph::GraphError::Sqlite(e)) => Some(e),
            _ => None,
        };
        matches!(
            sqlite,
            Some(rusqlite::Error::SqliteFailure(f, _))
                if matches!(
                    f.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    }
}

#[cfg(test)]
mod diag_code_tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch resolves to a real glossary entry. One instance per named branch,
    /// plus one unnamed variant for the catch-all.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let instances: Vec<ScheduleError> = vec![
            ScheduleError::InvalidTimeZone { zone: "x".into() },
            ScheduleError::StaleClaim {
                schedule_id: "x".into(),
                due_at: chrono::Utc::now(),
                claimant: "x".into(),
                generation: 0,
            },
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            ScheduleError::UnknownSchedule("x".into()),
        ];
        for e in &instances {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }
}
