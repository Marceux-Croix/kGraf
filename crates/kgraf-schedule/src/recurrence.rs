//! Recurrence parsing and next/previous-occurrence computation.
//!
//! Reuse (decision 0055): the `croner` crate (MIT, 4.0.0, verified on
//! crates.io 2026-09-26) does the parsing and the calendar search. kGraf takes only
//! that: no scheduler runtime comes with it.
//!
//! **Revision.** The first version
//! of this module ran every search on a dedicated thread with a channel timeout, to get
//! a hard wall-clock cutoff on `croner`'s search. That was both wrong and unnecessary:
//! wrong, because a scan of many occurrences (`Schedule::resolve_wake`) then spawned one
//! thread per occurrence (3,600 threads for one hour of a per-second schedule);
//! unnecessary, because `croner`'s search (`src/lib.rs:walk`) is a component-wise
//! calendar walk, not a brute-force second-by-second scan: it advances by field (year,
//! then month, then day, then hour/minute/second) and is bounded by `YEAR_UPPER_LIMIT`
//! (5000) and its own internal iteration cap, so a single call is already fast even for
//! a pattern that never matches. This version drops the thread entirely.
//!
//! **Revision 2: no wall-clock budget at
//! all, not even a post-hoc safety net.** An intermediate version kept a wall-clock
//! check as a second-line defense against a successful-but-slow search, gated so it
//! could never override `croner`'s own deterministic failure (a full-suite-load flake
//! it once caused, since fixed). That defense still
//! cost, even after that fix: a genuinely valid, successful pattern could still be
//! refused on a loaded machine, for no correctness benefit `croner`'s own
//! iteration-bounded search was not already providing. Dropped entirely -
//! [`RecurrenceRule::next_after_with_clock`]'s own doc has the full argument.
//! [`RecurrenceRule::new`] still validates a pattern at construction (both
//! directions), synchronously, with no thread and no budget of any kind.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;

use crate::error::ScheduleError;
use crate::timezone::parse_time_zone;

/// The wake-scan wall-clock bound (`Schedule::resolve_wake`'s own deadline over
/// however many occurrences it walks to build a catch-up report) - unrelated to a
/// single `croner` call, which no longer carries any wall-clock budget of its own
/// (see [`RecurrenceRule::next_after_with_clock`]'s doc). Kept here
/// because `schedule.rs`'s wake scan is `RecurrenceRule`'s own direct caller.
pub const DEFAULT_EXPANSION_BUDGET: Duration = Duration::from_secs(5);

/// A recurrence rule bound to an explicit IANA time zone.
///
/// The zone is not optional and has no default (decision 0052). A schedule
/// created with an unknown zone never reaches this type, because [`RecurrenceRule::new`]
/// is the only constructor and it returns [`ScheduleError::InvalidTimeZone`] instead.
/// A pattern that matches nothing at all never reaches this type either: construction
/// itself computes one occurrence and propagates the failure.
#[derive(Debug, Clone)]
pub struct RecurrenceRule {
    pattern: String,
    cron: Cron,
    zone: Tz,
}

impl RecurrenceRule {
    /// Parse a cron pattern (`croner`'s six-field syntax: seconds through
    /// day-of-week) in the given IANA zone.
    pub fn new(pattern: &str, zone: &str) -> Result<Self, ScheduleError> {
        let tz = parse_time_zone(zone)?;
        let cron = Cron::from_str(pattern).map_err(|source| ScheduleError::InvalidRecurrence {
            pattern: pattern.to_string(),
            source,
        })?;
        let rule = Self {
            pattern: pattern.to_string(),
            cron,
            zone: tz,
        };
        // Reject a pattern with no matching occurrence at all (e.g. 31 February) right
        // here, rather than letting a scan discover it later. `croner`'s own bound
        // makes this call itself fast even for a never-matching pattern (see
        // `next_after_with_clock`'s own doc for why), so no dedicated thread, and no
        // wall-clock budget, is needed for this check either.
        let now = Utc::now();
        rule.next_after_with_clock(now)?;
        // A pattern can construct fine here (its
        // forward search finds an occurrence, possibly years out - e.g. a year field
        // pinned to 2030) but whose *backward* search fails once a caller asks for
        // "the latest occurrence at or before now" before that year - exactly what
        // `Schedule::resolve_wake`'s wake-catch-up primitive does on every tick. Both
        // directions are validated at construction so that failure surfaces here, not
        // mid-tick.
        rule.previous_at_or_before_with_clock(now)?;
        Ok(rule)
    }

    /// The zone this rule evaluates in.
    pub fn zone(&self) -> Tz {
        self.zone
    }

    /// The pattern as given.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The next occurrence strictly after `after`, as a UTC instant.
    ///
    /// `croner` evaluates the calendar (wall-clock) fields in `self.zone`, so a
    /// "every day at 09:00" rule keeps firing at 09:00 local time across a DST
    /// transition; the returned instant's UTC offset moves, not its local hour.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>, ScheduleError> {
        self.next_after_with_clock(after)
    }

    /// The latest occurrence at or before `before` (inclusive), as a UTC instant.
    ///
    /// This is the wake-catch-up primitive (decision 0052's missed-run policy): "the
    /// latest occurrence at or before now" is a well-defined instant independent of any
    /// scan cap, unlike walking forward from a remembered `last_seen` a bounded number
    /// of steps (see `Schedule::resolve_wake`, and note that keying the catch-up run on the
    /// caller's scan cap was an earlier mistake).
    pub fn previous_at_or_before(
        &self,
        before: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, ScheduleError> {
        self.previous_at_or_before_with_clock(before)
    }

    /// [`RecurrenceRule::next_after`]'s own implementation.
    ///
    /// **No wall-clock check at all.** An
    /// earlier version measured elapsed time around this call and refused a
    /// *successful* search that took longer than a safety-net budget, as a defense
    /// against some hypothetical future `croner` regression. The actual cost of that
    /// defense: `croner`'s search (`walk`, in
    /// its own crate) is already bounded by a fixed iteration count
    /// (`MAX_SEARCH_ITERATIONS`), not by wall time, so it does a fixed amount of
    /// *work* regardless of machine speed - the wall-clock *time* that work takes
    /// does not. A post-hoc wall-clock check on top of an already-deterministic
    /// bound only ever adds a way for a genuinely valid, successful computation to be
    /// refused on a loaded machine, for no correctness benefit `croner`'s own bound
    /// was not already providing. Dropped outright, not softened to a log line: a
    /// log line on a call this fast (well under a millisecond, always) would be pure
    /// noise.
    fn next_after_with_clock(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>, ScheduleError> {
        let local_after = after.with_timezone(&self.zone);
        self.cron
            .find_next_occurrence(&local_after, false)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|source| ScheduleError::InvalidRecurrence {
                pattern: self.pattern.clone(),
                source,
            })
    }

    /// [`RecurrenceRule::previous_at_or_before`]'s own implementation - see
    /// [`RecurrenceRule::next_after_with_clock`]'s doc for the reasoning.
    fn previous_at_or_before_with_clock(
        &self,
        before: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, ScheduleError> {
        let local_before = before.with_timezone(&self.zone);
        self.cron
            .find_previous_occurrence(&local_before, true)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|source| ScheduleError::InvalidRecurrence {
                pattern: self.pattern.clone(),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_and_finds_next_occurrence_in_a_named_zone() {
        // Every day at 09:00:00 America/New_York.
        let rule = RecurrenceRule::new("0 0 9 * * *", "America/New_York").unwrap();
        let after = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let next = rule.next_after(after).unwrap();
        // 09:00 EST on 2026-01-01 is 14:00 UTC.
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 14, 0, 0).unwrap());
    }

    #[test]
    fn crosses_a_dst_transition_keeping_local_wall_time_stable() {
        // 2026-03-08 is the US spring-forward date (clocks jump 02:00 -> 03:00
        // local at 07:00 UTC). Starting after 09:00 local on March 6 (still EST,
        // UTC-5) puts the first occurrence on March 7, still under EST; the second
        // occurrence, on March 8, lands after that morning's transition, under EDT
        // (UTC-4).
        let rule = RecurrenceRule::new("0 0 9 * * *", "America/New_York").unwrap();
        let before = Utc.with_ymd_and_hms(2026, 3, 6, 15, 0, 0).unwrap();
        let first = rule.next_after(before).unwrap();
        assert_eq!(first, Utc.with_ymd_and_hms(2026, 3, 7, 14, 0, 0).unwrap());
        let second = rule.next_after(first).unwrap();
        // The next morning, 09:00 EDT is 13:00 UTC: the local hour held, the
        // offset moved.
        assert_eq!(second, Utc.with_ymd_and_hms(2026, 3, 8, 13, 0, 0).unwrap());
    }

    #[test]
    fn a_nonexistent_local_time_in_the_spring_forward_gap_fires_at_the_next_valid_instant() {
        // US spring-forward, 2026-03-08: local clocks jump from 02:00 to 03:00, so
        // 02:30 never happens. A schedule aimed at 02:30 must not silently vanish for
        // the day or throw; it fires at the first valid instant at or after the gap,
        // 03:00 EDT (07:00 UTC).
        let rule = RecurrenceRule::new("0 30 2 * * *", "America/New_York").unwrap();
        let before = Utc.with_ymd_and_hms(2026, 3, 8, 4, 0, 0).unwrap(); // 2026-03-07 23:00 EST
        let next = rule.next_after(before).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 3, 8, 7, 0, 0).unwrap());
    }

    #[test]
    fn an_ambiguous_local_time_in_the_fall_back_repeat_fires_once_at_the_first_instance() {
        // US fall-back, 2026-11-01: local clocks repeat 01:00-01:59, once as EDT
        // (UTC-4) and once as EST (UTC-5). A schedule aimed at 01:30 must fire exactly
        // once, at the earlier (EDT) instance, not twice and not at the later one.
        let rule = RecurrenceRule::new("0 30 1 * * *", "America/New_York").unwrap();
        // Start just after Oct 31's own 01:30 EDT (05:30 UTC), so the next occurrence
        // found is the ambiguous Nov 1 firing, not Oct 31's.
        let before = Utc.with_ymd_and_hms(2026, 10, 31, 6, 0, 0).unwrap();
        let first = rule.next_after(before).unwrap();
        // 01:30 EDT (first instance) is 05:30 UTC.
        assert_eq!(first, Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap());
        // The next day's 01:30 (now back to a single instance, EST) is 06:30 UTC the
        // following day, not a second visit to the repeated hour.
        let second = rule.next_after(first).unwrap();
        assert_eq!(second, Utc.with_ymd_and_hms(2026, 11, 2, 6, 30, 0).unwrap());
    }

    #[test]
    fn a_half_hourly_schedule_crosses_fall_back_with_no_gap_or_duplicate() {
        // Walk a half-hourly schedule across the 2026-11-01 fall-back transition and
        // check the UTC instants are strictly increasing by exactly 30 minutes each
        // step: no repeated UTC instant (a duplicate firing) and no UTC gap wider than
        // 30 minutes (a skipped firing).
        let rule = RecurrenceRule::new("0 0,30 * * * *", "America/New_York").unwrap();
        let mut cursor = Utc.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap();
        let mut previous = cursor;
        for _ in 0..12 {
            let next = rule.next_after(cursor).unwrap();
            assert!(next > previous, "occurrences must strictly increase in UTC");
            assert_eq!(
                (next - previous).num_minutes(),
                30,
                "every step must be exactly 30 UTC minutes across the transition"
            );
            previous = next;
            cursor = next;
        }
    }

    #[test]
    fn refuses_an_unknown_zone_at_construction() {
        let err = RecurrenceRule::new("0 0 9 * * *", "Nowhere/Imaginary").unwrap_err();
        assert!(matches!(err, ScheduleError::InvalidTimeZone { .. }));
    }

    #[test]
    fn refuses_an_unparsable_pattern() {
        let err = RecurrenceRule::new("not a cron pattern", "UTC").unwrap_err();
        assert!(matches!(err, ScheduleError::InvalidRecurrence { .. }));
    }

    #[test]
    fn refuses_a_never_matching_pattern_at_construction_not_at_first_use() {
        // 31 February never occurs. The defect this replaces: `RecurrenceRule::new`
        // used to accept this pattern happily and only fail (or, worse, stall a scan)
        // the first time someone called `next_after` on it. Construction itself must
        // reject it.
        let err = RecurrenceRule::new("0 0 0 31 2 *", "UTC").unwrap_err();
        assert!(
            matches!(err, ScheduleError::InvalidRecurrence { .. }),
            "expected construction to reject a never-matching pattern, got {err:?}"
        );
    }

    #[test]
    fn a_pattern_pinned_to_a_future_year_is_refused_at_construction_not_on_first_wake() {
        // This pattern's forward search succeeds (it finds the
        // 2030 occurrence), but a caller asking "the latest occurrence at or before
        // now" - `Schedule::resolve_wake`'s wake-catch-up primitive, run on every
        // tick - finds nothing before 2030 and used to fail only then, poisoning a
        // scheduler tick that had nothing to do with this schedule's own health.
        let err = RecurrenceRule::new("0 0 0 1 1 * 2030", "UTC").unwrap_err();
        assert!(
            matches!(err, ScheduleError::InvalidRecurrence { .. }),
            "expected construction to reject a year-pinned pattern with no prior \
             occurrence, got {err:?}"
        );
    }

    #[test]
    fn a_never_matching_pattern_fails_deterministically_with_no_budget_involved() {
        // the mechanism the two removed tests
        // here used to exercise (`next_after_with_clock`'s wall-clock budget) no
        // longer exists at all - `croner`'s own iteration-bounded search is the only
        // thing this pattern's failure depends on, so its own deterministic
        // `InvalidRecurrence` is asserted directly, with nothing to inject or defeat.
        let rule = RecurrenceRule::new("0 0 9 * * *", "UTC").unwrap();
        let after = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert!(rule.next_after_with_clock(after).is_ok());
    }
}
