//! Typed run-failure classes (decision 0052).
//!
//! Decision 0052 and LibreChat's readiness-before-capacity design)
//! draw one line this module exists to keep: a failure that means the schedule cannot
//! ever succeed as configured **disables** it and says why, while a failure that means
//! this attempt did not succeed **counts** toward a threshold, and the two must never be
//! confused. In particular, an outage of the credential *store* is not proof that the
//! *credential* is missing (do not read absence-of-answer as an answer).

use std::fmt;

/// Why a schedule was disabled. Shown on the work node (decision 0052).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisableReason {
    /// The credential a run needs does not exist (a real "not found", not a store
    /// outage).
    MissingCredential { detail: String },
    /// Required configuration (an MCP server, a target, a model route) is absent or
    /// invalid.
    ConfigurationMissing { detail: String },
    /// The credential exists but lacks the permission the run needs.
    PermissionDenied { detail: String },
    /// Consecutive transient failures reached the configured threshold
    /// ([`TransientFailurePolicy`]). Unlike the other three reasons, nothing about the
    /// schedule's configuration is known to be wrong; this is a pause, not a diagnosis,
    /// and is as visible on the work node as any other disable reason (a
    /// schedule that silently kept retrying forever, or silently stopped with no
    /// stated reason, are both a defect this variant exists to rule out).
    TooManyConsecutiveFailures {
        consecutive_failures: u32,
        threshold: u32,
    },
}

impl fmt::Display for DisableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisableReason::MissingCredential { detail } => {
                write!(f, "missing credential: {detail}")
            }
            DisableReason::ConfigurationMissing { detail } => {
                write!(f, "missing configuration: {detail}")
            }
            DisableReason::PermissionDenied { detail } => {
                write!(f, "permission denied: {detail}")
            }
            DisableReason::TooManyConsecutiveFailures {
                consecutive_failures,
                threshold,
            } => write!(
                f,
                "paused after {consecutive_failures} consecutive transient failures (threshold {threshold})"
            ),
        }
    }
}

/// How many consecutive transient failures a schedule tolerates before it is paused
/// (via [`DisableReason::TooManyConsecutiveFailures`]).
///
/// Decision 0052 says a transient failure "counts toward a consecutive-failure
/// threshold" but nothing in this crate acted on that
/// count at first: a schedule could fail forever, silently, with no visible state change. This
/// policy is what closes that gap, and it is a `ScheduleStore`-level, configurable
/// setting (`ScheduleStore::with_transient_failure_policy`), not a hardcoded number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransientFailurePolicy {
    /// Pause the schedule once `consecutive_failures` reaches this many.
    pub pause_after_consecutive_failures: u32,
}

impl Default for TransientFailurePolicy {
    /// Five consecutive transient failures. Chosen as a reasonable default that
    /// tolerates a brief outage (a few retries) without masking a schedule that has
    /// stopped working; not a measured figure, and callers with a different tolerance
    /// should set their own via `with_transient_failure_policy`.
    fn default() -> Self {
        Self {
            pause_after_consecutive_failures: 5,
        }
    }
}

/// The outcome of one occurrence's readiness check or run, classified so the claim
/// store knows whether to disable the schedule or just count a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureClass {
    /// Disables the schedule immediately; no threshold applies.
    Disabling(DisableReason),
    /// A transient failure (a timeout, a 5xx, an MCP server briefly unreachable, or a
    /// credential-*store* outage). Counts toward `consecutive_failures`, and never by
    /// itself disables the schedule, however many times it recurs consecutively; that
    /// is a policy decision for the orchestrator's readiness check, not this crate,
    /// which only carries the count.
    Transient { detail: String },
    /// A capacity refusal: the scheduler itself is
    /// over its own concurrency cap right now, which says nothing about whether this
    /// schedule's configuration works. Deferred to a later tick
    /// (`ScheduleStore::record_deferral`) rather than finalized, bounded by time -
    /// not a fixed retry count - and, unlike `Transient`, never counts
    /// toward the consecutive-failure pause threshold while still being deferred: a
    /// schedule must never be paused for a fact true of the whole process's capacity
    /// and not of it in particular (the same reasoning applies to
    /// readiness).
    Deferred { detail: String },
}

impl FailureClass {
    /// The disable reason, if this failure disables the schedule outright.
    pub fn disable_reason(&self) -> Option<&DisableReason> {
        match self {
            FailureClass::Disabling(reason) => Some(reason),
            FailureClass::Transient { .. } | FailureClass::Deferred { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_store_outage_is_transient_not_disabling() {
        // Decision 0052: "A credential-store outage is never read as proof
        // that the credential is missing." The type itself keeps these apart: there is
        // no `From<StoreOutage> for DisableReason` and no code path builds a
        // `Disabling` variant from a store-reachability failure.
        let outage = FailureClass::Transient {
            detail: "credential store unreachable".to_string(),
        };
        assert!(outage.disable_reason().is_none());
    }

    #[test]
    fn a_missing_credential_disables() {
        let missing = FailureClass::Disabling(DisableReason::MissingCredential {
            detail: "kimi-api-key".to_string(),
        });
        assert!(missing.disable_reason().is_some());
    }
}
