//! Deterministic per-schedule jitter.
//!
//! Provenance (decision 0055; idea port, not a code copy): LibreChat
//! `packages/api/src/schedules/` (MIT), as recorded in
//! the decision 0055 research notes: "Cron plus
//! deterministic per-schedule jitter within 120s, so everyone's 9:00 spreads out while
//! each displayed next-run time stays stable." LibreChat is TypeScript; per that
//! evidence file's own license note, a cross-language port carries the *idea*, not
//! copied source. The idea ported here is exactly the two properties named above:
//! spread (many schedules due at the same instant do not all fire at once) and
//! stability (the same schedule always gets the same offset, so its displayed next-run
//! time never jumps between recomputations).
//!
//! The mechanism differs from a guess at LibreChat's own hash: this crate derives the
//! offset from `blake3` (already a workspace dependency, `crates.io` MIT/Apache-2.0)
//! over the schedule id and the occurrence instant, which gives the same two
//! properties without needing to match an unseen implementation byte-for-byte.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::error::ScheduleError;

/// The jitter window: an occurrence fires at its due instant plus an offset drawn
/// deterministically from `[0, window)`. 120s, matching the LibreChat figure cited
/// above.
pub const DEFAULT_JITTER_WINDOW: Duration = Duration::from_secs(120);

/// Compute the deterministic jitter offset for one occurrence of one schedule.
///
/// The same `(schedule_id, due_at)` pair always yields the same offset, so the
/// occurrence's displayed next-run time is stable across recomputation, and two
/// different schedules (or two different occurrences of the same schedule) almost
/// certainly land at different offsets, spreading load that would otherwise all fire
/// on the same clock edge (e.g. every recurring schedule set to a round hour).
///
/// This raw offset can exceed the gap to the *next* occurrence on a very frequent
/// schedule (a per-second schedule's 120s default window can draw up to ~119.999s,
/// pushing a firing dozens of occurrences past its own due instant). Callers that know
/// the next occurrence should use [`jittered_due_at`], which bounds the offset below
/// that gap; this function alone does not.
pub fn deterministic_jitter(
    schedule_id: &str,
    due_at: DateTime<Utc>,
    window: Duration,
) -> Duration {
    if window.is_zero() {
        return Duration::ZERO;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(schedule_id.as_bytes());
    hasher.update(&due_at.timestamp_millis().to_le_bytes());
    let digest = hasher.finalize();
    // `digest.as_bytes()` is always exactly 32 bytes (blake3's fixed output size), so
    // indexing the first 8 is always in bounds; built by hand rather than via
    // `try_into().expect(..)` so there is no `expect` to trip outside a test, even
    // though the conversion could not actually fail here.
    let d = digest.as_bytes();
    let draw = u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    let window_millis = window.as_millis().max(1) as u64;
    Duration::from_millis(draw % window_millis)
}

/// The due instant plus its deterministic jitter, bounded so the jittered instant never
/// reaches `next_due_at` (the schedule's next occurrence after `due_at`, if known).
///
/// Without this bound, a frequent schedule's jitter can land at or after its own next
/// occurrence: a per-second schedule's default 120s window
/// drawing a +74.6s offset, more than seventy occurrences past `due_at`. Bounding the
/// effective window to `min(window, next_due_at - due_at)` guarantees
/// `jittered_due_at(..) < next_due_at` whenever `next_due_at` is given and is after
/// `due_at`.
///
/// Returns an error rather than silently treating an out-of-range duration as zero
/// jitter: the previous version's `unwrap_or(chrono::Duration::zero())` did
/// exactly that for any jitter or gap duration too large for `chrono::Duration`, which
/// in practice cannot happen for a real jitter draw but should never be swallowed
/// silently regardless.
pub fn jittered_due_at(
    schedule_id: &str,
    due_at: DateTime<Utc>,
    next_due_at: Option<DateTime<Utc>>,
    window: Duration,
) -> Result<DateTime<Utc>, ScheduleError> {
    let bounded_window = match next_due_at {
        Some(next) if next > due_at => {
            let gap = (next - due_at)
                .to_std()
                .map_err(|_| ScheduleError::DurationOutOfRange {
                    duration: window,
                    context: "next_due_at - due_at",
                })?;
            window.min(gap)
        }
        _ => window,
    };
    let jitter = deterministic_jitter(schedule_id, due_at, bounded_window);
    let jitter_delta =
        chrono::Duration::from_std(jitter).map_err(|_| ScheduleError::DurationOutOfRange {
            duration: jitter,
            context: "jitter offset",
        })?;
    Ok(due_at + jitter_delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn is_stable_across_recomputation() {
        let due = Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        let first = deterministic_jitter("schedule-a", due, DEFAULT_JITTER_WINDOW);
        let second = deterministic_jitter("schedule-a", due, DEFAULT_JITTER_WINDOW);
        assert_eq!(first, second);
    }

    #[test]
    fn stays_within_the_window() {
        let due = Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        for i in 0..500 {
            let id = format!("schedule-{i}");
            let jitter = deterministic_jitter(&id, due, DEFAULT_JITTER_WINDOW);
            assert!(jitter < DEFAULT_JITTER_WINDOW);
        }
    }

    #[test]
    fn spreads_different_schedules_due_at_the_same_instant() {
        // Every schedule fires at the top of the hour; the jitter must not collapse
        // them all back onto the same instant.
        let due = Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        let offsets: std::collections::HashSet<_> = (0..50)
            .map(|i| deterministic_jitter(&format!("schedule-{i}"), due, DEFAULT_JITTER_WINDOW))
            .collect();
        assert!(
            offsets.len() > 40,
            "expected the jitter to spread 50 schedules across the window, got {} distinct offsets",
            offsets.len()
        );
    }

    #[test]
    fn zero_window_means_no_jitter() {
        let due = Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        assert_eq!(
            deterministic_jitter("schedule-a", due, Duration::ZERO),
            Duration::ZERO
        );
    }

    #[test]
    fn jitter_never_reaches_the_next_occurrence_on_a_frequent_schedule() {
        // The defect: a per-second schedule with the default 120s window drew a
        // +74.6s offset for at least one due instant, landing more than seventy
        // occurrences past its own due time. Sweep many due instants a second apart
        // and check every jittered instant stays strictly before the next occurrence.
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        for i in 0..1000i64 {
            let due = base + chrono::Duration::seconds(i);
            let next_due = base + chrono::Duration::seconds(i + 1);
            let jittered = jittered_due_at(
                "per-second-schedule",
                due,
                Some(next_due),
                DEFAULT_JITTER_WINDOW,
            )
            .unwrap();
            assert!(
                jittered < next_due,
                "due={due} jittered to {jittered}, which reaches or passes the next occurrence {next_due}"
            );
            assert!(jittered >= due);
        }
    }

    #[test]
    fn with_no_next_occurrence_given_the_full_window_still_applies() {
        let due = Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        let jittered = jittered_due_at("schedule-a", due, None, DEFAULT_JITTER_WINDOW).unwrap();
        assert!(jittered >= due);
        assert!(jittered < due + chrono::Duration::from_std(DEFAULT_JITTER_WINDOW).unwrap());
    }
}
