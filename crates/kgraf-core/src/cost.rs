//! Cost derivation from the six-field usage form.
//!
//! Cost is derived at read time from stored usage and a price table, never
//! stored alongside the call. Prices change; the six fields and the raw blob do
//! not, and decision 0006 keeps the raw blob precisely so cost can be
//! re-derived after a pricing change.
//!
//! An unknown model is an error, not a zero. A zero would render a live cost
//! readout that is quietly wrong, which is a defect (fail loud).

use std::collections::{BTreeMap, BTreeSet};
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

/// An exact amount in pico-dollars (10^-12 dollars), for summing before
/// rounding.
///
/// A rate is micro-dollars per million tokens, so tokens times rate is an
/// exact count of pico-dollars: no division, no rounding. A 10-token call on a
/// $0.02-per-million embedding model is 200,000 pico-dollars, a fifth of a
/// micro-dollar, which [`Money`] cannot hold. Totals are summed here and
/// rounded once ([`Self::to_money`]), so a thousand such calls total $0.0002
/// rather than a thousand roundings to zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExactMoney(i128);

impl ExactMoney {
    /// Zero.
    pub const ZERO: ExactMoney = ExactMoney(0);

    /// The amount in pico-dollars.
    pub fn picos(self) -> i128 {
        self.0
    }

    /// A whole [`Money`] amount, exactly.
    pub fn from_money(money: Money) -> Self {
        ExactMoney(i128::from(money.micros()) * 1_000_000)
    }

    /// Split the rounded total of `amounts` into one [`Money`] per amount, in
    /// order, that sum to exactly `amounts.sum().to_money()`.
    ///
    /// Each share is the rounded running total less the rounded running total
    /// before it, so the rounding happens once over the whole list, as a total
    /// does, instead of once per amount. Rounding each amount alone lifts
    /// every amount under half a micro-dollar to one micro-dollar, which for
    /// many small calls (a ten-token embedding call is a fifth of a
    /// micro-dollar) overstates their sum several times over. A share may be
    /// zero where the running total did not reach the next micro-dollar; the
    /// shares still sum to the total, which is never shown as $0 when
    /// positive.
    pub fn apportion(amounts: &[ExactMoney]) -> Vec<Money> {
        let mut running = ExactMoney::ZERO;
        let mut shown = Money::ZERO;
        amounts
            .iter()
            .map(|amount| {
                running += *amount;
                let now = running.to_money();
                let share = Money::from_micros(now.micros() - shown.micros());
                shown = now;
                share
            })
            .collect()
    }

    /// The amount as [`Money`]: rounded to the nearest micro-dollar, except
    /// that a positive amount below half a micro-dollar is one micro-dollar.
    /// A priced call that cost something never shows as $0 (fail loud); the
    /// overstatement is under one micro-dollar per rounding.
    pub fn to_money(self) -> Money {
        let micros = (self.0 + 500_000).div_euclid(1_000_000);
        let micros = if self.0 > 0 && micros == 0 { 1 } else { micros };
        Money(i64::try_from(micros).unwrap_or(i64::MAX))
    }
}

impl std::ops::Add for ExactMoney {
    type Output = ExactMoney;
    fn add(self, rhs: ExactMoney) -> ExactMoney {
        ExactMoney(self.0.saturating_add(rhs.0))
    }
}

impl std::ops::AddAssign for ExactMoney {
    fn add_assign(&mut self, rhs: ExactMoney) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

impl std::iter::Sum for ExactMoney {
    fn sum<I: Iterator<Item = ExactMoney>>(iter: I) -> ExactMoney {
        iter.fold(ExactMoney::ZERO, |a, b| a + b)
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

    /// The model's rates change above an input size, this table holds only the
    /// base rates, and the call is larger than that size. Costing it at the base
    /// rate would understate it, so it is reported as not costed instead.
    #[error(
        "model `{model}` bills a higher rate above {threshold} input tokens and this call \
         used {input}; refusing to cost it at the base rate"
    )]
    AboveTierThreshold {
        model: String,
        threshold: u64,
        input: u64,
    },
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
    /// Per model, the input size above which the vendor bills different rates
    /// than `prices` holds. Not part of the user's price-table file.
    #[serde(skip)]
    tier_thresholds: BTreeMap<String, u64>,
    /// Routes priced only under their route-scoped key. Not part of the user's
    /// price-table file.
    #[serde(skip)]
    route_scoped_only: BTreeSet<String>,
}

impl PriceTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a price for a model, replacing any previous entry. A new price
    /// replaces the old entry's rate tier too: a flat override must not stay
    /// refused above the tier of the price it replaced. Call
    /// [`Self::set_tier_threshold`] after this to keep a tier.
    pub fn set(&mut self, model: impl Into<String>, price: ModelPrice) {
        let model = model.into();
        self.tier_thresholds.remove(&model);
        self.prices.insert(model, price);
    }

    /// Record that the model's rates change above `threshold` input tokens, so a
    /// larger call is refused rather than costed at the base rate.
    pub fn set_tier_threshold(&mut self, model: impl Into<String>, threshold: u64) {
        self.tier_thresholds.insert(model.into(), threshold);
    }

    /// Price calls on `route` only under their route-scoped key
    /// (see [`Self::route_key`]). The bare model name is never tried: on a route
    /// where the model string names something the account holder chose, a bare
    /// entry can describe a different model.
    pub fn require_route_scoped(&mut self, route: impl Into<String>) {
        self.route_scoped_only.insert(route.into());
    }

    /// Whether the bare model entry may price a call on `route`.
    fn bare_allowed(&self, route: Option<&str>) -> bool {
        !route.is_some_and(|r| self.route_scoped_only.contains(r))
    }

    /// Whether a call on `route` to `model` has a price entry, by the same
    /// lookup [`Self::cost_on`] uses.
    pub fn is_priced_on(&self, route: Option<&str>, model: &str) -> bool {
        self.price_on(route, model).is_some()
    }

    /// The price entry a call on `route` to `model` is costed by: the route's own
    /// entry, else the bare model entry where the route allows it.
    pub fn price_on(&self, route: Option<&str>, model: &str) -> Option<&ModelPrice> {
        route
            .and_then(|r| self.prices.get(&Self::route_key(model, r)))
            .or_else(|| {
                self.bare_allowed(route)
                    .then(|| self.prices.get(model))
                    .flatten()
            })
    }

    /// The price for a model, if one is recorded.
    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.prices.get(model)
    }

    /// Models the table knows about.
    pub fn models(&self) -> impl Iterator<Item = &str> {
        self.prices.keys().map(String::as_str)
    }

    /// The key a route-specific price is stored under: `<model>@<route>`. The
    /// same model id can be sold at different rates on different routes (a
    /// subscription route against the metered one), and only a route-specific
    /// entry can price both correctly.
    pub fn route_key(model: &str, route: &str) -> String {
        format!("{model}@{route}")
    }

    /// Cost one call's usage by model id alone.
    pub fn cost(&self, model: &str, usage: &crate::usage::TokenUsage) -> Result<Money, CostError> {
        self.cost_on(None, model, usage)
    }

    /// Cost `calls` calls that each used `per_call`, pricing every call at its
    /// own size. The tier threshold applies to one call, so pricing the sum as a
    /// single call would refuse a tiered model whose every call is small.
    pub fn cost_calls_on(
        &self,
        route: Option<&str>,
        model: &str,
        per_call: &crate::usage::TokenUsage,
        calls: u64,
    ) -> Result<Money, CostError> {
        let one = self.cost_exact_on(route, model, per_call)?;
        let calls = i128::from(calls);
        Ok(ExactMoney(one.0.saturating_mul(calls)).to_money())
    }

    /// Cost one call's usage. A price stored for the call's route
    /// (see [`Self::route_key`]) wins over the model's own entry.
    ///
    /// Rounded by [`ExactMoney::to_money`]: to the nearest micro-dollar, and
    /// never to zero for a call that cost something. A total over many calls
    /// sums [`Self::cost_exact_on`] and rounds once instead.
    pub fn cost_on(
        &self,
        route: Option<&str>,
        model: &str,
        usage: &crate::usage::TokenUsage,
    ) -> Result<Money, CostError> {
        self.cost_exact_on(route, model, usage)
            .map(ExactMoney::to_money)
    }

    /// Cost one call's usage exactly, in pico-dollars, for summing before
    /// rounding. Same lookup and tier rule as [`Self::cost_on`].
    pub fn cost_exact_on(
        &self,
        route: Option<&str>,
        model: &str,
        usage: &crate::usage::TokenUsage,
    ) -> Result<ExactMoney, CostError> {
        let routed = route.map(|r| Self::route_key(model, r));
        let key = match &routed {
            Some(k) if self.prices.contains_key(k) => k.as_str(),
            _ if self.bare_allowed(route) => model,
            // A route-scoped-only route with no scoped entry: no price.
            _ => {
                return Err(CostError::UnpricedModel {
                    model: model.to_owned(),
                });
            }
        };
        let price = self.get(key).ok_or_else(|| CostError::UnpricedModel {
            model: model.to_owned(),
        })?;
        if let Some(&threshold) = self.tier_thresholds.get(key)
            && usage.total_input() > threshold
        {
            return Err(CostError::AboveTierThreshold {
                model: model.to_owned(),
                threshold,
                input: usage.total_input(),
            });
        }
        // Tokens times a micro-dollar-per-million rate is exact in
        // pico-dollars. The rounding happens once, where the amount becomes
        // `Money`, not per usage field: five fields each rounded could drop a
        // whole small call to zero.
        fn picos(tokens: u64, rate: i64) -> i128 {
            i128::from(tokens) * i128::from(rate)
        }
        Ok(ExactMoney(
            picos(usage.uncached_input, price.uncached_input)
                + picos(usage.cache_read, price.cache_read)
                + picos(usage.cache_write, price.cache_write)
                + picos(usage.text_output, price.output)
                + picos(usage.reasoning_output, price.reasoning),
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
    fn apportioned_shares_sum_to_the_total_rounded_once_not_per_amount() {
        // Ten calls of a fifth of a micro-dollar each: 2 micro-dollars in all.
        let fifth = ExactMoney(200_000);
        let amounts = vec![fifth; 10];
        let shares = ExactMoney::apportion(&amounts);
        assert_eq!(shares.len(), 10);
        let sum: i64 = shares.iter().map(|m| m.micros()).sum();
        let total: ExactMoney = amounts.iter().copied().sum();
        assert_eq!(sum, total.to_money().micros());
        assert_eq!(sum, 2);
        // Rounded one by one, each would be one micro-dollar: five times over.
        let one_by_one: i64 = amounts.iter().map(|a| a.to_money().micros()).sum();
        assert_eq!(one_by_one, 10);
        // A positive total is never $0, and no share is negative.
        let tiny = ExactMoney::apportion(&[ExactMoney(1), ExactMoney(1)]);
        assert_eq!(tiny, vec![Money(1), Money(0)]);
        assert!(ExactMoney::apportion(&[]).is_empty());
        // Whole amounts come through unchanged.
        let whole = ExactMoney::from_money(Money(1_234));
        assert_eq!(
            ExactMoney::apportion(&[whole, whole]),
            vec![Money(1_234); 2]
        );
    }

    #[test]
    fn a_call_above_a_rate_tier_is_not_costed_at_the_base_rate() {
        let mut table = PriceTable::new();
        table.set("m", price());
        table.set_tier_threshold("m", 200_000);
        let small = TokenUsage {
            uncached_input: 200_000,
            ..TokenUsage::default()
        };
        assert!(table.cost("m", &small).is_ok());
        let large = TokenUsage {
            uncached_input: 150_000,
            cache_read: 60_000,
            ..TokenUsage::default()
        };
        let err = table.cost("m", &large).expect_err("above the tier");
        assert!(
            matches!(err, CostError::AboveTierThreshold { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_route_scoped_only_route_never_falls_back_to_the_bare_model_price() {
        let mut table = PriceTable::new();
        table.set("gpt-4o", price());
        let usage = TokenUsage {
            uncached_input: 1_000_000,
            ..TokenUsage::default()
        };
        assert!(
            table
                .cost_on(Some("azure-openai"), "gpt-4o", &usage)
                .is_ok()
        );
        table.require_route_scoped("azure-openai");
        assert!(matches!(
            table.cost_on(Some("azure-openai"), "gpt-4o", &usage),
            Err(CostError::UnpricedModel { .. })
        ));
        assert!(!table.is_priced_on(Some("azure-openai"), "gpt-4o"));
        // Other routes and the model-only lookup keep the bare entry.
        assert!(table.cost_on(Some("openai"), "gpt-4o", &usage).is_ok());
        assert!(table.cost("gpt-4o", &usage).is_ok());
        // The route-scoped entry prices it.
        table.set(PriceTable::route_key("gpt-4o", "azure-openai"), price());
        assert!(
            table
                .cost_on(Some("azure-openai"), "gpt-4o", &usage)
                .is_ok()
        );
        assert!(table.is_priced_on(Some("azure-openai"), "gpt-4o"));
    }

    #[test]
    fn setting_a_price_clears_the_tier_of_the_price_it_replaces() {
        let mut table = PriceTable::new();
        table.set("m", price());
        table.set_tier_threshold("m", 200_000);
        table.set("m", price());
        let large = TokenUsage {
            uncached_input: 300_000,
            ..TokenUsage::default()
        };
        assert!(table.cost("m", &large).is_ok());
    }

    #[test]
    fn calls_are_priced_one_by_one_against_the_tier() {
        let mut table = PriceTable::new();
        table.set("m", price());
        table.set_tier_threshold("m", 200_000);
        let call = TokenUsage {
            uncached_input: 100_000,
            ..TokenUsage::default()
        };
        // Five small calls total 500,000 input, above the tier, yet none is.
        let five = table
            .cost_calls_on(None, "m", &call, 5)
            .expect("each call is below");
        assert_eq!(five.micros(), 5 * table.cost("m", &call).unwrap().micros());
        let big = TokenUsage {
            uncached_input: 300_000,
            ..TokenUsage::default()
        };
        assert!(table.cost_calls_on(None, "m", &big, 1).is_err());
    }

    /// A $0.02-per-million embedding model, the cheapest hosted class.
    fn embedding_price() -> ModelPrice {
        ModelPrice {
            uncached_input: 20_000,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            reasoning: 0,
        }
    }

    #[test]
    fn a_priced_call_below_half_a_micro_dollar_is_never_shown_as_zero() {
        let mut table = PriceTable::new();
        table.set("e", embedding_price());
        let ten = TokenUsage {
            uncached_input: 10,
            ..TokenUsage::default()
        };
        // 10 x 20,000 = 200,000 pico-dollars: a fifth of a micro-dollar.
        assert_eq!(
            table.cost_exact_on(None, "e", &ten).expect("exact").picos(),
            200_000
        );
        assert_eq!(table.cost("e", &ten).expect("cost"), Money(1));
        // Zero tokens, or a free price, is a true zero, not the floor.
        assert_eq!(
            table.cost("e", &TokenUsage::default()).expect("cost"),
            Money::ZERO
        );
    }

    #[test]
    fn many_tiny_calls_summed_exactly_total_their_real_cost() {
        let mut table = PriceTable::new();
        table.set("e", embedding_price());
        let ten = TokenUsage {
            uncached_input: 10,
            ..TokenUsage::default()
        };
        let total: ExactMoney = (0..1_000)
            .map(|_| table.cost_exact_on(None, "e", &ten).expect("exact"))
            .sum();
        // 1,000 calls x 10 tokens x $0.02 per million = $0.0002.
        assert_eq!(total.to_money(), Money(200));
        assert_eq!(
            table.cost_calls_on(None, "e", &ten, 1_000).expect("calls"),
            Money(200)
        );
    }

    #[test]
    fn fields_are_summed_before_the_one_rounding() {
        let mut table = PriceTable::new();
        table.set(
            "m",
            ModelPrice {
                uncached_input: 300_000,
                cache_read: 300_000,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        // 0.3 + 0.3 micro-dollars: each field alone would round to 0 and the
        // sum of those roundings is 0; the exact sum rounds to 1.
        let usage = TokenUsage {
            uncached_input: 1,
            cache_read: 1,
            ..TokenUsage::default()
        };
        assert_eq!(
            table.cost_exact_on(None, "m", &usage).expect("x").picos(),
            600_000
        );
        assert_eq!(table.cost("m", &usage).expect("cost"), Money(1));
    }

    #[test]
    fn a_route_specific_price_wins_over_the_model_price() {
        let mut table = PriceTable::new();
        table.set("m", price());
        table.set(
            PriceTable::route_key("m", "subscription"),
            ModelPrice {
                uncached_input: 0,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
        );
        let usage = TokenUsage {
            uncached_input: 1_000_000,
            ..TokenUsage::default()
        };
        assert_eq!(
            table
                .cost_on(Some("subscription"), "m", &usage)
                .expect("cost"),
            Money(0)
        );
        assert_eq!(
            table.cost_on(Some("metered"), "m", &usage).expect("cost"),
            Money(3_000_000),
            "a route with no entry of its own uses the model price"
        );
        assert_eq!(table.cost("m", &usage).expect("cost"), Money(3_000_000));
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
