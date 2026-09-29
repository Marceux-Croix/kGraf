//! Cross-feature pairing preference (decision 0022, mechanism 2).
//!
//! **The rule:** deprioritize concurrent dispatch of
//! tasks whose descriptions share significant terms; a set intersection over
//! text, no runtime coordination. A measurement found that commit pairs sharing
//! a significant subject term collide 2.25x (Python) and 2.28x (Rust) as often,
//! so "sibling" work (one feature split into parts) is the worst pair to run
//! together and cross-feature work the best.
//!
//! **Significant term** follows the probe's vocabulary: a word of four or more
//! characters after stopword removal ([`crate::templates::terms`], reused, with
//! its three-character floor raised to four).
//!
//! **A preference, never a gate** (0022: contamination 0.18 to 0.23 is far
//! above a safety bar). It only reorders the contracts that are already
//! eligible, so it changes which pair runs together when the cap binds and
//! never keeps anything from dispatching. The user's order (0014) is applied
//! first and is not reordered.
//!
//! **Speculative conflicts count as maximal overlap** (decision 0074): a pair a
//! speculative merge found conflicting is the ground truth the text only
//! approximates, so a contract of such a pair is placed after every other
//! candidate while its partner runs.
//!
//! Written new: a greedy order over term-set overlaps is a few lines; the
//! searched sources (the 0055 reuse survey, Haystack's scheduler, already
//! ported for leases in `kgraf_graph::leases::schedule`) order by read and
//! write sets, not by text.

use std::collections::BTreeSet;

/// Words of four or more characters, lowercased, less stopwords: the
/// "significant term" of that measurement.
pub fn significant_terms(text: &str) -> BTreeSet<String> {
    crate::templates::terms(text)
        .into_iter()
        .filter(|t| t.chars().count() >= 4)
        .collect()
}

/// One candidate or running contract, as the preference sees it.
#[derive(Clone, Debug)]
pub struct PairingItem {
    pub id: String,
    pub terms: BTreeSet<String>,
}

impl PairingItem {
    pub fn new(id: impl Into<String>, description: &str) -> Self {
        Self {
            id: id.into(),
            terms: significant_terms(description),
        }
    }
}

/// How strongly `a` and `b` look like parts of one feature: the number of
/// significant terms they share, or `usize::MAX` for a pair a speculative
/// merge found conflicting.
fn overlap(a: &PairingItem, b: &PairingItem, conflicting: &dyn Fn(&str, &str) -> bool) -> usize {
    if conflicting(&a.id, &b.id) {
        return usize::MAX;
    }
    a.terms.intersection(&b.terms).count()
}

/// Order `candidates` for dispatch next to `running`: repeatedly take the
/// candidate whose largest overlap with anything already running (or already
/// taken this round) is smallest; ties keep the given order. So the first
/// contract dispatched is the first one given, and each later one is the
/// least feature-alike to what will run beside it.
///
/// Greedy on purpose: the candidates of one dispatch round are few (bounded by
/// the plan and the cap), so an O(n^2) pass is nothing next to a dispatch, and
/// an optimal grouping is graph coloring, which 0022 rules out ("a
/// conflict-minimizing scheduler" is not in scope).
pub fn order_for_concurrency(
    candidates: Vec<PairingItem>,
    running: &[PairingItem],
    conflicting: &dyn Fn(&str, &str) -> bool,
) -> Vec<String> {
    let mut left = candidates;
    let mut placed: Vec<PairingItem> = running.to_vec();
    let mut out = Vec::with_capacity(left.len());
    while !left.is_empty() {
        let score = |c: &PairingItem| {
            placed
                .iter()
                .map(|p| overlap(c, p, conflicting))
                .max()
                .unwrap_or(0)
        };
        // `min_by_key` keeps the first of equal keys: ties keep planned order.
        let best = left
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| score(c))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let chosen = left.remove(best);
        out.push(chosen.id.clone());
        placed.push(chosen);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str, _: &str) -> bool {
        false
    }

    #[test]
    fn significant_terms_are_four_or_more_characters_less_stopwords() {
        let t = significant_terms("Add the parser for YAML config; fix its tests");
        assert_eq!(
            t.into_iter().collect::<Vec<_>>(),
            vec!["config", "parser", "tests", "yaml"]
        );
    }

    #[test]
    fn a_cross_feature_candidate_is_paired_before_a_sibling() {
        let running = [PairingItem::new("a", "parser: tokenize string literals")];
        let order = order_for_concurrency(
            vec![
                PairingItem::new("b", "parser: tokenize numeric literals"),
                PairingItem::new("c", "billing: export invoices as CSV"),
            ],
            &running,
            &none,
        );
        assert_eq!(order, vec!["c", "b"]);
    }

    #[test]
    fn with_nothing_running_the_first_planned_goes_first_and_its_sibling_last() {
        let order = order_for_concurrency(
            vec![
                PairingItem::new("a", "parser: tokenize string literals"),
                PairingItem::new("b", "parser: tokenize numeric literals"),
                PairingItem::new("c", "billing: export invoices as CSV"),
            ],
            &[],
            &none,
        );
        assert_eq!(order, vec!["a", "c", "b"]);
    }

    #[test]
    fn disjoint_candidates_keep_planned_order() {
        let order = order_for_concurrency(
            vec![
                PairingItem::new("x", "render galaxy clusters"),
                PairingItem::new("y", "export invoices"),
                PairingItem::new("z", "rotate keychain secrets"),
            ],
            &[],
            &none,
        );
        assert_eq!(order, vec!["x", "y", "z"]);
    }

    #[test]
    fn a_speculative_conflict_outranks_any_text_overlap() {
        let running = [PairingItem::new("a", "parser: tokenize string literals")];
        let conflicting = |x: &str, y: &str| (x == "c" && y == "a") || (x == "a" && y == "c");
        let order = order_for_concurrency(
            vec![
                PairingItem::new("c", "billing: export invoices"),
                PairingItem::new("b", "parser: tokenize numeric literals"),
            ],
            &running,
            &conflicting,
        );
        assert_eq!(order, vec!["b", "c"]);
    }
}
