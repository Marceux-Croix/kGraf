//! The run's wall-clock limit, applied to the workers.
//!
//! A run given a time limit (`OrchestratorConfig::time_limit`, `kgraf
//! orchestrate --time-limit`) is measured from the moment it opens. A share of
//! the limit at its end is held back for what follows the last worker: the
//! verification of its tree, the merge and the run-wide gate. Every attempt
//! the run starts gets the rest as its deadline
//! (`kgraf_runtime::WorkerDeadline`), and stops at a round boundary before it
//! (`kgraf-runtime`'s `deadline.rs`), so what it delivered is verified and
//! merged inside the limit.
//!
//! ```text
//!   limit 900 s, reserve 90 s          worker deadline at 810 s
//!   |---- planner, workers ----------------|-- verify, merge, gate --|
//!   0                                     810                       900
//! ```
//!
//! No attempt starts (a dispatch, a retry, a throttled parallel attempt, a
//! resolver, a hand-back's continuation) when the time left before the
//! worker deadline is below the floor: the shortest round measured in this
//! run (the fastest route's), or, before any round is measured, a default.
//! The refusal is a notice and a trace record; the run ends with what it has.
//!
//! Without a time limit nothing here applies.

use std::collections::BTreeMap;
use std::time::Duration;

/// Percent of the limit held back after the worker deadline. Measured
/// Terminal-Bench runs verified a tree in 0.3 s and ran the run-wide gate in
/// 0.2 s, but a gate that runs a test suite takes tens of seconds, and a tool
/// call stopped at the deadline gets up to its cancel grace. A tenth of a
/// 900 s window (90 s) holds a gate run of a minute with room for the merge
/// and the process start before the run opened.
pub(crate) const RESERVE_PERCENT: u64 = 10;

/// The reserve's ceiling: a long limit does not hold back more than two
/// minutes of worker time.
pub(crate) const MAX_RESERVE_MS: u64 = 120_000;

/// The floor before any round is measured in the run. A first round on a
/// hosted route took 5 to 20 s in measured runs (the brief, one call and its
/// tool calls); 15 s starts an attempt that can make one round on a fast
/// route. A shorter limit (a few seconds, in tests) scales it down to
/// [`DEFAULT_FLOOR_PERCENT`] of the limit, so a short run can still dispatch.
pub(crate) const DEFAULT_FLOOR_MS: u64 = 15_000;

/// See [`DEFAULT_FLOOR_MS`].
pub(crate) const DEFAULT_FLOOR_PERCENT: u64 = 5;

/// The run's time limit, from when it opened.
#[derive(Clone, Debug)]
pub(crate) struct RunDeadline {
    started_ms: i64,
    limit_ms: u64,
    reserve_ms: u64,
    /// The last measured round per route (`WorkerResult::recent_round_ms`):
    /// the seed of the next worker on the route, and the floor's source.
    rounds: BTreeMap<String, u64>,
}

/// Why no attempt may start: the numbers a notice and a trace record state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoTime {
    /// Time into the run, from when it opened.
    pub elapsed_ms: u64,
    /// The worker deadline, from when the run opened.
    pub deadline_ms: u64,
    /// Time left before the worker deadline (0 past it).
    pub left_ms: u64,
    pub floor_ms: u64,
    pub limit_ms: u64,
}

fn as_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds as whole seconds, rounded.
pub(crate) fn secs(ms: u64) -> u64 {
    ms.saturating_add(500) / 1_000
}

impl NoTime {
    /// `12 s left before the worker deadline (810 s into a 900 s limit, 886 s
    /// elapsed), below the 15 s a round needs`.
    pub(crate) fn describe(&self) -> String {
        format!(
            "{} s left before the worker deadline ({} s into the run's {} s time limit, {} s \
             elapsed), below the {} s one round needs",
            secs(self.left_ms),
            secs(self.deadline_ms),
            secs(self.limit_ms),
            secs(self.elapsed_ms),
            secs(self.floor_ms)
        )
    }
}

impl RunDeadline {
    /// `None` without a limit.
    pub(crate) fn new(limit: Option<Duration>, started_ms: i64) -> Option<Self> {
        let limit_ms = as_ms(limit?);
        let reserve_ms = (limit_ms.saturating_mul(RESERVE_PERCENT) / 100).min(MAX_RESERVE_MS);
        Some(Self {
            started_ms,
            limit_ms,
            reserve_ms,
            rounds: BTreeMap::new(),
        })
    }

    pub(crate) fn limit_ms(&self) -> u64 {
        self.limit_ms
    }

    /// The worker deadline, from when the run opened.
    pub(crate) fn deadline_ms(&self) -> u64 {
        self.limit_ms.saturating_sub(self.reserve_ms)
    }

    /// Time into the run at `now_ms`.
    pub(crate) fn elapsed_ms(&self, now_ms: i64) -> u64 {
        u64::try_from(now_ms.saturating_sub(self.started_ms)).unwrap_or(0)
    }

    /// Time left before the worker deadline at `now_ms` (0 past it).
    pub(crate) fn left_ms(&self, now_ms: i64) -> u64 {
        self.deadline_ms().saturating_sub(self.elapsed_ms(now_ms))
    }

    /// A worker on `route` ended with its round measure.
    pub(crate) fn record_round(&mut self, route: &str, ms: u64) {
        self.rounds.insert(route.to_owned(), ms);
    }

    /// The last measured round on `route`, if any.
    pub(crate) fn round_on(&self, route: &str) -> Option<u64> {
        self.rounds.get(route).copied()
    }

    /// The least time an attempt needs to start: the fastest route's last
    /// measured round, else the default.
    pub(crate) fn floor_ms(&self) -> u64 {
        self.rounds.values().min().copied().unwrap_or_else(|| {
            DEFAULT_FLOOR_MS.min(self.limit_ms.saturating_mul(DEFAULT_FLOOR_PERCENT) / 100)
        })
    }

    /// `Err` when no attempt may start at `now_ms`.
    pub(crate) fn may_start(&self, now_ms: i64) -> Result<(), NoTime> {
        let left_ms = self.left_ms(now_ms);
        let floor_ms = self.floor_ms();
        if left_ms < floor_ms || left_ms == 0 {
            return Err(NoTime {
                elapsed_ms: self.elapsed_ms(now_ms),
                deadline_ms: self.deadline_ms(),
                left_ms,
                floor_ms,
                limit_ms: self.limit_ms,
            });
        }
        Ok(())
    }

    /// The deadline of a worker started at `now_ms` on `route`, on the
    /// runtime's clock.
    pub(crate) fn worker_deadline(
        &self,
        now_ms: i64,
        route: &str,
    ) -> kgraf_runtime::WorkerDeadline {
        kgraf_runtime::WorkerDeadline {
            at: tokio::time::Instant::now() + Duration::from_millis(self.left_ms(now_ms)),
            expected_round: self.round_on(route).map(Duration::from_millis),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(limit_s: u64) -> RunDeadline {
        RunDeadline::new(Some(Duration::from_secs(limit_s)), 1_000).expect("limit")
    }

    #[test]
    fn no_limit_no_deadline() {
        assert!(RunDeadline::new(None, 0).is_none());
    }

    #[test]
    fn the_reserve_is_a_tenth_of_the_limit_up_to_two_minutes() {
        assert_eq!(run(900).deadline_ms(), 810_000);
        assert_eq!(run(3_600).deadline_ms(), 3_480_000);
        assert_eq!(run(10).deadline_ms(), 9_000);
    }

    #[test]
    fn the_measured_run_would_not_start_its_last_attempt() {
        // The diagnosed run: a new attempt 886.8 s into a 900 s window.
        let d = run(900);
        let e = d.may_start(1_000 + 886_800).unwrap_err();
        assert_eq!((e.left_ms, e.floor_ms), (0, DEFAULT_FLOOR_MS));
        assert!(e.describe().contains("0 s left"), "{}", e.describe());
    }

    #[test]
    fn the_floor_is_the_fastest_measured_round() {
        let mut d = run(900);
        assert_eq!(d.may_start(1_000 + 700_000), Ok(()), "110 s left");
        d.record_round("slow", 162_000);
        d.record_round("fast", 5_000);
        assert_eq!(d.floor_ms(), 5_000);
        assert_eq!(
            d.may_start(1_000 + 800_000),
            Ok(()),
            "10 s left, 5 s rounds"
        );
        d.record_round("fast", 30_000);
        assert!(d.may_start(1_000 + 800_000).is_err());
    }

    #[test]
    fn a_short_limit_scales_the_default_floor() {
        let d = run(4);
        assert_eq!(d.floor_ms(), 200);
        assert_eq!(d.may_start(1_000), Ok(()));
    }

    #[test]
    fn a_worker_is_seeded_with_its_routes_round() {
        let mut d = run(900);
        d.record_round("r", 41_000);
        let w = d.worker_deadline(1_000, "r");
        assert_eq!(w.expected_round, Some(Duration::from_secs(41)));
        assert_eq!(d.worker_deadline(1_000, "other").expected_round, None);
    }
}
