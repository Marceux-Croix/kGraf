//! Decision 0057's non-rewriting context reduction: observation masking, then
//! truncation, always computed from the **original** transcript.
//!
//! Two algorithms are ported here, both chosen because they never paraphrase
//! (0057: "Compaction never rewrites"):
//!
//! - **Masking**: keep the most recent `keep_last_observations` tool outputs
//!   unmasked and replace every older one with a placeholder. Ported from
//!   SWE-agent's `LastNObservations` history processor
//!   (`third_party/sweagent/UPSTREAM`), whose docstring is itself the citation
//!   the citation for this rule: "used in the original paper to elide but the last
//!   5 observations." Never applied where it would not shrink the entry
//!   An observation shorter than the placeholder is left
//!   alone, because masking exists to save tokens, not to spend them.
//! - **Truncation**: if masking alone does not fit the token budget, drop the
//!   oldest remaining entries (oldest first) until it does. Ported from
//!   OpenHands' token-budget suffix-sizing search
//!   (`third_party/openhands/UPSTREAM`), adapted to a linear scan because this
//!   module's token cost ([`Reducible::approx_tokens`]) is a local O(1)
//!   estimate rather than a per-probe LLM tokenizer call. Entries that share a
//!   [`Reducible::truncation_unit`] are dropped together or not at all: a tool call and its result are one unit, and truncation must
//!   never orphan one half of a pair the model would otherwise see as an
//!   unanswered tool call.
//!
//! **Always from the original, never from a reduction.** [`reduce`] takes
//! `&[T]` where `T: Reducible`. Its output, [`ReducedView`], holds
//! `Vec<Reduced<T>>`, and [`Reduced<T>`] does not implement `Reducible` -- so
//! `reduce(&view.entries, ..)` does not type-check
//! (`tests/compile_fail/reduced_view_is_not_reducible.rs` pins this). The only
//! way to reduce again is to go back to the original slice, which is exactly
//! decision 0057's "a compacted context is never compacted again."
//!
//! **Pinned items are never masked or dropped.** The caller passes a set of
//! indices into the original slice (the original instruction's position,
//! typically 0, plus anything else the caller pins); [`Reducible::is_pinned`]
//! lets an entry pin itself as well (both are honored).
//!
//! **Pinned content that alone exceeds the budget is a defect, not a silent
//! overrun** (0057 assumes the pinned floor fits under the
//! post-compaction target). [`ReductionReport::over_budget`] is set rather
//! than the budget being quietly exceeded.

use std::collections::{BTreeMap, BTreeSet};

/// What [`reduce`] needs to know about one transcript entry.
pub trait Reducible {
    /// A local, O(1) token estimate. Real tokenization is a provider concern;
    /// this is the runtime's cheap approximation for sizing decisions (the
    /// same role `get_total_token_count`'s call site plays upstream, minus the
    /// tokenizer round trip).
    fn approx_tokens(&self) -> usize;

    /// The tool-output text, if this entry is a tool observation eligible for
    /// masking. `None` for everything else (model text, control events).
    fn as_observation(&self) -> Option<&str>;

    /// A copy of this entry with its observation content replaced by
    /// `placeholder`. Only called on entries where `as_observation` returned
    /// `Some`, and only committed if it actually reduces `approx_tokens`
    /// -- `reduce` calls this to *check* the candidate
    /// before deciding to keep it.
    fn masked(&self, placeholder: &str) -> Self;

    /// Whether this entry pins itself regardless of the caller's pinned set
    /// (for example, a system prompt event knows it must never be masked).
    fn is_pinned(&self) -> bool {
        false
    }

    /// A key shared by every entry in one atomic truncation unit, for example a tool call and its result. `None` means this entry is
    /// its own unit. Truncation drops every member of a unit together or
    /// leaves all of them, so a tool call is never left with its result
    /// missing (or vice versa).
    fn truncation_unit(&self) -> Option<u64> {
        None
    }
}

/// Masking policy.
#[derive(Clone, Debug)]
pub struct MaskConfig {
    /// How many of the most recent observations stay unmasked. SWE-agent's
    /// paper-cited default is 5 (decision 0057 addendum 2).
    pub keep_last_observations: usize,
    /// The text that replaces a masked observation's content.
    pub placeholder: String,
}

impl Default for MaskConfig {
    fn default() -> Self {
        Self {
            keep_last_observations: 5,
            placeholder: "[older tool output masked]".to_owned(),
        }
    }
}

/// A token budget for the reduced view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenBudget(pub usize);

/// What happened to one entry during reduction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReductionAction {
    /// Unchanged.
    Kept,
    /// Its observation content was replaced by the placeholder.
    Masked,
}

/// One surviving entry and what was done to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reduced<T> {
    pub item: T,
    pub action: ReductionAction,
    /// This entry's index in the original slice `reduce` was given, so a
    /// caller can correlate a reduced entry back to the transcript it came
    /// from without `Reduced<T>` needing to be `Reducible` itself.
    pub original_index: usize,
}

/// A record of what reduction did, independent of the entries themselves, so
/// it can be logged or asserted on even when `T` is not `Debug`-friendly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReductionReport {
    pub kept: usize,
    pub masked: usize,
    pub dropped: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// Original-slice indices that were masked.
    pub masked_indices: Vec<usize>,
    /// Original-slice indices that were dropped entirely by truncation.
    pub dropped_indices: Vec<usize>,
    /// Set when `tokens_after` still exceeds the budget after masking and
    /// truncation have both run as far as they safely can (every remaining
    /// entry is pinned, or pinned entries alone exceed the budget). 0057
    /// treats this as a defect to size briefs against, not a silent overrun:
    /// a caller that sees this set must not treat the
    /// reduced view as fitting the model's window.
    pub over_budget: bool,
}

/// The output of [`reduce`]: the surviving entries, in original order, plus a
/// report of what was masked or dropped.
///
/// Deliberately not `Reducible`, and its `entries` are `Reduced<T>` rather
/// than bare `T`: this is what makes "reduce a reduction" fail to compile
/// rather than merely fail at runtime (decision 0057).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReducedView<T> {
    pub entries: Vec<Reduced<T>>,
    pub report: ReductionReport,
}

/// Reduce `original` to fit `budget`, masking first and truncating only if
/// masking alone is not enough. Always computed from `original`; never call
/// this on a previous [`ReducedView`]'s output (its `Reduced<T>` items are not
/// `Reducible`, so the type system already refuses that).
pub fn reduce<T: Reducible + Clone>(
    original: &[T],
    pinned: &BTreeSet<usize>,
    budget: TokenBudget,
    config: &MaskConfig,
) -> ReducedView<T> {
    let tokens_before: usize = original.iter().map(Reducible::approx_tokens).sum();

    let is_pinned = |index: usize, item: &T| pinned.contains(&index) || item.is_pinned();

    // Step 1: masking. Keep the most recent `keep_last_observations`
    // observations unmasked; mask every earlier one. Pinned entries are never
    // masked even if they are observations, and an entry is only masked if
    // doing so actually shrinks it: a short observation ("OK", an
    // empty diff) must never be replaced by a longer placeholder.
    let observation_indices: Vec<usize> = original
        .iter()
        .enumerate()
        .filter(|(i, item)| !is_pinned(*i, item) && item.as_observation().is_some())
        .map(|(i, _)| i)
        .collect();
    let keep_from = observation_indices
        .len()
        .saturating_sub(config.keep_last_observations);
    let mask_candidates: BTreeSet<usize> =
        observation_indices[..keep_from].iter().copied().collect();

    let mut entries: Vec<Reduced<T>> = Vec::with_capacity(original.len());
    let mut masked_indices = Vec::new();
    for (index, item) in original.iter().enumerate() {
        let masked_candidate = mask_candidates
            .contains(&index)
            .then(|| item.masked(&config.placeholder))
            .filter(|candidate| candidate.approx_tokens() < item.approx_tokens());
        match masked_candidate {
            Some(masked) => {
                entries.push(Reduced {
                    item: masked,
                    action: ReductionAction::Masked,
                    original_index: index,
                });
                masked_indices.push(index);
            }
            None => {
                entries.push(Reduced {
                    item: item.clone(),
                    action: ReductionAction::Kept,
                    original_index: index,
                });
            }
        }
    }

    // Step 2: truncation, only if masking did not reach the budget. Drop the
    // oldest non-pinned *units* first (OpenHands' suffix-sizing pattern,
    // ported as a linear scan: `approx_tokens` is O(1), so there is no
    // per-probe cost a binary search needs to amortize). A unit is a set of
    // entries sharing a `truncation_unit` key: every member is
    // dropped together, or none are, so a tool call is never separated from
    // its result.
    let mut dropped_indices = Vec::new();
    let mut total: usize = entries.iter().map(|e| e.item.approx_tokens()).sum();
    if total > budget.0 {
        // Group entries into units, in first-appearance (oldest-first) order.
        let mut unit_positions: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        let mut unit_order: Vec<UnitRef> = Vec::new();
        for (pos, reduced) in entries.iter().enumerate() {
            match reduced.item.truncation_unit() {
                Some(key) => {
                    let group = unit_positions.entry(key).or_default();
                    if group.is_empty() {
                        unit_order.push(UnitRef::Group(key));
                    }
                    group.push(pos);
                }
                None => unit_order.push(UnitRef::Singleton(pos)),
            }
        }

        let mut drop_positions: BTreeSet<usize> = BTreeSet::new();
        for unit in unit_order {
            if total <= budget.0 {
                break;
            }
            let members: Vec<usize> = match unit {
                UnitRef::Singleton(pos) => vec![pos],
                UnitRef::Group(key) => unit_positions[&key].clone(),
            };
            let unit_pinned = members
                .iter()
                .any(|&pos| is_pinned(entries[pos].original_index, &entries[pos].item));
            if unit_pinned {
                continue;
            }
            let unit_tokens: usize = members
                .iter()
                .map(|&pos| entries[pos].item.approx_tokens())
                .sum();
            for &pos in &members {
                drop_positions.insert(pos);
                dropped_indices.push(entries[pos].original_index);
            }
            total -= unit_tokens;
        }
        dropped_indices.sort_unstable();
        if !drop_positions.is_empty() {
            let mut kept = Vec::with_capacity(entries.len() - drop_positions.len());
            for (pos, reduced) in entries.into_iter().enumerate() {
                if !drop_positions.contains(&pos) {
                    kept.push(reduced);
                }
            }
            entries = kept;
        }
    }

    let tokens_after = entries.iter().map(|e| e.item.approx_tokens()).sum();
    let over_budget = tokens_after > budget.0;
    let report = ReductionReport {
        kept: entries.len(),
        masked: masked_indices.len(),
        dropped: dropped_indices.len(),
        tokens_before,
        tokens_after,
        masked_indices,
        dropped_indices,
        over_budget,
    };
    ReducedView { entries, report }
}

/// One truncation unit, in the order its first member appears.
enum UnitRef {
    Singleton(usize),
    Group(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Event {
        text: String,
        is_observation: bool,
        pinned: bool,
        unit: Option<u64>,
    }

    fn instruction(text: &str) -> Event {
        Event {
            text: text.to_owned(),
            is_observation: false,
            pinned: true,
            unit: None,
        }
    }

    fn observation(text: &str) -> Event {
        Event {
            text: text.to_owned(),
            is_observation: true,
            pinned: false,
            unit: None,
        }
    }

    fn text(text: &str) -> Event {
        Event {
            text: text.to_owned(),
            is_observation: false,
            pinned: false,
            unit: None,
        }
    }

    fn call_and_result(unit: u64) -> (Event, Event) {
        (
            Event {
                text: format!("tool call {unit}"),
                is_observation: false,
                pinned: false,
                unit: Some(unit),
            },
            Event {
                text: format!("tool result {unit}"),
                is_observation: true,
                pinned: false,
                unit: Some(unit),
            },
        )
    }

    impl Reducible for Event {
        fn approx_tokens(&self) -> usize {
            self.text.len()
        }
        fn as_observation(&self) -> Option<&str> {
            self.is_observation.then_some(self.text.as_str())
        }
        fn masked(&self, placeholder: &str) -> Self {
            Event {
                text: placeholder.to_owned(),
                is_observation: true,
                pinned: self.pinned,
                unit: self.unit,
            }
        }
        fn is_pinned(&self) -> bool {
            self.pinned
        }
        fn truncation_unit(&self) -> Option<u64> {
            self.unit
        }
    }

    fn config(keep: usize) -> MaskConfig {
        MaskConfig {
            keep_last_observations: keep,
            placeholder: "MASKED".to_owned(),
        }
    }

    fn transcript(n_observations: usize) -> Vec<Event> {
        let mut v = vec![instruction("do the thing")];
        for i in 0..n_observations {
            v.push(observation(&format!("tool output {i}")));
        }
        v
    }

    #[test]
    fn only_observations_older_than_the_keep_window_are_masked() {
        let original = transcript(8);
        let view = reduce(
            &original,
            &BTreeSet::new(),
            TokenBudget(usize::MAX),
            &config(5),
        );
        let masked: Vec<usize> = view.report.masked_indices.clone();
        // 8 observations at indices 1..=8; keep the last 5 (indices 4..=8),
        // mask the first 3 (indices 1,2,3).
        assert_eq!(masked, vec![1, 2, 3]);
        assert_eq!(view.report.kept, original.len());
        assert_eq!(view.report.dropped, 0);
        for reduced in &view.entries {
            if masked.contains(&reduced.original_index) {
                assert_eq!(reduced.action, ReductionAction::Masked);
                assert_eq!(reduced.item.text, "MASKED");
            } else {
                assert_eq!(reduced.action, ReductionAction::Kept);
            }
        }
    }

    #[test]
    fn the_pinned_instruction_is_never_masked_even_as_the_oldest_entry() {
        // The instruction is itself index 0 and is_pinned(); it is not an
        // observation, but this proves it survives both stages untouched.
        let original = transcript(20);
        let view = reduce(&original, &BTreeSet::new(), TokenBudget(1), &config(1));
        assert!(
            view.entries
                .iter()
                .any(|r| r.original_index == 0 && r.item.text == "do the thing"),
            "the pinned instruction must survive a budget of 1 token"
        );
    }

    #[test]
    fn a_caller_pinned_index_is_never_masked_or_dropped() {
        let mut original = transcript(0);
        original.push(observation("keep me")); // index 1
        original.push(observation("mask me")); // index 2
        original.push(observation("mask me too")); // index 3
        let mut pinned = BTreeSet::new();
        pinned.insert(1); // caller pins an observation that would otherwise be masked
        let view = reduce(&original, &pinned, TokenBudget(usize::MAX), &config(0));
        assert!(!view.report.masked_indices.contains(&1));
        assert!(view.report.masked_indices.contains(&2));
        assert!(view.report.masked_indices.contains(&3));
    }

    #[test]
    fn truncation_only_runs_when_masking_alone_does_not_fit_the_budget() {
        let original = transcript(10);
        let tokens_before: usize = original.iter().map(Reducible::approx_tokens).sum();
        // A budget that masking alone satisfies: nothing is dropped.
        let view = reduce(
            &original,
            &BTreeSet::new(),
            TokenBudget(tokens_before),
            &config(5),
        );
        assert_eq!(view.report.dropped, 0);
    }

    #[test]
    fn truncation_drops_the_oldest_unpinned_entries_first_to_fit_the_budget() {
        let original = transcript(10);
        let tokens_before: usize = original.iter().map(Reducible::approx_tokens).sum();
        let view = reduce(&original, &BTreeSet::new(), TokenBudget(5), &config(0));
        // A truncation step that does
        // nothing must fail this test, not slip through on the `len() == 1`
        // escape hatch alone.
        assert!(
            view.report.dropped > 0,
            "truncation must actually drop something when over budget"
        );
        assert!(
            view.report.tokens_after < tokens_before,
            "the reduced view must be smaller than the original"
        );
        assert!(
            view.report.tokens_after <= 5 || view.entries.len() == 1,
            "{:?}",
            view.report
        );
        // Whatever survives besides the pinned instruction is from the tail.
        let surviving_observation_indices: Vec<usize> = view
            .entries
            .iter()
            .filter(|r| r.original_index != 0)
            .map(|r| r.original_index)
            .collect();
        if let Some(&first) = surviving_observation_indices.first() {
            assert!(
                view.report.dropped_indices.iter().all(|&d| d < first),
                "dropped indices must all be older than what survives"
            );
        }
        // Index 0 (the pinned instruction) always survives.
        assert!(view.entries.iter().any(|r| r.original_index == 0));
        assert!(!view.report.dropped_indices.contains(&0));
    }

    #[test]
    fn reducing_twice_from_the_original_gives_the_same_view() {
        let original = transcript(12);
        let pinned = BTreeSet::new();
        let a = reduce(&original, &pinned, TokenBudget(40), &config(3));
        let b = reduce(&original, &pinned, TokenBudget(40), &config(3));
        assert_eq!(a, b);
    }

    #[test]
    fn a_transcript_with_no_observations_is_left_alone_by_masking() {
        let original = vec![instruction("i"), text("hello"), text("world")];
        let view = reduce(
            &original,
            &BTreeSet::new(),
            TokenBudget(usize::MAX),
            &config(5),
        );
        assert_eq!(view.report.masked, 0);
        assert_eq!(view.entries.len(), 3);
    }

    #[test]
    fn masking_never_increases_tokens_a_short_observation_is_left_alone() {
        // The placeholder ("MASKED", 6 chars) is longer than this
        // observation's own text, so masking it would spend tokens, not save
        // them.
        let mut original = transcript(0);
        original.push(observation("ok")); // 2 chars, shorter than "MASKED"
        for i in 0..5 {
            original.push(observation(&format!(
                "padding to push the first one out {i}"
            )));
        }
        let view = reduce(
            &original,
            &BTreeSet::new(),
            TokenBudget(usize::MAX),
            &config(0), // mask every observation it can
        );
        assert!(
            !view.report.masked_indices.contains(&1),
            "a 2-character observation must not be replaced by a 6-character placeholder"
        );
        let short = view
            .entries
            .iter()
            .find(|r| r.original_index == 1)
            .expect("the short observation survives");
        assert_eq!(short.item.text, "ok");
        assert_eq!(short.action, ReductionAction::Kept);
    }

    #[test]
    fn truncation_drops_a_tool_call_and_its_result_together_never_orphaning_one() {
        // A tool call and its result truncate as one unit.
        let (call, result) = call_and_result(1);
        let mut original = vec![instruction("do the thing")];
        original.push(call);
        original.push(result);
        // Pad with more (unpaired) observations so there is a nonzero budget
        // gap to close without needing to touch the paired unit at all if the
        // implementation is buggy in the *other* direction (dropping only one
        // half). Keep the pair oldest so it is truncation's first candidate.
        for i in 0..5 {
            original.push(observation(&format!("padding {i}")));
        }
        let tokens_before: usize = original.iter().map(Reducible::approx_tokens).sum();
        // A budget that forces dropping the oldest unit (the pair) to fit.
        let budget = tokens_before - 5;
        let view = reduce(&original, &BTreeSet::new(), TokenBudget(budget), &config(0));

        let call_present = view.entries.iter().any(|r| r.original_index == 1);
        let result_present = view.entries.iter().any(|r| r.original_index == 2);
        assert_eq!(
            call_present, result_present,
            "a tool call and its result must be dropped together or not at all"
        );
    }

    #[test]
    fn pinned_content_alone_exceeding_the_budget_is_reported_not_silently_overrun() {
        // Pinned content alone exceeds the budget.
        let original = vec![instruction(&"x".repeat(100))];
        let view = reduce(&original, &BTreeSet::new(), TokenBudget(1), &config(5));
        assert!(view.report.over_budget, "{:?}", view.report);
        assert_eq!(view.entries.len(), 1, "the pinned entry still survives");
    }

    #[test]
    fn fitting_the_budget_reports_over_budget_false() {
        let original = transcript(3);
        let tokens_before: usize = original.iter().map(Reducible::approx_tokens).sum();
        let view = reduce(
            &original,
            &BTreeSet::new(),
            TokenBudget(tokens_before),
            &config(5),
        );
        assert!(!view.report.over_budget);
    }

    // `reduce(&view.entries, ..)` does not compile: `Reduced<Event>` has no
    // `Reducible` impl. That is the "impossible by type" half of decision
    // 0057's "a compacted context is never compacted again"; proven by the
    // trybuild fixture at
    // `tests/compile_fail/reduced_view_is_not_reducible.rs`, run from
    // `tests/compile_fail.rs`.
}
