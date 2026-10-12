//! A share of the run's wall-clock limit for the optional planner asks.
//!
//! A run given a time limit (`OrchestratorConfig::time_limit`) holds the
//! planner asks that are not needed to start work to a share of that limit:
//! the interface asks and the re-plans. The first contracts ask is never held
//! back: without a plan there is no run.
//!
//! The rule, on the planner time spent so far in this run:
//!
//! ```text
//!   no limit                              --> every ask is made
//!   spent >= budget                       --> the ask is skipped
//!   spent + mean ask so far > budget      --> the ask is skipped
//!   otherwise                             --> the ask is made
//! ```
//!
//! The mean is taken over every planner ask measured so far, the contracts
//! asks included. Before any ask is measured only the first line applies.

use std::time::Duration;

/// Percent of the run's limit the planner asks may use. Measured runs of a
/// 900 s task window spent 51 to 82 percent of it in planner asks and left the
/// workers 3 to 14 steps. A third is enough for a contracts ask and one or two
/// interface asks on a local model, and leaves two thirds to the workers.
pub(crate) const PLANNER_SHARE_PERCENT: u64 = 30;

/// Planner time spent in this run, against the share of the limit.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlannerBudget {
    /// `None`: no limit is known and nothing is held back.
    budget_ms: Option<u64>,
    spent_ms: u64,
    asks: u64,
}

/// Why an ask was held back: the numbers a notice and a trace record state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Exceeded {
    pub spent_ms: u64,
    pub budget_ms: u64,
    /// The mean ask so far (0 before any is measured).
    pub expected_ms: u64,
}

/// Milliseconds as whole seconds, rounded.
fn secs(ms: u64) -> u64 {
    (ms + 500) / 1_000
}

impl Exceeded {
    /// `planner time 185 s spent of a 270 s budget (30% of the run's time
    /// limit), and the next ask is expected to take 93 s`.
    pub(crate) fn describe(&self) -> String {
        format!(
            "planner time {} s spent of a {} s budget ({PLANNER_SHARE_PERCENT}% of the run's \
             time limit), and the next ask is expected to take {} s",
            secs(self.spent_ms),
            secs(self.budget_ms),
            secs(self.expected_ms)
        )
    }
}

impl PlannerBudget {
    pub(crate) fn new(time_limit: Option<Duration>) -> Self {
        let budget_ms = time_limit.map(|l| {
            let ms = u64::try_from(l.as_millis()).unwrap_or(u64::MAX);
            ms.saturating_mul(PLANNER_SHARE_PERCENT) / 100
        });
        Self {
            budget_ms,
            spent_ms: 0,
            asks: 0,
        }
    }

    /// One planner ask took `ms`.
    pub(crate) fn record(&mut self, ms: u64) {
        self.spent_ms = self.spent_ms.saturating_add(ms);
        self.asks += 1;
    }

    /// `Err` when the next optional ask should not be made.
    pub(crate) fn check(&self) -> Result<(), Exceeded> {
        let Some(budget_ms) = self.budget_ms else {
            return Ok(());
        };
        let expected_ms = self.spent_ms.checked_div(self.asks).unwrap_or(0);
        if self.spent_ms.saturating_add(expected_ms) > budget_ms || self.spent_ms >= budget_ms {
            return Err(Exceeded {
                spent_ms: self.spent_ms,
                budget_ms,
                expected_ms,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(s: u64) -> Option<Duration> {
        Some(Duration::from_secs(s))
    }

    #[test]
    fn no_limit_never_holds_an_ask_back() {
        let mut b = PlannerBudget::new(None);
        b.record(10_000_000);
        assert_eq!(b.check(), Ok(()));
    }

    #[test]
    fn the_budget_is_a_share_of_the_limit() {
        let mut b = PlannerBudget::new(limit(900));
        b.record(270_000);
        assert_eq!(b.check().unwrap_err().budget_ms, 270_000);
    }

    #[test]
    fn the_first_ask_is_allowed_before_anything_is_measured() {
        assert_eq!(PlannerBudget::new(limit(900)).check(), Ok(()));
    }

    #[test]
    fn an_ask_expected_to_end_past_the_budget_is_held_back() {
        // A measured run: a contracts ask of 81 s, an interface ask of 104 s,
        // then a third that would end past 270 s.
        let mut b = PlannerBudget::new(limit(900));
        b.record(81_000);
        assert_eq!(b.check(), Ok(()), "162 s expected, within 270 s");
        b.record(104_000);
        let e = b.check().unwrap_err();
        assert_eq!((e.spent_ms, e.expected_ms), (185_000, 92_500));
    }

    #[test]
    fn an_ask_expected_to_end_inside_the_budget_is_made() {
        let mut b = PlannerBudget::new(limit(900));
        b.record(30_000);
        b.record(30_000);
        assert_eq!(b.check(), Ok(()));
    }
}
