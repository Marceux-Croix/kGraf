//! Token budgets for context assembly.
//!
//! A context slice is sized to a budget (an acceptance criterion), which means
//! something has to count tokens before the text is ever sent to a provider.
//! kGraf does not embed a tokenizer for that: the tokenizers differ per
//! vendor, several are not published, and the slice must be assembled before a
//! provider is even chosen.
//!
//! So the estimate is deliberately crude and deliberately *named* as an
//! estimate, and it is biased to overcount. Undercounting would silently
//! overflow a real context window; overcounting only wastes a little of the
//! budget. Once a call has been made, the provider's own reported usage is the
//! figure of record and this estimate is never reconciled against it.

/// Estimate the token count of a string.
///
/// Four bytes per token is the conventional English-prose ratio; code runs
/// denser because of punctuation and identifiers, so this uses a
/// deliberately tighter divisor and rounds up. The result is an upper bound in
/// practice, not a measurement.
pub fn estimate_tokens(text: &str) -> u64 {
    // 3 bytes per token, rounded up, plus one for the line-structure overhead
    // that whitespace-sensitive tokenizers add. Empty text costs nothing.
    if text.is_empty() {
        return 0;
    }
    (text.len() as u64).div_ceil(3) + 1
}

/// A budget being spent down while a context slice is assembled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenBudget {
    limit: u64,
    spent: u64,
}

impl TokenBudget {
    /// A budget of `limit` estimated tokens.
    pub fn new(limit: u64) -> Self {
        Self { limit, spent: 0 }
    }

    /// The budget's ceiling.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Estimated tokens committed so far.
    pub fn spent(&self) -> u64 {
        self.spent
    }

    /// Estimated tokens still available.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.spent)
    }

    /// Whether `text` would still fit.
    pub fn would_fit(&self, text: &str) -> bool {
        estimate_tokens(text) <= self.remaining()
    }

    /// Commit `text` to the budget if it fits, reporting whether it did.
    ///
    /// Returns `false` and changes nothing when it does not fit, so a caller
    /// can try a smaller candidate. It never partially commits: a half-included
    /// code element is worse context than no element.
    pub fn try_spend(&mut self, text: &str) -> bool {
        let cost = estimate_tokens(text);
        if cost > self.remaining() {
            return false;
        }
        self.spent += cost;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_free() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn the_estimate_overcounts_rather_than_undercounts() {
        // The failure that matters is an overflowed context window, so the
        // estimate must sit above the conventional four-bytes-per-token ratio.
        let text = "fn parse(input: &str) -> Result<Ast, ParseError> { todo!() }";
        let conventional = text.len() as u64 / 4;
        assert!(
            estimate_tokens(text) > conventional,
            "estimate {} must exceed the four-byte ratio {}",
            estimate_tokens(text),
            conventional
        );
    }

    #[test]
    fn a_budget_refuses_an_item_that_does_not_fit_and_stays_unchanged() {
        let mut budget = TokenBudget::new(10);
        assert!(!budget.try_spend(&"x".repeat(100)));
        assert_eq!(budget.spent(), 0);
        assert_eq!(budget.remaining(), 10);
    }

    #[test]
    fn a_budget_never_partially_commits() {
        let mut budget = TokenBudget::new(20);
        assert!(budget.try_spend("abcdef"));
        let after_first = budget.spent();
        assert!(!budget.try_spend(&"y".repeat(200)));
        assert_eq!(budget.spent(), after_first);
    }

    #[test]
    fn a_budget_fills_to_its_limit_and_no_further() {
        let mut budget = TokenBudget::new(100);
        let item = "abc"; // two estimated tokens each
        let mut accepted = 0;
        while budget.try_spend(item) {
            accepted += 1;
            assert!(accepted < 1000, "budget failed to terminate");
        }
        assert!(budget.spent() <= budget.limit());
        assert!(!budget.would_fit(item));
    }
}
