//! Work-block sizing (decision 0057, addendum 2).
//!
//! A brief is planned so its prefix, slices and projected transcript stay under the
//! worker plan target (half the compaction point) of the route's effective window.
//! The transcript is projected **from the ledger of similar tasks**: for every past
//! worker task on the same model, the growth from its first request's context to its
//! peak, and its round count. The 90th percentile of each is used once a model has
//! [`MIN_SAMPLES`] tasks; until then the projection is a stated default, labeled as
//! such, and never passed off as measured.
//!
//! Written new: the projection is a percentile over the project's own ledger rows,
//! which no library holds (searched: none of the reuse candidates size work
//! blocks; goose and codex-rs size by turn count only).

use kgraf_core::{CostError, Money, PriceTable, TokenUsage};
use kgraf_provider::{CallRecord, TaskUsage};
use kgraf_runtime::ContextConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Ledger tasks needed on a model before its projection is measured.
pub const MIN_SAMPLES: usize = 3;
/// Starting projection per round until the ledger has history: one tool round of a
/// slice-sized read plus the model's reply. Engineering judgment, not measured.
pub const DEFAULT_TOKENS_PER_ROUND: u64 = 2_500;
/// Starting projection of rounds until the ledger has history.
pub const DEFAULT_ROUNDS: u32 = 10;

/// Where a projection came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProjectionSource {
    /// The 90th percentile of this many ledger tasks on the model.
    Ledger { tasks: usize },
    /// The stated default: no measured history yet.
    UnmeasuredDefault,
}

/// A projected work block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    /// Prefix, slices and instruction.
    pub base_tokens: u64,
    /// Projected peak context: base plus projected transcript growth.
    pub tokens: u64,
    pub rounds: u32,
    /// The plan target it is compared against.
    pub target_tokens: u64,
    pub source: ProjectionSource,
}

impl Projection {
    /// Whether the block fits under the plan target.
    pub fn fits(&self) -> bool {
        self.tokens <= self.target_tokens
    }

    /// The projected tokens no split removes: `fixed_base`, the part of the
    /// base every child carries too (the worker prefix), plus the projected
    /// transcript growth, which is the same for every block on the model.
    pub fn unsplittable_tokens(&self, fixed_base: u64) -> u64 {
        fixed_base.saturating_add(self.tokens.saturating_sub(self.base_tokens))
    }

    /// Whether a split can bring a child under the plan target: only when the
    /// part no split removes is under it by itself. Otherwise every child is
    /// over the target too, and a split only multiplies planner calls,
    /// interfaces and handshake parties.
    pub fn split_can_fit(&self, fixed_base: u64) -> bool {
        self.unsplittable_tokens(fixed_base) < self.target_tokens
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TaskHistory {
    first_input: u64,
    peak_input: u64,
    rounds: u32,
    usage: TokenUsage,
}

fn input_of(u: &TokenUsage) -> u64 {
    u.uncached_input + u.cache_read + u.cache_write
}

fn add_usage(total: &mut TokenUsage, u: &TokenUsage) {
    total.uncached_input += u.uncached_input;
    total.cache_read += u.cache_read;
    total.cache_write += u.cache_write;
    total.text_output += u.text_output;
    total.reasoning_output += u.reasoning_output;
}

/// The agent-id prefix of the orchestrator's own calls, which are not worker
/// tasks and are left out of sizing history.
pub const ORCHESTRATOR_AGENT_PREFIX: &str = "orchestrator";

/// Per-model history of worker tasks, from ledger rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sizer {
    by_model: BTreeMap<String, Vec<TaskHistory>>,
}

/// The `pct`th percentile (1 to 100) by nearest rank. `None` for an empty
/// list. One function for every percentile in the crate's sizing and speed
/// estimates.
pub(crate) fn percentile<T: Copy + Ord>(mut values: Vec<T>, pct: usize) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (values.len() * pct.min(100)).div_ceil(100);
    Some(values[rank.saturating_sub(1).min(values.len() - 1)])
}

fn p90<T: Copy + Ord>(values: Vec<T>) -> Option<T> {
    percentile(values, 90)
}

impl Sizer {
    /// History from ledger rows. Rows whose agent starts with `orchestrator` are
    /// the orchestrator's own calls and are not worker tasks.
    pub fn from_calls(calls: &[CallRecord]) -> Self {
        let mut rows: Vec<&CallRecord> = calls
            .iter()
            .filter(|c| !c.agent_id.starts_with(ORCHESTRATOR_AGENT_PREFIX))
            .collect();
        rows.sort_by_key(|c| c.started_ms);
        let mut tasks: BTreeMap<(String, String), TaskHistory> = BTreeMap::new();
        for c in rows {
            let entry = tasks
                .entry((c.model.clone(), c.task_id.clone()))
                .or_default();
            let input = input_of(&c.usage);
            if entry.rounds == 0 {
                entry.first_input = input;
            }
            entry.peak_input = entry.peak_input.max(input);
            entry.rounds += 1;
            add_usage(&mut entry.usage, &c.usage);
        }
        let mut by_model: BTreeMap<String, Vec<TaskHistory>> = BTreeMap::new();
        for ((model, _), h) in tasks {
            by_model.entry(model).or_default().push(h);
        }
        Self { by_model }
    }

    /// The same history as [`Self::from_calls`], from the ledger's SQL
    /// aggregate ([`kgraf_provider::Ledger::usage_by_task`] with
    /// [`ORCHESTRATOR_AGENT_PREFIX`] excluded) rather than a fold over every
    /// row: the ledger groups on its read connection, so refreshing the sizer
    /// never scans the table in Rust.
    pub fn from_task_usage(tasks: &[TaskUsage]) -> Self {
        let mut by_model: BTreeMap<String, Vec<TaskHistory>> = BTreeMap::new();
        for t in tasks {
            by_model
                .entry(t.model.clone())
                .or_default()
                .push(TaskHistory {
                    first_input: t.first_input,
                    peak_input: t.peak_input,
                    rounds: u32::try_from(t.calls).unwrap_or(u32::MAX),
                    usage: TokenUsage {
                        raw: None,
                        ..t.usage.clone()
                    },
                });
        }
        Self { by_model }
    }

    fn history(&self, model: &str) -> Option<&[TaskHistory]> {
        self.by_model
            .get(model)
            .map(Vec::as_slice)
            .filter(|h| h.len() >= MIN_SAMPLES)
    }

    /// Project a block of `base_tokens` on `model` with a step budget.
    pub fn project(
        &self,
        model: &str,
        base_tokens: u64,
        step_budget: u32,
        context: &ContextConfig,
    ) -> Projection {
        let target = context.worker_plan_target_tokens();
        let (growth, rounds, source) = match self.history(model) {
            Some(h) => (
                p90(h
                    .iter()
                    .map(|t| t.peak_input.saturating_sub(t.first_input))
                    .collect())
                .unwrap_or(0),
                p90(h.iter().map(|t| t.rounds).collect()).unwrap_or(1),
                ProjectionSource::Ledger { tasks: h.len() },
            ),
            None => {
                let rounds = DEFAULT_ROUNDS.min(step_budget);
                (
                    u64::from(rounds.saturating_sub(1)) * DEFAULT_TOKENS_PER_ROUND,
                    rounds,
                    ProjectionSource::UnmeasuredDefault,
                )
            }
        };
        Projection {
            base_tokens,
            tokens: base_tokens + growth,
            rounds: rounds.min(step_budget).max(1),
            target_tokens: target,
            source,
        }
    }

    /// The spend to reserve for a block: the 90th percentile of a measured task's
    /// cost on this model, or, unmeasured, every projected round billed at the
    /// projected peak context plus a full output. Conservative by construction.
    ///
    /// Each call is priced at its own size: a model's rate tier applies to one
    /// call's input, so a task's summed usage is never priced as a single call.
    /// A measured task is priced as its average call repeated for its rounds.
    pub fn estimate_cost(
        &self,
        prices: &PriceTable,
        route: Option<&str>,
        model: &str,
        projection: &Projection,
        max_output_tokens: u32,
    ) -> Result<Money, CostError> {
        if let Some(h) = self.history(model) {
            let costs = h
                .iter()
                .map(|t| {
                    let calls = u64::from(t.rounds).max(1);
                    let each = |n: u64| n.div_ceil(calls);
                    let average = TokenUsage {
                        uncached_input: each(t.usage.uncached_input),
                        cache_read: each(t.usage.cache_read),
                        cache_write: each(t.usage.cache_write),
                        text_output: each(t.usage.text_output),
                        reasoning_output: each(t.usage.reasoning_output),
                        ..TokenUsage::default()
                    };
                    prices.cost_calls_on(route, model, &average, calls)
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(p90(costs).unwrap_or(Money::ZERO));
        }
        prices.cost_calls_on(
            route,
            model,
            &dear_usage_on(
                prices,
                route,
                model,
                projection.tokens,
                u64::from(max_output_tokens),
            ),
            u64::from(projection.rounds),
        )
    }
}

/// Usage for `input` and `output` tokens with the input billed at the dearer of
/// the model's uncached and cache-write rates: a provider may write a prompt to
/// its cache on any call, so a projection at the uncached rate alone can be low.
pub fn dear_usage(prices: &PriceTable, model: &str, input: u64, output: u64) -> TokenUsage {
    dear_usage_on(prices, None, model, input, output)
}

/// [`dear_usage`] for a call on `route`: the dearer-rate comparison reads the
/// route's own price when it has one.
pub fn dear_usage_on(
    prices: &PriceTable,
    route: Option<&str>,
    model: &str,
    input: u64,
    output: u64,
) -> TokenUsage {
    let routed = route.map(|r| PriceTable::route_key(model, r));
    let price = routed
        .as_deref()
        .and_then(|k| prices.get(k))
        .or_else(|| prices.get(model));
    let cache_write_dearer = price.is_some_and(|p| p.cache_write > p.uncached_input);
    if cache_write_dearer {
        TokenUsage {
            cache_write: input,
            text_output: output,
            ..TokenUsage::default()
        }
    } else {
        TokenUsage {
            uncached_input: input,
            text_output: output,
            ..TokenUsage::default()
        }
    }
}

/// An upper bound on the tokens of text no provider has counted: two UTF-8
/// bytes per token, where the project's estimator uses three. It holds for
/// prose, code and CJK text; text denser than two bytes per token (long runs of
/// random punctuation, base64) can exceed it.
pub fn bound_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::{CallId, ModelPrice, Protocol};
    use kgraf_provider::Settlement;

    fn row(task: &str, agent: &str, model: &str, at: i64, input: u64) -> CallRecord {
        CallRecord {
            id: CallId::new(format!("{task}-{at}")),
            adapter: "fake".into(),
            protocol: Protocol::Additive,
            model: model.into(),
            agent_id: agent.into(),
            task_id: task.into(),
            settlement: Settlement::Counted,
            usage: TokenUsage {
                uncached_input: input,
                text_output: 100,
                ..TokenUsage::default()
            },
            request_id: None,
            started_ms: at,
            finished_ms: Some(at),
            trace: Default::default(),
            redacted: false,
            route: None,
            queued_ms: None,
            http_status: None,
            retry_after_ms: None,
        }
    }

    fn ctx() -> ContextConfig {
        ContextConfig::for_model(200_000).expect("config")
    }

    #[test]
    fn without_history_the_projection_is_the_labeled_default() {
        let p = Sizer::default().project("m", 5_000, 20, &ctx());
        assert_eq!(p.source, ProjectionSource::UnmeasuredDefault);
        assert_eq!(p.rounds, DEFAULT_ROUNDS);
        assert_eq!(
            p.tokens,
            5_000 + u64::from(DEFAULT_ROUNDS - 1) * DEFAULT_TOKENS_PER_ROUND
        );
        // 100,000-token ceiling, 80% point, half of it.
        assert_eq!(p.target_tokens, 40_000);
        assert!(p.fits());
    }

    /// A 64,000-token window gives a 25,600-token target; ten unmeasured
    /// rounds project 22,500 tokens of growth. With a 4,200-token prefix no
    /// child of a split can fit, whatever the instruction; with a 1,000-token
    /// prefix a large instruction can be split under it.
    #[test]
    fn a_split_helps_only_when_the_unsplittable_part_is_under_the_target() {
        let window = ContextConfig::for_model(64_000).expect("config");
        let one_line = Sizer::default().project("m", 4_200 + 10, 20, &window);
        assert_eq!(one_line.target_tokens, 25_600);
        assert!(!one_line.fits());
        assert_eq!(one_line.unsplittable_tokens(4_200), 26_700);
        assert!(!one_line.split_can_fit(4_200));
        let large = Sizer::default().project("m", 1_000 + 30_000, 20, &window);
        assert!(!large.fits());
        assert!(large.split_can_fit(1_000));
    }

    #[test]
    fn with_history_the_projection_is_the_ledgers_90th_percentile_growth() {
        let mut calls = Vec::new();
        for (i, growth) in [1_000u64, 2_000, 30_000].iter().enumerate() {
            let task = format!("t{i}");
            calls.push(row(&task, "w", "m", 0, 4_000));
            calls.push(row(&task, "w", "m", 1, 4_000 + growth / 2));
            calls.push(row(&task, "w", "m", 2, 4_000 + growth));
        }
        // The orchestrator's own calls are not worker tasks.
        calls.push(row("plan", "orchestrator:r", "m", 0, 90_000));
        let sizer = Sizer::from_calls(&calls);
        let p = sizer.project("m", 5_000, 20, &ctx());
        assert_eq!(p.source, ProjectionSource::Ledger { tasks: 3 });
        assert_eq!(p.tokens, 35_000);
        assert_eq!(p.rounds, 3);
        assert!(p.fits());
        let big = sizer.project("m", 20_000, 20, &ctx());
        assert!(!big.fits(), "20,000 base + 30,000 growth > 40,000");
        // Another model has no history.
        assert_eq!(
            sizer.project("other", 5_000, 20, &ctx()).source,
            ProjectionSource::UnmeasuredDefault
        );
    }

    #[test]
    fn the_cost_estimate_refuses_an_unpriced_model() {
        let mut prices = PriceTable::new();
        prices.set(
            "m",
            ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                output: 2_000_000,
                reasoning: 2_000_000,
            },
        );
        let sizer = Sizer::default();
        let p = sizer.project("m", 1_000, 2, &ctx());
        // 2 rounds * 3,500 tokens at $1/M + 2 * 100 at $2/M = 7,000 + 400 micros.
        assert_eq!(
            sizer
                .estimate_cost(&prices, None, "m", &p, 100)
                .expect("priced"),
            Money(7_400)
        );
        assert!(sizer.estimate_cost(&prices, None, "x", &p, 100).is_err());
    }

    /// The tier applies to one call. A tiered model whose every projected call
    /// is small is priced, though the rounds together exceed the tier.
    #[test]
    fn a_tiered_model_is_estimated_per_call_not_on_the_sum() {
        let mut prices = PriceTable::new();
        prices.set(
            "m",
            ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                output: 2_000_000,
                reasoning: 2_000_000,
            },
        );
        prices.set_tier_threshold("m", 5_000);
        let sizer = Sizer::default();
        let p = sizer.project("m", 1_000, 2, &ctx());
        // 2 rounds of 3,500 tokens: 7,000 in all, above 5,000, but each call is below.
        assert_eq!(
            sizer
                .estimate_cost(&prices, None, "m", &p, 100)
                .expect("each call is below the tier"),
            Money(7_400)
        );
    }

    /// The orchestrator sizes from the ledger's SQL aggregate, not from a fold
    /// over every row; both must give the same history.
    #[test]
    fn the_sql_aggregate_gives_the_same_sizer_as_the_row_fold() {
        let ledger = kgraf_provider::Ledger::open_in_memory().expect("ledger");
        for (agent, task, model, inputs) in [
            ("w1", "r/c1", "m", vec![4_000u64, 4_500, 5_000]),
            ("w2", "r/c2", "m", vec![3_000, 9_000]),
            ("w1", "r/c3", "n", vec![1_000]),
            ("orchestrator:r", "r", "m", vec![90_000]),
        ] {
            for input in inputs {
                let token = ledger
                    .begin("fake", Protocol::Additive, model, agent, task)
                    .expect("begin");
                token
                    .settle(
                        &TokenUsage {
                            uncached_input: input,
                            cache_read: 7,
                            text_output: 100,
                            ..TokenUsage::default()
                        },
                        None,
                    )
                    .expect("settle");
            }
        }
        // One left in flight: sizing counts it as a round, as the fold does.
        let _open = ledger
            .begin("fake", Protocol::Additive, "m", "w3", "r/c4")
            .expect("begin");
        let folded = Sizer::from_calls(&ledger.calls().expect("calls"));
        let aggregated = Sizer::from_task_usage(
            &ledger
                .usage_by_task(Some(ORCHESTRATOR_AGENT_PREFIX))
                .expect("tasks"),
        );
        assert_eq!(aggregated, folded);
        assert!(!aggregated.by_model.is_empty());
    }
}
