//! The token-accounting normal form.
//!
//! Decision 0006 fixes two things that most client libraries get wrong.
//!
//! First, **the stored normal form is six independent fields, never derived at
//! write time**: uncached input, cache read, cache write, text output,
//! reasoning output, and the provider's raw usage blob verbatim. Three of the
//! eight protocols surveyed changed shape within a year, and the raw blob is
//! the only way to re-derive cost after a schema or pricing change.
//!
//! Second, **normalization is per protocol, not per provider**. Providers split
//! into two incompatible conventions for whether cache counts sit inside the
//! reported input total, and one vendor ships both behind a single API key, so
//! keying the arithmetic on a provider name silently corrupts every cached
//! request.

use std::fmt;

/// Whether a protocol's reported input total already contains its cache counts.
///
/// This is a property of the wire protocol, not of the company operating the
/// endpoint. The same credential against one vendor's native path and its
/// Anthropic-compatible path gives opposite semantics, which is exactly why
/// this enum exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Reported input *excludes* cache reads and writes, so the true input is
    /// the sum of all three. Anthropic's Messages API and Anthropic-compatible
    /// endpoints built to match it.
    Additive,
    /// Cache counts are carved *out of* the reported input total, so the
    /// reported total is already the true total. Everyone else.
    Inclusive,
}

impl Protocol {
    /// The wire-format name, for persistence and for display.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Additive => "additive",
            Protocol::Inclusive => "inclusive",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Failures that can arise while normalizing a provider's reported usage.
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    /// An inclusive protocol reported cache counts exceeding its own input
    /// total. Under the inclusive convention the cache counts are a subset of
    /// the reported input, so this is arithmetically impossible and means the
    /// protocol was misclassified. It raises rather than clamping (fail loud).
    #[error(
        "inclusive protocol reported input {reported_input} but cache read {cache_read} plus \
         cache write {cache_write} exceeds it; the protocol is misclassified"
    )]
    InclusiveUnderflow {
        reported_input: u64,
        cache_read: u64,
        cache_write: u64,
    },
    /// A protocol requiring a usage field did not send it.
    #[error("provider usage is missing required field `{field}`")]
    MissingField { field: &'static str },
}

/// One call's usage in kGraf's normal form.
///
/// Every field is stored as reported, independent of every other. Totals are
/// methods, never columns, so a change in how a total is defined cannot
/// invalidate stored rows.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    /// Input tokens that were neither read from nor written to the cache.
    pub uncached_input: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read: u64,
    /// Input tokens written into the provider's prompt cache.
    ///
    /// Kept separate from `cache_read` because the two are priced differently;
    /// one surveyed crate sums them into a single number, a 20x error.
    pub cache_write: u64,
    /// Output tokens that are visible text.
    pub text_output: u64,
    /// Output tokens spent on reasoning, where the protocol distinguishes them.
    pub reasoning_output: u64,
    /// The provider's usage object exactly as it arrived on the wire.
    ///
    /// `None` only for usage synthesized locally, such as a reconciliation
    /// estimate for a stream that died before delivering its usage event.
    pub raw: Option<serde_json::Value>,
}

impl TokenUsage {
    /// Normalize a protocol's reported numbers into the six-field form.
    ///
    /// `reported_input` is whatever the protocol calls its input total. Under
    /// [`Protocol::Additive`] it is the uncached portion already; under
    /// [`Protocol::Inclusive`] the cache counts are carved out of it.
    pub fn normalize(
        protocol: Protocol,
        reported_input: u64,
        cache_read: u64,
        cache_write: u64,
        text_output: u64,
        reasoning_output: u64,
        raw: Option<serde_json::Value>,
    ) -> Result<Self, UsageError> {
        let uncached_input = match protocol {
            Protocol::Additive => reported_input,
            Protocol::Inclusive => reported_input
                .checked_sub(cache_read)
                .and_then(|rest| rest.checked_sub(cache_write))
                .ok_or(UsageError::InclusiveUnderflow {
                    reported_input,
                    cache_read,
                    cache_write,
                })?,
        };
        Ok(Self {
            uncached_input,
            cache_read,
            cache_write,
            text_output,
            reasoning_output,
            raw,
        })
    }

    /// Every input token the call consumed, cached or not.
    ///
    /// This is the figure that is wrong by construction if the protocol is
    /// misclassified: adding the cache fields on an inclusive provider double
    /// counts, and failing to add them on an additive one undercounts.
    pub fn total_input(&self) -> u64 {
        self.uncached_input + self.cache_read + self.cache_write
    }

    /// Every output token the call produced.
    pub fn total_output(&self) -> u64 {
        self.text_output + self.reasoning_output
    }

    /// Input plus output.
    pub fn total(&self) -> u64 {
        self.total_input() + self.total_output()
    }

    /// Whether the call carried no usage at all, which is how a dropped stream
    /// presents before reconciliation.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Merge a streaming usage delta into this one.
    ///
    /// Anthropic's streaming usage arrives across two events and **the delta is
    /// cumulative**, so the correct operation is to take the last value seen
    /// per field rather than to sum. A widely repeated belief that the delta
    /// carries only output tokens is true of the legacy examples and false of
    /// the current schema, so every field is merged, not just the output ones.
    ///
    /// A zero in `delta` is read as "not reported" here, since a normalized
    /// delta cannot tell an absent field from a reported zero. A caller that
    /// can tell uses [`Self::merge_cumulative_reported`], so a later event's
    /// explicit zero is taken rather than left as the earlier, larger figure.
    pub fn merge_cumulative(&mut self, delta: &TokenUsage) {
        self.merge_cumulative_reported(delta, ReportedFields::nonzero(delta));
    }

    /// [`Self::merge_cumulative`], taking exactly the fields `reported` names,
    /// including a reported zero.
    pub fn merge_cumulative_reported(&mut self, delta: &TokenUsage, reported: ReportedFields) {
        fn take_last(target: &mut u64, seen: u64, reported: bool) {
            if reported {
                *target = seen;
            }
        }
        take_last(
            &mut self.uncached_input,
            delta.uncached_input,
            reported.uncached_input,
        );
        take_last(&mut self.cache_read, delta.cache_read, reported.cache_read);
        take_last(
            &mut self.cache_write,
            delta.cache_write,
            reported.cache_write,
        );
        take_last(
            &mut self.text_output,
            delta.text_output,
            reported.text_output,
        );
        take_last(
            &mut self.reasoning_output,
            delta.reasoning_output,
            reported.reasoning_output,
        );
        if delta.raw.is_some() {
            self.raw = delta.raw.clone();
        }
    }
}

/// Which fields of a usage delta the provider actually reported, as opposed
/// to fields that read as zero because the event never carried them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportedFields {
    pub uncached_input: bool,
    pub cache_read: bool,
    pub cache_write: bool,
    pub text_output: bool,
    pub reasoning_output: bool,
}

impl ReportedFields {
    /// Every field whose value in `usage` is nonzero.
    pub fn nonzero(usage: &TokenUsage) -> Self {
        Self {
            uncached_input: usage.uncached_input > 0,
            cache_read: usage.cache_read > 0,
            cache_write: usage.cache_write > 0,
            text_output: usage.text_output > 0,
            reasoning_output: usage.reasoning_output > 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn additive_protocol_sums_the_cache_fields() {
        let usage = TokenUsage::normalize(Protocol::Additive, 100, 900, 50, 20, 0, None)
            .expect("additive normalization");
        assert_eq!(usage.uncached_input, 100);
        assert_eq!(usage.total_input(), 1050);
    }

    #[test]
    fn inclusive_protocol_carves_the_cache_fields_out() {
        // A reported input of 1050 that already contains 900 cache reads and 50
        // cache writes leaves 100 genuinely uncached.
        let usage = TokenUsage::normalize(Protocol::Inclusive, 1050, 900, 50, 20, 0, None)
            .expect("inclusive normalization");
        assert_eq!(usage.uncached_input, 100);
        assert_eq!(usage.total_input(), 1050);
    }

    #[test]
    fn the_two_conventions_agree_on_the_true_total() {
        // This is the property the whole design exists to hold: the same real
        // call reported under either convention normalizes to the same totals.
        let additive =
            TokenUsage::normalize(Protocol::Additive, 100, 900, 50, 20, 5, None).expect("additive");
        let inclusive = TokenUsage::normalize(Protocol::Inclusive, 1050, 900, 50, 20, 5, None)
            .expect("inclusive");
        assert_eq!(additive, inclusive);
    }

    #[test]
    fn misclassified_protocol_raises_rather_than_clamping() {
        // Inclusive arithmetic on additive numbers underflows. Fail loud: that
        // must raise: a clamp here would hand back a plausible wrong number,
        // which is precisely the failure mode decision 0006 rejects.
        let err = TokenUsage::normalize(Protocol::Inclusive, 100, 900, 50, 20, 0, None)
            .expect_err("must not silently clamp");
        assert!(matches!(err, UsageError::InclusiveUnderflow { .. }));
    }

    #[test]
    fn cumulative_stream_deltas_are_taken_not_summed() {
        // Two events carrying a cumulative output count must end at 40, not 60.
        let mut usage = TokenUsage::default();
        usage.merge_cumulative(&TokenUsage {
            uncached_input: 100,
            text_output: 20,
            ..TokenUsage::default()
        });
        usage.merge_cumulative(&TokenUsage {
            text_output: 40,
            ..TokenUsage::default()
        });
        assert_eq!(usage.text_output, 40);
        // The field absent from the second event survives the merge.
        assert_eq!(usage.uncached_input, 100);
    }

    #[test]
    fn a_reported_zero_in_a_later_cumulative_event_is_taken() {
        let mut usage = TokenUsage {
            uncached_input: 100,
            cache_read: 900,
            text_output: 20,
            ..TokenUsage::default()
        };
        // The later event reports cache_read as zero and says so; it does not
        // mention uncached_input at all.
        usage.merge_cumulative_reported(
            &TokenUsage::default(),
            ReportedFields {
                cache_read: true,
                ..ReportedFields::default()
            },
        );
        assert_eq!(usage.cache_read, 0, "an explicit zero is taken");
        assert_eq!(usage.uncached_input, 100, "an unreported field survives");
        assert_eq!(usage.text_output, 20);
        // The plain merge still reads a zero as not reported.
        usage.merge_cumulative(&TokenUsage::default());
        assert_eq!(usage.uncached_input, 100);
    }

    #[test]
    fn raw_blob_is_preserved_verbatim() {
        let raw = json!({"input_tokens": 100, "some_field_added_next_quarter": 7});
        let usage = TokenUsage::normalize(Protocol::Additive, 100, 0, 0, 10, 0, Some(raw.clone()))
            .expect("normalize");
        assert_eq!(usage.raw.as_ref(), Some(&raw));
    }
}
