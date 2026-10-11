//! The wall-clock budget of a run (`--budget-mins`).
//!
//! A host that kills a job at a fixed limit leaves no report. The runner
//! therefore stops starting work before the limit: it asks the budget before
//! each item, caps the time limit of the item it starts, and keeps a reserve
//! for the removal of the sandbox user, the cleanup and the report.
//!
//! The budget is a plain value (a start instant, a total, a reserve), so the
//! tests build one with short durations instead of faking a clock.

use std::time::{Duration, Instant};

use serde::Serialize;

/// Time kept for `kgraf sandbox remove` (its limit, 300 s), the status check
/// after it (120 s), the cleanup and the report (120 s). Used when the run
/// has a sandbox user phase.
pub const RESERVE_WITH_SANDBOX_USER: Duration = Duration::from_secs(540);
/// Time kept for the cleanup and the report alone.
pub const RESERVE_WITHOUT_SANDBOX_USER: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug)]
pub struct Budget {
    start: Instant,
    /// `None`: no budget, nothing is ever refused or capped.
    total: Option<Duration>,
    reserve: Duration,
}

/// A time limit after the budget capped it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capped {
    pub limit: Duration,
    /// The budget, not the item's own limit, set `limit`. A process that
    /// ends by timeout under this limit was stopped at the budget.
    pub by_budget: bool,
}

impl Budget {
    pub fn unlimited() -> Self {
        Self {
            start: Instant::now(),
            total: None,
            reserve: Duration::ZERO,
        }
    }

    /// A budget of `mins` minutes that starts now. `reserve` is kept free at
    /// the end.
    pub fn new(mins: u64, reserve: Duration) -> Self {
        Self::with_total(Duration::from_secs(mins.saturating_mul(60)), reserve)
    }

    pub fn with_total(total: Duration, reserve: Duration) -> Self {
        Self {
            start: Instant::now(),
            total: Some(total),
            reserve,
        }
    }

    pub fn is_limited(&self) -> bool {
        self.total.is_some()
    }

    /// The whole budget in minutes, for the report.
    pub fn mins(&self) -> Option<u64> {
        self.total.map(|t| t.as_secs() / 60)
    }

    /// Time left for work: the total, less the time used and the reserve.
    /// `None` when there is no budget.
    pub fn left(&self) -> Option<Duration> {
        self.total.map(|t| {
            t.saturating_sub(self.reserve)
                .saturating_sub(self.start.elapsed())
        })
    }

    /// The item's expected time fits in what is left. Always true with no
    /// budget. A budget that is spent refuses every item.
    pub fn allows(&self, expected: Duration) -> bool {
        match self.left() {
            None => true,
            Some(left) => !left.is_zero() && left >= expected,
        }
    }

    /// No time is left for work.
    pub fn spent(&self) -> bool {
        self.left().is_some_and(|l| l.is_zero())
    }

    /// `limit`, cut to the time left.
    pub fn cap(&self, limit: Duration) -> Capped {
        match self.left() {
            Some(left) if left < limit => Capped {
                limit: left,
                by_budget: true,
            },
            _ => Capped {
                limit,
                by_budget: false,
            },
        }
    }

    /// The record for an item the budget refused.
    pub fn refusal(&self, what: &str, expected: Duration) -> NotRun {
        NotRun {
            what: what.to_owned(),
            why: not_run_reason(expected, self.left().unwrap_or_default()),
        }
    }
}

pub fn not_run_reason(expected: Duration, left: Duration) -> String {
    format!(
        "not run: budget (expected about {} s, {} s left)",
        expected.as_secs(),
        left.as_secs()
    )
}

/// An item the budget kept from starting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NotRun {
    pub what: String,
    pub why: String,
}

/// What the budget did to the baseline of a run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BudgetReport {
    /// The budget in minutes.
    pub mins: u64,
    /// Items of the baseline that did not start.
    pub not_run: Vec<NotRun>,
    /// Items of the baseline that were running when the budget ended.
    pub stopped: Vec<String>,
}

impl BudgetReport {
    pub fn cut_anything(&self) -> bool {
        !self.not_run.is_empty() || !self.stopped.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_budget_refuses_and_caps_nothing() {
        let b = Budget::unlimited();
        assert!(b.allows(Duration::from_secs(10_000_000)));
        assert!(!b.spent());
        assert_eq!(b.left(), None);
        assert_eq!(
            b.cap(Duration::from_secs(5)),
            Capped {
                limit: Duration::from_secs(5),
                by_budget: false
            }
        );
    }

    #[test]
    fn an_item_longer_than_what_is_left_is_refused() {
        let b = Budget::with_total(Duration::from_secs(100), Duration::from_secs(30));
        assert!(b.allows(Duration::from_secs(60)));
        assert!(!b.allows(Duration::from_secs(80)), "100 - 30 reserve = 70");
    }

    #[test]
    fn the_reserve_is_never_offered_to_work() {
        let b = Budget::with_total(Duration::from_secs(30), Duration::from_secs(30));
        assert!(b.spent());
        assert!(!b.allows(Duration::from_millis(1)));
        assert_eq!(b.cap(Duration::from_secs(5)).limit, Duration::ZERO);
        assert!(b.cap(Duration::from_secs(5)).by_budget);
    }

    #[test]
    fn a_limit_under_what_is_left_stays_its_own() {
        let b = Budget::with_total(Duration::from_secs(1000), Duration::ZERO);
        let c = b.cap(Duration::from_secs(10));
        assert_eq!(c.limit, Duration::from_secs(10));
        assert!(!c.by_budget);
    }

    #[test]
    fn minutes_convert_to_a_total() {
        let b = Budget::new(85, RESERVE_WITH_SANDBOX_USER);
        assert_eq!(b.mins(), Some(85));
        let left = b.left().unwrap();
        assert!(left <= Duration::from_secs(85 * 60 - 540));
        assert!(left > Duration::from_secs(85 * 60 - 540 - 5));
    }
}
