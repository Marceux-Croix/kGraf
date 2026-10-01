//! The step budget: the final round is held back for an answer, and the
//! model is told when three rounds are left.
//!
//! Ported in part from mini-swe-agent src/minisweagent/agents/default.py
//! (`DefaultAgent.query`), at 04d809ceab9df28f9adaed044884180159172930, MIT
//! (Copyright (c) 2025 Kilian A. Lieret and Carlos E. Jimenez). License read at
//! the source 2026-09-26.
//! Ported: a step is one model call; the limit is checked **before** each query
//! (`0 < step_limit <= n_calls` raises `LimitsExceeded`), and a run stopped by the
//! limit exits with an empty submission.
//! Not in upstream at that commit, and written here: the reserved final round and
//! the three-rounds-left notice. Upstream's limit simply ends the run; kGraf
//! spends the last round on an answer instead, and a turn that ends on a tool call
//! in that round produces no answer.

use crate::brief::{
    ALTERNATIVES_LABEL, ASSUMPTIONS_LABEL, DECISIONS_LABEL, MIN_STEP_BUDGET, RATIONALE_CLOSE,
    RATIONALE_OPEN, RATIONALE_WORD_CAP,
};
use crate::event::NoticeKind;

/// Rounds left, counting the final round, at which the model is warned.
pub const WARN_AT_ROUNDS_LEFT: u32 = 3;

/// What the loop does before one round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundPlan {
    /// Run the round. `notice` is text to add to the user turn first.
    Run {
        round: u32,
        final_round: bool,
        notice: Option<(NoticeKind, String)>,
    },
    /// The budget is spent.
    Exhausted,
}

/// Counts model rounds against a budget.
#[derive(Clone, Debug)]
pub struct StepBudget {
    budget: u32,
    used: u32,
}

impl StepBudget {
    /// A budget of `budget` rounds with `used` already spent (non-zero on resume).
    /// A budget under [`MIN_STEP_BUDGET`] is raised to it; the brief's validation
    /// refuses such a budget before this is reached.
    pub fn new(budget: u32, used: u32) -> Self {
        Self {
            budget: budget.max(MIN_STEP_BUDGET),
            used,
        }
    }

    /// Rounds spent.
    pub fn used(&self) -> u32 {
        self.used
    }

    /// Add `rounds` to the budget: a progress checkpoint granted another block
    /// (decision 0096).
    pub fn extend(&mut self, rounds: u32) {
        self.budget = self.budget.saturating_add(rounds);
    }

    /// Rounds not yet spent, the final round included.
    pub fn rounds_left(&self) -> u32 {
        self.budget.saturating_sub(self.used)
    }

    /// Plan the next round and count it as spent.
    pub fn next_round(&mut self) -> RoundPlan {
        // Checked before the query, as upstream does.
        if self.used >= self.budget {
            return RoundPlan::Exhausted;
        }
        let left = self.budget - self.used;
        self.used += 1;
        let notice = if left == 1 {
            Some((
                NoticeKind::FinalRound,
                format!(
                    "This is your final round. Tool calls made in it will not run. \
                     Give your final answer now. After it, optionally add a rationale \
                     for a contract that depends on yours, between {RATIONALE_OPEN} and \
                     {RATIONALE_CLOSE}, with the lines `{DECISIONS_LABEL}`, \
                     `{ALTERNATIVES_LABEL}` and `{ASSUMPTIONS_LABEL}`: why you decided \
                     what you decided, what you rejected and why, and what you assumed \
                     the contracts you depend on or that depend on you deliver or \
                     consume. {RATIONALE_WORD_CAP} words total across the three lines; \
                     omit the block if you have nothing worth passing on -- it is never \
                     invented on your behalf."
                ),
            ))
        } else if left == WARN_AT_ROUNDS_LEFT {
            Some((
                NoticeKind::StepsLeft,
                format!(
                    "You have {WARN_AT_ROUNDS_LEFT} rounds left, including this one. \
                     The last round is reserved for your final answer and runs no tools."
                ),
            ))
        } else {
            None
        };
        RoundPlan::Run {
            round: self.used,
            final_round: left == 1,
            notice,
        }
    }

    /// Undo the last `next_round` call for a round that never reached the
    /// provider: a steer that arrives between the count and the request
    /// leaving the process abandons the call before any model turn happens,
    /// and the retry counts its own round at the top of the loop. Without
    /// this refund, one model turn would spend two rounds of budget, and a
    /// worker steered every round could end `BudgetExhausted` with the model
    /// never having acted.
    pub fn refund_round(&mut self) {
        self.used = self.used.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(budget: u32) -> Vec<Option<NoticeKind>> {
        let mut b = StepBudget::new(budget, 0);
        let mut out = Vec::new();
        while let RoundPlan::Run { notice, .. } = b.next_round() {
            out.push(notice.map(|(k, _)| k));
        }
        out
    }

    #[test]
    fn the_warning_comes_at_three_left_and_the_last_round_is_final() {
        assert_eq!(
            kinds(5),
            vec![
                None,
                None,
                Some(NoticeKind::StepsLeft),
                None,
                Some(NoticeKind::FinalRound)
            ]
        );
        assert_eq!(kinds(2), vec![None, Some(NoticeKind::FinalRound)]);
        assert_eq!(
            kinds(3),
            vec![
                Some(NoticeKind::StepsLeft),
                None,
                Some(NoticeKind::FinalRound)
            ]
        );
    }

    #[test]
    fn only_the_last_round_is_final_and_the_budget_then_stops() {
        let mut b = StepBudget::new(3, 0);
        let finals: Vec<bool> = (0..3)
            .map(|_| match b.next_round() {
                RoundPlan::Run { final_round, .. } => final_round,
                RoundPlan::Exhausted => panic!("early"),
            })
            .collect();
        assert_eq!(finals, vec![false, false, true]);
        assert_eq!(b.next_round(), RoundPlan::Exhausted);
        assert_eq!(b.used(), 3);
    }

    #[test]
    fn a_refunded_round_can_be_recounted_without_spending_an_extra_one() {
        let mut b = StepBudget::new(3, 0);
        assert!(matches!(b.next_round(), RoundPlan::Run { round: 1, .. }));
        // The round never reached the provider (steer arrived first): refund
        // it, then the retry recounts round 1, not round 2.
        b.refund_round();
        assert_eq!(b.used(), 0);
        assert!(matches!(b.next_round(), RoundPlan::Run { round: 1, .. }));
        assert!(matches!(b.next_round(), RoundPlan::Run { round: 2, .. }));
        assert!(matches!(
            b.next_round(),
            RoundPlan::Run {
                round: 3,
                final_round: true,
                ..
            }
        ));
        assert_eq!(b.next_round(), RoundPlan::Exhausted);
    }

    #[test]
    fn rounds_left_counts_the_final_round_and_shrinks_with_each_round() {
        let mut b = StepBudget::new(4, 0);
        assert_eq!(b.rounds_left(), 4);
        b.next_round();
        assert_eq!(b.rounds_left(), 3);
        b.refund_round();
        assert_eq!(b.rounds_left(), 4);
        let resumed = StepBudget::new(4, 3);
        assert_eq!(resumed.rounds_left(), 1);
    }

    #[test]
    fn an_extended_budget_moves_the_warning_and_the_final_round() {
        let mut b = StepBudget::new(5, 0);
        b.next_round();
        b.next_round();
        b.extend(5);
        let mut kinds = Vec::new();
        while let RoundPlan::Run { notice, .. } = b.next_round() {
            kinds.push(notice.map(|(k, _)| k));
        }
        assert_eq!(kinds.len(), 8);
        assert_eq!(kinds[5], Some(NoticeKind::StepsLeft));
        assert_eq!(kinds[7], Some(NoticeKind::FinalRound));
        assert_eq!(b.used(), 10);
    }

    #[test]
    fn a_refund_on_an_untouched_budget_does_not_go_negative() {
        let mut b = StepBudget::new(3, 0);
        b.refund_round();
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn a_resumed_budget_continues_from_the_rounds_used() {
        let mut b = StepBudget::new(4, 3);
        assert!(matches!(
            b.next_round(),
            RoundPlan::Run {
                round: 4,
                final_round: true,
                ..
            }
        ));
    }
}
