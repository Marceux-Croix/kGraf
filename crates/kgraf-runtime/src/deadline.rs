//! A worker's wall-clock deadline: the point by which it must have stopped so
//! that the caller can still verify and merge what it left.
//!
//! A worker given a deadline ([`crate::SpawnOptions::deadline`]) measures each
//! round, from one round boundary to the next (the model call, its tool calls,
//! and a compaction when one ran). At each boundary it starts the next round
//! only when that round can plausibly end before the deadline:
//!
//! ```text
//!   no deadline                               --> the round runs
//!   remaining == 0                            --> stop (out of time)
//!   expected round > remaining                --> stop (out of time)
//!   otherwise (or nothing measured, no seed)  --> the round runs
//! ```
//!
//! A compaction's summary call runs at a boundary after this check and is
//! measured as a round; it is not cut mid-call.
//!
//! The expected round is the longest of the last [`RECENT_ROUNDS`] measured
//! rounds; before any round is measured, the caller's seed (the route's
//! measured round, when it has one). The longest recent round, not the mean
//! of all of them: on a local model rounds grow with the context (a measured
//! run went from 41 s to 162 s per call over 30 rounds), so the early short
//! rounds underpredict the next one, and one quick round between long ones
//! must not hide them.
//!
//! A round that is running when the deadline passes is cut: its model call is
//! dropped and settled in the ledger as cancelled with the usage the stream
//! reported, and running tool calls are stopped as a steer stops them (a
//! writing call gets the grace, and its declared paths count as changed). The
//! worker then stops at the boundary. Either way it ends
//! [`crate::WorkerOutcome::BudgetExhausted`] (the time budget is spent), with
//! [`crate::WorkerResult::out_of_time`] carrying the numbers that decided it.

use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;

/// How many recent rounds the expected round is taken over.
pub const RECENT_ROUNDS: usize = 3;

/// The deadline a caller gives a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerDeadline {
    /// When the worker must have stopped.
    pub at: Instant,
    /// What one round is expected to take before this worker has measured
    /// one (the route's last measured round, when the caller has one). `None`:
    /// the first round runs, and is cut at the deadline if it must be.
    pub expected_round: Option<Duration>,
}

/// Why a worker stopped at its deadline: the numbers a notice and the trace
/// state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeadlineStop {
    pub remaining_ms: u64,
    pub last_round_ms: Option<u64>,
    pub expected_round_ms: Option<u64>,
}

/// The round clock of one worker segment.
#[derive(Debug)]
pub(crate) struct RoundClock {
    deadline: Option<WorkerDeadline>,
    recent: VecDeque<Duration>,
    started: Option<Instant>,
    /// Set when the deadline cut a round in flight.
    cut: bool,
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl RoundClock {
    pub(crate) fn new(deadline: Option<WorkerDeadline>) -> Self {
        Self {
            deadline,
            recent: VecDeque::with_capacity(RECENT_ROUNDS),
            started: None,
            cut: false,
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline.map(|d| d.at)
    }

    /// Whether the deadline has passed (never, without one).
    pub(crate) fn passed(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|d| now >= d.at)
    }

    /// A round boundary: the round that was running (if any) ended at `now`.
    pub(crate) fn boundary(&mut self, now: Instant) {
        if let Some(started) = self.started.take() {
            if self.recent.len() == RECENT_ROUNDS {
                self.recent.pop_front();
            }
            self.recent
                .push_back(now.saturating_duration_since(started));
        }
    }

    /// A round starts at `now`.
    pub(crate) fn start(&mut self, now: Instant) {
        self.started = Some(now);
    }

    /// The deadline cut the round in flight.
    pub(crate) fn mark_cut(&mut self) {
        self.cut = true;
    }

    pub(crate) fn was_cut(&self) -> bool {
        self.cut
    }

    /// What the next round is expected to take: the longest recent round, or
    /// the seed before any round is measured.
    pub(crate) fn expected(&self) -> Option<Duration> {
        self.measured()
            .or_else(|| self.deadline.and_then(|d| d.expected_round))
    }

    /// The longest recent round this worker measured, without the seed.
    pub(crate) fn measured(&self) -> Option<Duration> {
        self.recent.iter().max().copied()
    }

    /// The last measured round, for the stop's numbers.
    pub(crate) fn last(&self) -> Option<Duration> {
        self.recent.back().copied()
    }

    /// `Err` when the next round must not start.
    pub(crate) fn check(&self, now: Instant) -> Result<(), DeadlineStop> {
        let Some(deadline) = self.deadline else {
            return Ok(());
        };
        let remaining = deadline.at.saturating_duration_since(now);
        let expected = self.expected();
        if remaining.is_zero() || expected.is_some_and(|e| e > remaining) {
            return Err(DeadlineStop {
                remaining_ms: ms(remaining),
                last_round_ms: self.last().map(ms),
                expected_round_ms: expected.map(ms),
            });
        }
        Ok(())
    }
}

/// Milliseconds as whole seconds, rounded, for a notice.
fn secs(ms: u64) -> u64 {
    ms.saturating_add(500) / 1_000
}

/// The worker's notice at a deadline stop (loop-only: never model input).
pub(crate) fn stop_text(stop: &DeadlineStop, cut: bool) -> String {
    let mut text = format!(
        "the worker stopped at its time limit: {} s were left before its deadline",
        secs(stop.remaining_ms)
    );
    match (stop.last_round_ms, stop.expected_round_ms) {
        (Some(last), Some(expected)) => text.push_str(&format!(
            ", its last round took {} s and the next is expected to take {} s",
            secs(last),
            secs(expected)
        )),
        (None, Some(expected)) => text.push_str(&format!(
            ", and a round on this route is expected to take {} s",
            secs(expected)
        )),
        (Some(last), None) => text.push_str(&format!(", its last round took {} s", secs(last))),
        (None, None) => {}
    }
    if cut {
        text.push_str("; the round in flight was cut at the deadline");
    }
    text.push_str("; the tree it left is handed back as it stands");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(left: Duration, seed: Option<Duration>) -> (RoundClock, Instant) {
        let now = Instant::now();
        (
            RoundClock::new(Some(WorkerDeadline {
                at: now + left,
                expected_round: seed,
            })),
            now,
        )
    }

    #[test]
    fn no_deadline_never_stops() {
        let c = RoundClock::new(None);
        assert_eq!(c.check(Instant::now()), Ok(()));
        assert!(!c.passed(Instant::now()));
    }

    #[test]
    fn the_first_round_runs_without_a_seed() {
        let (c, now) = clock(Duration::from_secs(1), None);
        assert_eq!(c.check(now), Ok(()));
    }

    #[test]
    fn a_seed_longer_than_the_time_left_stops_the_first_round() {
        let (c, now) = clock(Duration::from_secs(10), Some(Duration::from_secs(30)));
        let stop = c.check(now).unwrap_err();
        assert_eq!(stop.expected_round_ms, Some(30_000));
        assert_eq!(stop.last_round_ms, None);
    }

    #[test]
    fn the_longest_recent_round_decides() {
        // Rounds of 40 s, 160 s, 5 s with 95 s left: the 5 s round does not
        // hide the 160 s one.
        let (mut c, now) = clock(Duration::from_secs(300), None);
        let mut t = now;
        for d in [40, 160, 5] {
            c.start(t);
            t += Duration::from_secs(d);
            c.boundary(t);
        }
        assert_eq!(c.expected(), Some(Duration::from_secs(160)));
        let stop = c.check(t).unwrap_err();
        assert_eq!(
            (
                stop.remaining_ms,
                stop.last_round_ms,
                stop.expected_round_ms
            ),
            (95_000, Some(5_000), Some(160_000))
        );
    }

    #[test]
    fn only_the_last_rounds_count() {
        let (mut c, now) = clock(Duration::from_secs(1_000), None);
        let mut t = now;
        for d in [300, 10, 10, 10] {
            c.start(t);
            t += Duration::from_secs(d);
            c.boundary(t);
        }
        assert_eq!(c.expected(), Some(Duration::from_secs(10)));
        assert_eq!(c.check(t), Ok(()));
    }

    #[test]
    fn a_passed_deadline_stops_whatever_was_measured() {
        let (c, now) = clock(Duration::ZERO, None);
        assert!(c.passed(now));
        assert_eq!(c.check(now).unwrap_err().remaining_ms, 0);
    }
}
