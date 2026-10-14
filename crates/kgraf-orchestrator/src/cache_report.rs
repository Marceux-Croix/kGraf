//! What the prompt cache did for each kind of orchestrator call, from the
//! ledger: the share of input read from the cache and the write premium paid.
//! Numbers only. kGraf does not claim the cache lowered a bill: a run with the
//! cache layout on is compared with one off by these figures and by the call
//! counts beside them (`ChangeCounts::delta_calls`, `delta_reasks`).

use kgraf_core::{Money, PriceTable};
use kgraf_provider::CallRecord;
use std::collections::BTreeMap;

/// One call kind's cache figures.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheLine {
    pub kind: String,
    pub calls: u32,
    /// `cache_read / (cache_read + uncached_input + cache_write)` over the
    /// calls whose vendor reported cache fields, or `None` when none did
    /// (shown as "not reported", never as 0%) or they sent no input. A call
    /// that reported nothing is left out of the rate: its zero read is not a
    /// measurement.
    pub hit_rate: Option<f64>,
    /// Calls of the kind that reported cache fields, and their input tokens
    /// (the rate's denominator).
    pub reported_calls: u32,
    pub reported_input: u64,
    pub cache_read: u64,
    pub uncached_input: u64,
    pub cache_write: u64,
    /// What the written tokens cost beyond the uncached rate. `None` when a
    /// call that wrote has no price entry for its model.
    pub write_premium: Option<Money>,
}

impl CacheLine {
    /// The report line's text after the kind.
    pub fn text(&self) -> String {
        let rate = match self.hit_rate {
            Some(r) => {
                format!(
                    "cache read {:.1}% of input ({} of {} tokens)",
                    r * 100.0,
                    (r * self.reported_input as f64).round() as u64,
                    self.reported_input
                ) + &if self.reported_calls < self.calls {
                    format!(
                        ", {} of {} calls reported cache fields",
                        self.reported_calls, self.calls
                    )
                } else {
                    String::new()
                }
            }
            None => "cache read share not reported".to_owned(),
        };
        let premium = match (self.cache_write, self.write_premium) {
            (0, _) => "no cache write".to_owned(),
            (n, Some(p)) => format!("{n} tokens written, premium ${:.6}", p.dollars()),
            (n, None) => format!("{n} tokens written, premium unpriced"),
        };
        format!(
            "{} call{}: {rate}; {premium}",
            self.calls,
            if self.calls == 1 { "" } else { "s" }
        )
    }
}

/// Whether a usage object (as the vendor sent it) has any cache field. A
/// vendor that reports a zero read still reports; one whose object names no
/// cache field does not, and its zero is no measurement.
fn names_a_cache_field(raw: &serde_json::Value) -> bool {
    match raw {
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(k, v)| k.to_ascii_lowercase().contains("cache") || names_a_cache_field(v)),
        serde_json::Value::Array(items) => items.iter().any(names_a_cache_field),
        _ => false,
    }
}

/// The figures of each call kind in `rows`, kinds in name order.
pub fn cache_lines(rows: &[CallRecord], prices: &PriceTable) -> Vec<CacheLine> {
    #[derive(Default)]
    struct Sum {
        calls: u32,
        reported_calls: u32,
        reported_input: u64,
        reported_read: u64,
        read: u64,
        uncached: u64,
        write: u64,
        premium_micros: i128,
        unpriced_write: bool,
    }
    let mut by_kind: BTreeMap<String, Sum> = BTreeMap::new();
    for row in rows {
        let Some(kind) = row.trace.kind.as_deref() else {
            continue;
        };
        let s = by_kind.entry(kind.to_owned()).or_default();
        s.calls += 1;
        if row.usage.raw.as_ref().is_some_and(names_a_cache_field) {
            s.reported_calls += 1;
            s.reported_input +=
                row.usage.cache_read + row.usage.uncached_input + row.usage.cache_write;
            s.reported_read += row.usage.cache_read;
        }
        s.read += row.usage.cache_read;
        s.uncached += row.usage.uncached_input;
        s.write += row.usage.cache_write;
        if row.usage.cache_write > 0 {
            match prices.price_on(row.route.as_deref(), &row.model) {
                Some(p) => {
                    s.premium_micros += i128::from(row.usage.cache_write)
                        * i128::from(p.cache_write - p.uncached_input)
                        / 1_000_000;
                }
                None => s.unpriced_write = true,
            }
        }
    }
    by_kind
        .into_iter()
        .map(|(kind, s)| CacheLine {
            kind,
            calls: s.calls,
            hit_rate: (s.reported_input > 0)
                .then(|| s.reported_read as f64 / s.reported_input as f64),
            reported_calls: s.reported_calls,
            reported_input: s.reported_input,
            cache_read: s.read,
            uncached_input: s.uncached,
            cache_write: s.write,
            write_premium: (!s.unpriced_write)
                .then(|| Money::from_micros(i64::try_from(s.premium_micros).unwrap_or(i64::MAX))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::{ModelPrice, Protocol, TokenUsage};
    use kgraf_provider::Ledger;

    /// A settled row of `kind` on `model` with `usage`, through a real ledger.
    fn row(ledger: &Ledger, kind: &str, model: &str, usage: TokenUsage) -> CallRecord {
        let token = ledger
            .begin_traced(
                "a",
                Protocol::Additive,
                model,
                "orchestrator:r1",
                "r1",
                kgraf_provider::CallTrace {
                    kind: Some(kind.to_owned()),
                    ..Default::default()
                },
            )
            .expect("begin");
        let id = token.id().clone();
        token.settle(&usage, None).expect("settle");
        ledger.call(&id).expect("read").expect("row")
    }

    fn usage(read: u64, uncached: u64, write: u64, raw: Option<serde_json::Value>) -> TokenUsage {
        TokenUsage {
            uncached_input: uncached,
            cache_read: read,
            cache_write: write,
            text_output: 10,
            raw,
            ..TokenUsage::default()
        }
    }

    fn prices() -> PriceTable {
        let mut p = PriceTable::new();
        p.set(
            "priced",
            ModelPrice {
                uncached_input: 3_000_000,
                cache_read: 300_000,
                cache_write: 3_750_000,
                output: 15_000_000,
                reasoning: 15_000_000,
            },
        );
        p
    }

    /// The hit rate is the read share of all input, the premium is the write
    /// tokens' rate above the uncached rate, per kind.
    #[test]
    fn the_hit_rate_and_the_write_premium_come_from_the_ledger_rows() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let raw = || Some(serde_json::json!({"cache_read_input_tokens": 0}));
        let rows = vec![
            row(&ledger, "replan_delta", "priced", usage(0, 200, 800, raw())),
            row(&ledger, "replan_delta", "priced", usage(800, 300, 0, raw())),
            row(&ledger, "decompose", "priced", usage(0, 500, 0, raw())),
        ];
        let lines = cache_lines(&rows, &prices());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].kind, "decompose");
        assert_eq!(lines[0].hit_rate, Some(0.0), "reported, and zero");
        let d = &lines[1];
        assert_eq!((d.kind.as_str(), d.calls), ("replan_delta", 2));
        // read 800 of (800 read + 500 uncached + 800 written) = 2100.
        assert_eq!(d.hit_rate, Some(800.0 / 2100.0));
        // 800 tokens at 0.75 dollars per million over the uncached rate.
        assert_eq!(d.write_premium, Some(Money::from_micros(600)));
        assert!(d.text().contains("2 calls"), "{}", d.text());
        assert!(d.text().contains("38.1%"), "{}", d.text());
    }

    /// Calls of one kind where some report cache fields and some do not: the
    /// rate is over the reporting calls only, and the line says how many did.
    #[test]
    fn a_mixed_kind_takes_its_rate_from_the_calls_that_reported() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let raw = Some(serde_json::json!({"cache_read_input_tokens": 0}));
        let rows = vec![
            row(&ledger, "replan_delta", "priced", usage(300, 100, 0, raw)),
            // No cache field: its 5000 uncached tokens are no evidence of a miss.
            row(&ledger, "replan_delta", "priced", usage(0, 5_000, 0, None)),
        ];
        let line = &cache_lines(&rows, &prices())[0];
        assert_eq!(line.calls, 2);
        assert_eq!((line.reported_calls, line.reported_input), (1, 400));
        assert_eq!(line.hit_rate, Some(0.75), "not 300 of 5400");
        assert_eq!(
            (line.cache_read, line.uncached_input),
            (300, 5_100),
            "totals are totals"
        );
        assert!(line.text().contains("75.0%"), "{}", line.text());
        assert!(
            line.text().contains("1 of 2 calls reported cache fields"),
            "{}",
            line.text()
        );
        // All calls reporting: no such note.
        assert!(
            !cache_lines(&rows[..1], &prices())[0]
                .text()
                .contains("reported cache fields")
        );
    }

    /// A vendor whose usage object names no cache field has no hit rate, not
    /// a zero one; a write on an unpriced model has no premium figure.
    #[test]
    fn a_vendor_without_cache_fields_is_not_reported_as_zero() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let plain = Some(serde_json::json!({"prompt_tokens": 100, "completion_tokens": 10}));
        let nested = Some(serde_json::json!({"prompt_tokens_details": {"cached_tokens": 0}}));
        let rows = vec![
            row(&ledger, "replan_delta", "priced", usage(0, 100, 0, plain)),
            row(&ledger, "triage", "priced", usage(0, 100, 0, None)),
            row(&ledger, "reground", "priced", usage(0, 100, 0, nested)),
            row(&ledger, "decompose", "unknown", usage(0, 100, 50, None)),
        ];
        let lines = cache_lines(&rows, &prices());
        let by = |k: &str| lines.iter().find(|l| l.kind == k).expect("kind");
        assert_eq!(by("replan_delta").hit_rate, None);
        assert!(by("replan_delta").text().contains("not reported"));
        assert_eq!(by("triage").hit_rate, None);
        assert_eq!(by("reground").hit_rate, Some(0.0), "reported, and zero");
        assert_eq!(by("decompose").write_premium, None);
        assert!(by("decompose").text().contains("premium unpriced"));
    }
}
