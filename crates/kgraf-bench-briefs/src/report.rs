//! Rendering `ProfileStore`'s `Source::Bench` tallies as a per-model,
//! per-kind table.

use kgraf_core::model_profile::{ProfileStore, Source, TaskKind};
use std::fmt::Write as _;

/// A per-model, per-kind table of every `Source::Bench` tally in `store`,
/// marked "bench" and carrying `suite_hash` so a reader can tell which
/// version of the suite produced it.
pub fn render(store: &ProfileStore, suite_hash: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "kgraf-bench-briefs  suite {suite_hash}  (source: bench)"
    );
    let _ = writeln!(
        out,
        "{:<28} {:<10} {:<14} {:>8} {:>9} {:>7} {:>16}",
        "model", "vendor", "kind", "attempts", "satisfied", "rate", "cost/satisfied"
    );
    let mut rows = 0;
    for (model, profile) in &store.profiles {
        let Some(kinds) = profile.tallies.get(&Source::Bench) else {
            continue;
        };
        for kind in TaskKind::ALL {
            let Some(tally) = kinds.get(&kind) else {
                continue;
            };
            rows += 1;
            let rate = tally
                .satisfaction_rate()
                .map_or_else(|| "-".to_owned(), |r| format!("{:.0}%", r * 100.0));
            let cost = match tally.cost_per_satisfied() {
                Some(m) => m.to_string(),
                None if tally.unpriced > 0 => "unpriced".to_owned(),
                None => "-".to_owned(),
            };
            let _ = writeln!(
                out,
                "{:<28} {:<10} {:<14} {:>8} {:>9} {:>7} {:>16}",
                model,
                profile.vendor,
                kind.as_str(),
                tally.attempts,
                tally.satisfied,
                rate,
                cost,
            );
        }
    }
    if rows == 0 {
        let _ = writeln!(out, "(no bench tallies recorded)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::cost::Money;

    #[test]
    fn the_table_shows_bench_tallies_and_never_live_ones() {
        let mut store = ProfileStore::default();
        store.record(
            "m",
            "v",
            Source::Bench,
            TaskKind::Docs,
            true,
            Some(Money::from_micros(400)),
        );
        store.record(
            "m",
            "v",
            Source::Live,
            TaskKind::Docs,
            true,
            Some(Money::from_micros(999)),
        );
        let text = render(&store, "abc123");
        assert!(text.contains("abc123"));
        assert!(text.contains("docs"));
        assert!(text.contains("$0.000400"));
        assert!(!text.contains("$0.000999"));
    }

    #[test]
    fn an_empty_store_says_so() {
        let text = render(&ProfileStore::default(), "abc123");
        assert!(text.contains("no bench tallies"));
    }
}
