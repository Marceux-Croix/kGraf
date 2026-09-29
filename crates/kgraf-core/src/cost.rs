//! Cost derivation from the six-field usage form.
//!
//! Cost is derived at read time from stored usage and a price table, never
//! stored alongside the call. Prices change; the six fields and the raw blob do
//! not, and decision 0006 keeps the raw blob precisely so cost can be
//! re-derived after a pricing change.
//!
//! An unknown model is an error, not a zero. A zero would render a live cost
//! readout that is quietly wrong, which is a defect (fail loud).

use std::collections::BTreeMap;
use std::fmt;

/// A monetary amount in micro-dollars, so arithmetic is exact.
///
/// Provider prices are quoted per million tokens in dollars, which is not
/// representable in binary floating point; a long-running ledger summed in
/// `f64` drifts. Micro-dollars keep every realistic total exact in `i64`.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Money(pub i64);

impl Money {
    /// Zero.
    pub const ZERO: Money = Money(0);

    /// Build an amount from micro-dollars.
    pub fn from_micros(micros: i64) -> Self {
        Money(micros)
    }

    /// The amount in micro-dollars.
    pub fn micros(self) -> i64 {
        self.0
    }

    /// The amount in dollars, for display only. Never sum these.
    pub fn dollars(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

impl std::ops::Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0 + rhs.0)
    }
}

impl std::ops::AddAssign for Money {
    fn add_assign(&mut self, rhs: Money) {
        self.0 += rhs.0;
    }
}

impl std::iter::Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Money {
        Money(iter.map(|m| m.0).sum())
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${:.6}", self.dollars())
    }
}

/// Per-million-token prices for one model, in micro-dollars.
///
/// Every input class is priced separately. A cache read and a cache write cost
/// different multiples of the uncached rate at every provider surveyed, and
/// collapsing them is the 20x error decision 0006 names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelPrice {
    /// Micro-dollars per million uncached input tokens.
    pub uncached_input: i64,
    /// Micro-dollars per million cache-read input tokens.
    pub cache_read: i64,
    /// Micro-dollars per million cache-write input tokens.
    pub cache_write: i64,
    /// Micro-dollars per million output tokens.
    pub output: i64,
    /// Micro-dollars per million reasoning tokens.
    ///
    /// Every protocol surveyed bills reasoning at the output rate, so this is
    /// normally equal to `output`; it is a separate field because the two are
    /// separate fields in the usage form and one of them may diverge.
    pub reasoning: i64,
}

/// Failures in cost derivation.
#[derive(Debug, thiserror::Error)]
pub enum CostError {
    /// No price is recorded for this model. A missing price raises
    /// rather than costing the call at zero.
    #[error("no price recorded for model `{model}`; refusing to cost it at zero")]
    UnpricedModel { model: String },
}

/// Prices for the models a project actually uses.
///
/// Deliberately empty by default and populated from configuration. Baking a
/// price list into the binary means a stale build reports stale money, and
/// there is no way for the user to correct it without a release.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PriceTable {
    prices: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a price for a model, replacing any previous entry.
    pub fn set(&mut self, model: impl Into<String>, price: ModelPrice) {
        self.prices.insert(model.into(), price);
    }

    /// The price for a model, if one is recorded.
    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.prices.get(model)
    }

    /// Models the table knows about.
    pub fn models(&self) -> impl Iterator<Item = &str> {
        self.prices.keys().map(String::as_str)
    }

    /// Cost one call's usage.
    pub fn cost(&self, model: &str, usage: &crate::usage::TokenUsage) -> Result<Money, CostError> {
        let price = self.get(model).ok_or_else(|| CostError::UnpricedModel {
            model: model.to_owned(),
        })?;
        // Round to the nearest micro-dollar rather than truncating, so a ledger
        // of many small calls does not drift systematically low.
        fn per_million(tokens: u64, rate: i64) -> i64 {
            let numerator = tokens as i128 * rate as i128;
            ((numerator + 500_000) / 1_000_000) as i64
        }
        Ok(Money(
            per_million(usage.uncached_input, price.uncached_input)
                + per_million(usage.cache_read, price.cache_read)
                + per_million(usage.cache_write, price.cache_write)
                + per_million(usage.text_output, price.output)
                + per_million(usage.reasoning_output, price.reasoning),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Protocol, TokenUsage};

    fn price() -> ModelPrice {
        // Round numbers, not any real vendor's list.
        ModelPrice {
            uncached_input: 3_000_000,
            cache_read: 300_000,
            cache_write: 3_750_000,
            output: 15_000_000,
            reasoning: 15_000_000,
        }
    }

    #[test]
    fn unknown_model_raises_rather_than_costing_zero() {
        let table = PriceTable::new();
        let err = table
            .cost("some-model", &TokenUsage::default())
            .expect_err("an unpriced model must raise");
        assert!(matches!(err, CostError::UnpricedModel { .. }));
    }

    #[test]
    fn cache_read_and_write_are_priced_separately() {
        let mut table = PriceTable::new();
        table.set("m", price());
        let read_only = TokenUsage {
            cache_read: 1_000_000,
            ..TokenUsage::default()
        };
        let write_only = TokenUsage {
            cache_write: 1_000_000,
            ..TokenUsage::default()
        };
        let read = table.cost("m", &read_only).expect("cost");
        let write = table.cost("m", &write_only).expect("cost");
        assert_eq!(read, Money(300_000));
        assert_eq!(write, Money(3_750_000));
        // The 20x class of error: collapsing the two into one number.
        assert_ne!(read, write);
    }

    #[test]
    fn cost_is_identical_across_the_two_protocol_conventions() {
        // The same real call, reported under either convention, must cost the
        // same. This is the money-side statement of the normalization property.
        let mut table = PriceTable::new();
        table.set("m", price());
        let additive =
            TokenUsage::normalize(Protocol::Additive, 100, 900, 50, 20, 0, None).expect("additive");
        let inclusive = TokenUsage::normalize(Protocol::Inclusive, 1050, 900, 50, 20, 0, None)
            .expect("inclusive");
        assert_eq!(
            table.cost("m", &additive).expect("cost"),
            table.cost("m", &inclusive).expect("cost")
        );
    }

    #[test]
    fn money_sums_exactly() {
        // A tenth of a cent, summed a thousand times, is exactly one dollar.
        let total: Money = std::iter::repeat_n(Money(1_000), 1_000).sum();
        assert_eq!(total, Money(1_000_000));
        assert_eq!(total.dollars(), 1.0);
    }

    /// The normal form a live-call record carries, as the ledger stored it.
    fn recorded_usage(record: &str) -> (String, TokenUsage) {
        let record: serde_json::Value = serde_json::from_str(record).expect("fixture");
        let usage = &record["usage"];
        let field = |name: &str| usage[name].as_u64().expect("recorded field");
        (
            record["returned_model"].as_str().expect("model").to_owned(),
            TokenUsage {
                uncached_input: field("uncached_input"),
                cache_read: field("cache_read"),
                cache_write: field("cache_write"),
                text_output: field("text_output"),
                reasoning_output: field("reasoning_output"),
                raw: Some(usage["raw"].clone()),
            },
        )
    }

    #[test]
    fn cost_derives_from_the_three_live_call_records() {
        // Usage and cost are recorded for both on real speakers' usage, not
        // on a mock's: cost is derived at read time from recorded usage and a price
        // table, so this is the whole mechanism. The rates are round numbers, not
        // either vendor's list; the arithmetic is what is under test.
        let kimi = recorded_usage(include_str!(
            "../tests/fixtures/2026-09-26-kimi-live-call.json"
        ));
        let kimi_anthropic = recorded_usage(include_str!(
            "../tests/fixtures/2026-09-26-kimi-anthropic-live-call.json"
        ));
        let openai = recorded_usage(include_str!(
            "../tests/fixtures/2026-09-26-openai-live-call.json"
        ));
        let mut table = PriceTable::new();
        table.set("kimi-k3", price());
        table.set("gpt-6-luna", price());

        // 92 in at $3/M, 17 text + 47 reasoning at $15/M.
        assert_eq!(table.cost(&kimi.0, &kimi.1).expect("priced"), Money(1236));
        // Recorded before the thinking split was fixed (54 text, 0 reasoning). With
        // reasoning priced at the output rate the cost is the same either way, which
        // is why that defect never showed in money.
        assert_eq!(
            table
                .cost(&kimi_anthropic.0, &kimi_anthropic.1)
                .expect("priced"),
            Money(1086)
        );
        // 13 in, 5 out.
        assert_eq!(
            table.cost(&openai.0, &openai.1).expect("priced"),
            Money(114)
        );

        // A model with no price raises rather than costing zero.
        let mut partial = PriceTable::new();
        partial.set("kimi-k3", price());
        assert!(matches!(
            partial.cost(&openai.0, &openai.1),
            Err(CostError::UnpricedModel { .. })
        ));
    }
}
