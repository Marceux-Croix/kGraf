//! Rendering `ProfileStore`'s `Source::Bench` tallies as a per-model,
//! per-kind table.

use kgraf_core::model_profile::{ProfileStore, Source, TaskKind};
use std::fmt::Write as _;

/// A per-model, per-kind table of every `Source::Bench` tally in `store`,
/// marked "bench" and carrying `suite_hash` so a reader can tell which
/// version of the suite produced it.
pub fn render(store: &ProfileStore, suite_hash: &str) -> String {
    render_with_stale(store, suite_hash, None)
}

/// [`render`], plus a line naming tallies from another suite version that were
/// left out (`stale`: that suite's hash, or "unknown"; see `suite_store`).
pub fn render_with_stale(store: &ProfileStore, suite_hash: &str, stale: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(old) = stale {
        let _ = writeln!(
            out,
            "note: bench tallies recorded under suite {old} are not shown; they measured different briefs"
        );
    }
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
    fn another_suites_tallies_are_named_not_shown() {
        let text = render_with_stale(&ProfileStore::default(), "abc123", Some("old999"));
        assert!(text.contains("old999"), "{text}");
        assert!(text.contains("not shown"), "{text}");
        assert!(text.contains("no bench tallies"), "{text}");
    }

    #[test]
    fn an_empty_store_says_so() {
        let text = render(&ProfileStore::default(), "abc123");
        assert!(text.contains("no bench tallies"));
    }
}
