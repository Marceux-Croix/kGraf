//! The schedule types: what a scheduled run is (decision 0052).
//!
//! "A scheduled run is an orchestrator run started by the clock. It is either
//! recurring, on an RFC 5545 recurrence rule, or one-shot, at an instant."
//! `croner`'s cron syntax stands in for RFC 5545 here per decision 0055
//! : both name a recurring calendar pattern, and the roadmap names `croner`
//! specifically as the recurrence engine.

use std::time::Instant;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::error::ScheduleError;
use crate::recurrence::{DEFAULT_EXPANSION_BUDGET, RecurrenceRule};
use crate::timezone::parse_time_zone;

/// A one-shot or recurring firing pattern.
#[derive(Debug, Clone)]
pub enum ScheduleKind {
    /// Fires exactly once, at `at`.
    OneShot { at: DateTime<Utc> },
    /// Fires on a recurrence rule. Boxed: `RecurrenceRule` carries a parsed `Cron`
    /// pattern, which is far larger than `OneShot`'s single timestamp, and clippy's
    /// `large_enum_variant` is right that every `ScheduleKind` value should not pay
    /// for the larger variant's size.
    Recurring { rule: Box<RecurrenceRule> },
}

/// What happens to occurrences that were due while the app was not running.
///
/// Decision 0052 requires this policy be defined, tested and configurable, and the
/// roadmap names the default in the same breath it warns against the alternative: "run
/// once on wake, not a backlog storm."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum MissedRunPolicy {
    /// Every missed occurrence is dropped; only the next future occurrence is kept.
    Skip,
    /// If at least one occurrence was missed, exactly one catch-up run fires on wake,
    /// representing all of them; the rest are dropped. This is the default: a schedule
    /// that was silent for a week still only fires once when the app reopens, not once
    /// per missed tick.
    #[default]
    RunOnceOnWake,
}

/// The hard ceiling on how many occurrences `resolve_wake`'s missed-count scan will
/// step through, regardless of the `max_scan` a caller asks for. This exists so a
/// caller-supplied `max_scan` cannot itself become the pathological-expansion vector
/// the recurrence budget is meant to close off.
pub const HARD_MAX_SCAN: u32 = 10_000;

/// A schedule: a firing pattern in an explicit zone, with its wake policy.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub id: String,
    pub zone: Tz,
    pub kind: ScheduleKind,
    pub missed_run_policy: MissedRunPolicy,
}

/// `t` cut to whole milliseconds, the claim table's resolution (`store::to_millis`).
///
/// Every schedule instant goes through this at creation and on load. The claim table
/// stores milliseconds, so an occurrence read back from it (a deferral retry, a
/// reclaim) carries a truncated instant. A one-shot instant with sub-millisecond
/// digits then read as later than its own stored occurrence, and
/// `SchedulerLoop::decide_deferral` gave up the deferral as "a later occurrence is
/// already due".
pub fn to_whole_millis(t: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(t.timestamp_millis()).unwrap_or(t)
}

impl Schedule {
    /// A one-shot schedule. The instant is cut to whole milliseconds
    /// ([`to_whole_millis`]). `zone` is validated even though a one-shot instant is
    /// already a UTC instant: decision 0052 asks for every schedule to carry an
    /// explicit, validated zone, because the zone is also what a work node displays the
    /// run time in.
    pub fn one_shot(
        id: impl Into<String>,
        at: DateTime<Utc>,
        zone: &str,
    ) -> Result<Self, ScheduleError> {
        let tz = parse_time_zone(zone)?;
        Ok(Self {
            id: id.into(),
            zone: tz,
            kind: ScheduleKind::OneShot {
                at: to_whole_millis(at),
            },
            missed_run_policy: MissedRunPolicy::default(),
        })
    }

    /// A recurring schedule from a cron pattern and an explicit IANA zone.
    pub fn recurring(
        id: impl Into<String>,
        pattern: &str,
        zone: &str,
    ) -> Result<Self, ScheduleError> {
        let rule = RecurrenceRule::new(pattern, zone)?;
        Ok(Self {
            id: id.into(),
            zone: rule.zone(),
            kind: ScheduleKind::Recurring {
                rule: Box::new(rule),
            },
            missed_run_policy: MissedRunPolicy::default(),
        })
    }

    pub fn with_missed_run_policy(mut self, policy: MissedRunPolicy) -> Self {
        self.missed_run_policy = policy;
        self
    }

    /// The next occurrence strictly after `after`.
    ///
    /// A one-shot schedule has exactly one occurrence: `None` once it is in the past.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        match &self.kind {
            ScheduleKind::OneShot { at } => Ok((*at > after).then_some(*at)),
            ScheduleKind::Recurring { rule } => rule.next_after(after).map(Some),
        }
    }

    /// The latest occurrence in `(after, now]`, if any - independent of
    /// `missed_run_policy`.
    ///
    /// This is the policy-*neutral* wake-catch-up candidate: `resolve_wake` applies
    /// `missed_run_policy` on top of essentially this same computation, but a caller
    /// that needs to tell "an occurrence just became due, on schedule" from "an
    /// occurrence was missed while nothing was watching" (a scheduler loop's own
    /// steady-state-versus-wake distinction) needs the unfiltered answer: under
    /// `MissedRunPolicy::Skip`, `resolve_wake` never returns a `catch_up` at all, which
    /// would wrongly suggest nothing is due even in the steady-state case where the
    /// loop ticks every few seconds and this is simply the schedule's normal, on-time
    /// firing.
    pub fn latest_occurrence_since(
        &self,
        after: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        match &self.kind {
            ScheduleKind::OneShot { at } => Ok((*at > after && *at <= now).then_some(*at)),
            ScheduleKind::Recurring { rule } => {
                let latest = rule.previous_at_or_before(now)?;
                Ok((latest > after).then_some(latest))
            }
        }
    }

    /// Resolve what to do about occurrences missed since `last_seen` (the schedule's own
    /// bookkeeping, `None` on first evaluation, meaning "nothing missed by
    /// definition"), applying `self.missed_run_policy`.
    ///
    /// Two concerns that an earlier version of this method conflated are now
    /// kept apart:
    ///
    /// - **The catch-up occurrence** (which single instant to run, under
    ///   `RunOnceOnWake`) is the latest occurrence at or before `now`
    ///   ([`RecurrenceRule::previous_at_or_before`]), computed directly and
    ///   independently of `max_scan`. The earlier version took the last element of the
    ///   capped forward scan below, which meant two callers passing different
    ///   `max_scan` values (a small one, a large one) could compute *different* catch-up
    ///   instants for the same wake-up - a scan cap should only ever affect how
    ///   precisely the *count* is known, never which occurrence actually runs.
    /// - **The missed count** is a bounded forward scan from `last_seen`, capped at
    ///   `max_scan.min(HARD_MAX_SCAN)` occurrences *and* at one shared deadline
    ///   (`DEFAULT_EXPANSION_BUDGET`) for the whole scan, not one per-call thread-backed
    ///   budget the way the first version did (3,600 threads for one hour of a
    ///   per-second schedule). Past either bound, the exact count is not knowable within
    ///   budget and is reported as `MissedCount::AtLeast(n)` rather than a false-precision
    ///   exact number.
    pub fn resolve_wake(
        &self,
        last_seen: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        max_scan: u32,
    ) -> Result<WakeResolution, ScheduleError> {
        let last_seen = match last_seen {
            None => {
                return Ok(WakeResolution {
                    catch_up: None,
                    next: self.next_after(now)?,
                    missed: MissedCount::Exactly(0),
                });
            }
            Some(t) => t,
        };

        match &self.kind {
            ScheduleKind::OneShot { at } => {
                // A one-shot occurrence is missed if it was due before `now` and we
                // never advanced past it; there is exactly one to catch up.
                if *at > last_seen && *at <= now {
                    Ok(WakeResolution {
                        catch_up: Some(*at),
                        next: None,
                        missed: MissedCount::Exactly(1),
                    })
                } else {
                    Ok(WakeResolution {
                        catch_up: None,
                        next: self.next_after(now)?,
                        missed: MissedCount::Exactly(0),
                    })
                }
            }
            ScheduleKind::Recurring { rule } => {
                let (missed, cursor_after_scan) =
                    scan_missed_count(rule, last_seen, now, max_scan)?;

                let next = if cursor_after_scan > now {
                    Some(cursor_after_scan)
                } else {
                    Some(rule.next_after(now)?)
                };

                // Independent of the scan above and of `max_scan`: the actual catch-up
                // instant, if any, is simply "the latest occurrence at or before now".
                let latest_at_or_before_now = rule.previous_at_or_before(now)?;
                let catch_up_candidate =
                    (latest_at_or_before_now > last_seen).then_some(latest_at_or_before_now);

                match (self.missed_run_policy, catch_up_candidate) {
                    (MissedRunPolicy::Skip, _) | (_, None) => Ok(WakeResolution {
                        catch_up: None,
                        next,
                        missed,
                    }),
                    (MissedRunPolicy::RunOnceOnWake, Some(catch_up)) => Ok(WakeResolution {
                        catch_up: Some(catch_up),
                        next,
                        missed,
                    }),
                }
            }
        }
    }
}

/// Count occurrences in `(last_seen, now]`, capped at `max_scan.min(HARD_MAX_SCAN)`
/// occurrences and at one shared wall-clock deadline for the entire scan.
///
/// Keeps only a running counter and the last occurrence seen - no growing `Vec` of
/// every missed instant, which the first version of this scan built and then only ever
/// read the last element of. Each `rule.next_after` call is already fast and
/// thread-free (see the `recurrence` module doc); this function's own deadline is the
/// *scan's* budget, checked once per iteration, not a new per-call budget.
///
/// Returns `(count, cursor)` where `cursor` is either the first occurrence found beyond
/// `now` (ready to hand back as `next` without a second lookup) or the last occurrence
/// counted, if the scan stopped before reaching `now`.
fn scan_missed_count(
    rule: &RecurrenceRule,
    last_seen: DateTime<Utc>,
    now: DateTime<Utc>,
    max_scan: u32,
) -> Result<(MissedCount, DateTime<Utc>), ScheduleError> {
    let cap = max_scan.min(HARD_MAX_SCAN);
    let deadline = Instant::now() + DEFAULT_EXPANSION_BUDGET;
    let mut cursor = last_seen;
    let mut count: u64 = 0;
    let mut exact = true;

    for _ in 0..cap {
        if Instant::now() >= deadline {
            exact = false;
            break;
        }
        let occurrence = rule.next_after(cursor)?;
        if occurrence > now {
            cursor = occurrence;
            break;
        }
        count += 1;
        cursor = occurrence;
    }
    if count == cap as u64 {
        exact = false;
    }

    let missed = if exact {
        MissedCount::Exactly(count)
    } else {
        MissedCount::AtLeast(count)
    };
    Ok((missed, cursor))
}

/// How many occurrences were missed, honestly bounded by the scan budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedCount {
    /// The exact count, fully enumerated within budget.
    Exactly(u64),
    /// At least this many; enumeration hit the scan cap or its shared time budget
    /// before reaching `now`.
    AtLeast(u64),
}

/// What to do after the app wakes and re-evaluates a schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeResolution {
    /// The single occurrence to run now to represent every missed firing, if the
    /// policy and the miss history call for one. Always the latest occurrence at or
    /// before `now`, independent of any scan cap.
    pub catch_up: Option<DateTime<Utc>>,
    /// The next future occurrence to claim for, if any (`None` only for an exhausted
    /// one-shot).
    pub next: Option<DateTime<Utc>>,
    /// How many occurrences fell in `(last_seen, now]`.
    pub missed: MissedCount,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn latest_occurrence_since_finds_the_one_in_range_regardless_of_policy() {
        let schedule = Schedule::recurring("s1", "0 0 * * * *", "UTC")
            .unwrap()
            .with_missed_run_policy(MissedRunPolicy::Skip);
        let after = Utc.with_ymd_and_hms(2026, 1, 1, 3, 59, 40).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 4, 0, 5).unwrap();
        // `resolve_wake` under Skip would report no catch-up at all; the policy-
        // neutral primitive still finds the 04:00 firing (a scheduler loop needs
        // this to tell "on time" from "missed" before a policy applies).
        assert_eq!(
            schedule.latest_occurrence_since(after, now).unwrap(),
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 4, 0, 0).unwrap())
        );
    }

    #[test]
    fn latest_occurrence_since_is_none_when_already_accounted_for() {
        let schedule = Schedule::recurring("s1", "0 0 * * * *", "UTC").unwrap();
        let due = Utc.with_ymd_and_hms(2026, 1, 1, 4, 0, 0).unwrap();
        assert_eq!(schedule.latest_occurrence_since(due, due).unwrap(), None);
    }

    #[test]
    fn first_evaluation_has_nothing_missed() {
        let schedule = Schedule::recurring("s1", "0 0 * * * *", "UTC").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 30, 0).unwrap();
        let resolution = schedule.resolve_wake(None, now, 1000).unwrap();
        assert_eq!(resolution.catch_up, None);
        assert_eq!(resolution.missed, MissedCount::Exactly(0));
    }

    #[test]
    fn skip_policy_drops_every_missed_occurrence() {
        // Hourly schedule, last seen at 00:00, waking at 04:30: four hourly firings
        // (01:00..04:00) were missed; 05:00 is still in the future.
        let schedule = Schedule::recurring("s1", "0 0 * * * *", "UTC")
            .unwrap()
            .with_missed_run_policy(MissedRunPolicy::Skip);
        let last_seen = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 4, 30, 0).unwrap();
        let resolution = schedule.resolve_wake(Some(last_seen), now, 1000).unwrap();
        assert_eq!(
            resolution.catch_up, None,
            "Skip must never produce a catch-up run"
        );
        assert_eq!(resolution.missed, MissedCount::Exactly(4));
        assert_eq!(
            resolution.next,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 5, 0, 0).unwrap())
        );
    }

    #[test]
    fn run_once_on_wake_fires_exactly_one_catch_up_not_a_backlog_storm() {
        let schedule = Schedule::recurring("s1", "0 0 * * * *", "UTC")
            .unwrap()
            .with_missed_run_policy(MissedRunPolicy::RunOnceOnWake);
        let last_seen = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 4, 30, 0).unwrap();
        let resolution = schedule.resolve_wake(Some(last_seen), now, 1000).unwrap();
        // Four ticks (01:00..04:00) were missed; exactly one catch-up fires, at the
        // most recent missed tick, never four.
        assert_eq!(
            resolution.catch_up,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 4, 0, 0).unwrap())
        );
        assert_eq!(resolution.missed, MissedCount::Exactly(4));
        assert_eq!(
            resolution.next,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 5, 0, 0).unwrap())
        );
    }

    #[test]
    fn nothing_missed_means_no_catch_up_under_either_policy() {
        let schedule = Schedule::recurring("s1", "0 0 * * * *", "UTC")
            .unwrap()
            .with_missed_run_policy(MissedRunPolicy::RunOnceOnWake);
        let last_seen = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 30, 0).unwrap();
        let resolution = schedule.resolve_wake(Some(last_seen), now, 1000).unwrap();
        assert_eq!(resolution.catch_up, None);
        assert_eq!(resolution.missed, MissedCount::Exactly(0));
    }

    #[test]
    fn a_missed_one_shot_catches_up_once() {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 3, 0, 0).unwrap();
        let schedule = Schedule::one_shot("s1", at, "UTC").unwrap();
        let last_seen = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 6, 0, 0).unwrap();
        let resolution = schedule.resolve_wake(Some(last_seen), now, 1000).unwrap();
        assert_eq!(resolution.catch_up, Some(at));
        assert_eq!(resolution.next, None);
    }

    #[test]
    fn a_scan_cap_reports_at_least_not_a_guess() {
        // A per-second schedule with a tiny scan cap over a long gap: the exact count
        // is not knowable within the cap, so it must say so rather than report a
        // number that looks exact but is not.
        let schedule = Schedule::recurring("s1", "* * * * * *", "UTC")
            .unwrap()
            .with_missed_run_policy(MissedRunPolicy::Skip);
        let last_seen = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap();
        let resolution = schedule.resolve_wake(Some(last_seen), now, 10).unwrap();
        assert_eq!(resolution.missed, MissedCount::AtLeast(10));
    }

    #[test]
    fn the_catch_up_instant_does_not_depend_on_the_scan_cap() {
        // The earlier defect: the first version's catch-up was
        // the last element of the capped forward scan, so a small cap and a large cap
        // computed *different* catch-up occurrences for the same wake-up over a long
        // gap. A cap of 10 used to give 00:10 and a cap of 100 used to give 01:40 on a
        // 24h gap for a per-minute schedule; both must now give the same, correct
        // answer: the actual latest occurrence at or before `now`.
        let schedule = Schedule::recurring("s1", "0 * * * * *", "UTC")
            .unwrap()
            .with_missed_run_policy(MissedRunPolicy::RunOnceOnWake);
        let last_seen = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = last_seen + chrono::Duration::hours(24);
        let expected_catch_up = now; // per-minute schedule: `now` itself is a firing.

        let small_cap = schedule.resolve_wake(Some(last_seen), now, 10).unwrap();
        let large_cap = schedule.resolve_wake(Some(last_seen), now, 2000).unwrap();

        assert_eq!(small_cap.catch_up, Some(expected_catch_up));
        assert_eq!(large_cap.catch_up, Some(expected_catch_up));
        assert_eq!(
            small_cap.catch_up, large_cap.catch_up,
            "the catch-up instant must not depend on max_scan"
        );
        // The two calls' *counts* are allowed to differ (one is capped, one is not);
        // only the catch-up instant is required to agree.
        assert_eq!(small_cap.missed, MissedCount::AtLeast(10));
        assert_eq!(large_cap.missed, MissedCount::Exactly(24 * 60));
    }

    #[test]
    fn a_large_gap_on_a_frequent_schedule_resolves_quickly_with_no_thread_per_occurrence() {
        // Regression proxy for the thread-per-call defect: a per-second schedule with
        // a one-hour gap used to spawn 3,600 OS threads (one per `next_after` call) in
        // the old implementation. The rewritten scan calls `next_after` directly, in a
        // loop, with one shared deadline; this should complete in well under a second
        // even with a scan cap far above the actual gap size.
        let schedule = Schedule::recurring("s1", "* * * * * *", "UTC")
            .unwrap()
            .with_missed_run_policy(MissedRunPolicy::Skip);
        let last_seen = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = last_seen + chrono::Duration::hours(1);
        let started = Instant::now();
        let resolution = schedule
            .resolve_wake(Some(last_seen), now, HARD_MAX_SCAN)
            .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(resolution.missed, MissedCount::Exactly(3600));
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "expected a thread-free scan of 3,600 occurrences to be fast, took {elapsed:?}"
        );
    }
}
