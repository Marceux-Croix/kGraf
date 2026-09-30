//! A loader for the vendored litellm price and context-window table.
//!
//! The vendored file is `third_party/litellm/model_prices_and_context_window.json`
//! (see `third_party/litellm/UPSTREAM` for provenance). It is not kGraf's
//! shape: it mixes five model classes (chat, embedding, image generation, and
//! more) in one flat JSON object, uses dollars-per-token rather than the
//! micro-dollars-per-million-tokens [`ModelPrice`] stores, and several entries
//! are litellm's own schema documentation rather than a priced model at all
//! (the `sample_spec` key holds prose strings in numeric-looking fields).
//!
//! This module is the single place that reads that shape. Everything else in
//! kGraf sees only [`ModelInfo`] and [`PriceTable`].
//!
//! Fail loud: an entry this loader cannot map to a priced, context-windowed
//! model is reported in [`LiteLlmCatalog::unmapped`] with a reason, never
//! silently dropped or priced at zero.

use crate::cost::ModelPrice;
use crate::cost::PriceTable;
use serde_json::Value;
use std::collections::BTreeMap;

/// litellm's `mode` field, for the model classes this loader knows how to
/// price from [`crate::usage::TokenUsage`]'s six-field form.
///
/// Every other mode (`embedding`, `image_generation`, `audio_speech`, ...) is
/// priced per image, per second, or per character, not per input/output
/// token, so it has no meaningful [`ModelPrice`] and is reported as
/// [`UnmappedReason::NotTokenPriced`] rather than forced into this shape.
const TOKEN_PRICED_MODES: [&str; 3] = ["chat", "completion", "responses"];

/// A dollar-per-token rate, as litellm stores it, converted to the
/// micro-dollars-per-million-tokens [`ModelPrice`] uses.
///
/// litellm: dollars per single token (e.g. `2.5e-6` for gpt-4o input).
/// kGraf: micro-dollars per million tokens (e.g. `2_500_000` for the same
/// rate). Both scale the same underlying number by `1e6`, once for the
/// per-token to per-million-token change of unit and once for dollars to
/// micro-dollars, so the conversion factor is `1e6 * 1e6 = 1e12`.
fn micros_per_million(dollars_per_token: f64) -> Option<i64> {
    if !dollars_per_token.is_finite() || dollars_per_token < 0.0 {
        return None;
    }
    let micros = dollars_per_token * 1_000_000_000_000.0;
    // Real vendor rates land far inside i64's range; a value that does not is
    // a parsing error upstream (or a unit mistake), not a price to trust.
    if micros > i64::MAX as f64 {
        return None;
    }
    Some(micros.round() as i64)
}

/// One model's price and context window, as loaded from the vendored table.
///
/// [`ModelPrice`] alone cannot carry a context window: it is, and stays, only
/// the five per-token rates [`PriceTable::cost`] multiplies against usage.
/// Adding a context-window field there would ripple into every existing
/// caller and literal that builds a `ModelPrice` (cost.rs's own tests
/// included) for a concern that has nothing to do with pricing arithmetic.
/// Pairing the two here, in the loader that is the only place both are known
/// at once, keeps that change confined to this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The five per-token rates, in micro-dollars per million tokens.
    pub price: ModelPrice,
    /// The model's context window, in tokens. Decision 0057's effective
    /// window is the lesser of this and a run-level cap.
    pub context_window: u64,
    /// The lowest input size above which the entry lists different rates (its
    /// `*_above_<N>k_tokens` fields). The five rates above are the base rates
    /// only, so a call larger than this is not costed from them.
    pub tier_threshold: Option<u64>,
}

/// Why one vendored entry could not become a [`ModelInfo`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnmappedReason {
    /// litellm's `mode` for this entry is not one of [`TOKEN_PRICED_MODES`],
    /// so it is not priced per input/output token at all (an image, audio, or
    /// embedding model, litellm's own `sample_spec` documentation entry, or
    /// an entry with no `mode`).
    NotTokenPriced {
        /// The raw `mode` value, if the entry had a string one.
        mode: Option<String>,
    },
    /// The entry claims a token-priced mode but is missing, or has a
    /// non-numeric, `input_cost_per_token` or `output_cost_per_token`. Both
    /// are load-bearing: [`crate::cost::PriceTable::cost`] has no lesser
    /// substitute for either.
    MissingInputOrOutputCost,
    /// A cost field parsed but was negative, non-finite, or otherwise not a
    /// rate this loader trusts.
    InvalidCost,
    /// `max_input_tokens` is absent and `max_output_tokens` is present, so
    /// litellm's own documented convention for `max_tokens` (mirror
    /// `max_output_tokens` when the provider specifies one) means the legacy
    /// `max_tokens` field here is the output cap, not the context window.
    MaxTokensIsOutputCap,
    /// Neither `max_input_tokens` nor a legacy `max_tokens` known not to be
    /// mirroring `max_output_tokens` is a usable number, so no context window
    /// can be reported for this model.
    MissingContextWindow,
}

/// One vendored entry the loader could not map, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmappedEntry {
    /// The vendored table's key for this entry (litellm's model id).
    pub model: String,
    pub reason: UnmappedReason,
}

/// Failures loading the vendored file itself, as opposed to one entry in it.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The file is not valid JSON.
    #[error("litellm price table is not valid JSON")]
    Json(#[from] serde_json::Error),
    /// The file parsed but its top level is not a JSON object of model
    /// entries, which is the only shape litellm has ever published it in.
    #[error("litellm price table's top level is not a JSON object")]
    NotAnObject,
}

/// The vendored litellm table, loaded and split into what could and could not
/// be mapped to kGraf's cost model.
#[derive(Clone, Debug)]
pub struct LiteLlmCatalog {
    /// The blake3 hash of the exact file bytes this catalog was loaded from.
    /// The version identity for this price table: decision 0055 versions the
    /// table by content hash rather than a litellm release tag, because
    /// litellm does not tag this file on its own release cadence.
    pub content_hash: blake3::Hash,
    models: BTreeMap<String, ModelInfo>,
    /// Every entry the loader saw but could not map, with a reason. Reported
    /// rather than dropped (fail loud).
    pub unmapped: Vec<UnmappedEntry>,
}

impl LiteLlmCatalog {
    /// Parse the vendored file's bytes into a catalog.
    ///
    /// Fails only if the file itself is not the JSON object litellm has
    /// always published; a model entry within it that cannot be priced is
    /// not a load failure, it is an [`UnmappedEntry`].
    pub fn load(bytes: &[u8]) -> Result<Self, LoadError> {
        let content_hash = blake3::hash(bytes);
        let root: Value = serde_json::from_slice(bytes)?;
        let Value::Object(entries) = root else {
            return Err(LoadError::NotAnObject);
        };

        let mut models = BTreeMap::new();
        let mut unmapped = Vec::new();

        for (model, entry) in entries {
            match map_entry(&entry) {
                Ok(info) => {
                    models.insert(model, info);
                }
                Err(reason) => unmapped.push(UnmappedEntry { model, reason }),
            }
        }

        Ok(Self {
            content_hash,
            models,
            unmapped,
        })
    }

    /// The context window and price info for one model, if it was mapped.
    pub fn get(&self, model: &str) -> Option<&ModelInfo> {
        self.models.get(model)
    }

    /// Every mapped model's id.
    pub fn models(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(String::as_str)
    }

    /// [`Self::to_price_table`], plus a bare-id entry for each `(model,
    /// provider)` pair whose bare `model` is not itself a catalog key but
    /// whose `provider/model` key is.
    ///
    /// litellm keys some vendors' models only under a provider prefix
    /// (`moonshot/kimi-k3`, no bare `kimi-k3`), so a route that names the bare
    /// id would run unpriced. The route's own provider is the only prefix
    /// tried: a bare id is never matched to another provider's entry of the
    /// same name, since two vendors can sell one model id at different rates.
    /// An exact bare key always wins over a prefixed one.
    pub fn to_price_table_for_providers(&self, wanted: &[(&str, &str)]) -> PriceTable {
        let mut table = self.to_price_table();
        for (model, provider) in wanted {
            if self.models.contains_key(*model) {
                continue;
            }
            if let Some(info) = self.models.get(&format!("{provider}/{model}")) {
                table.set((*model).to_owned(), info.price);
                if let Some(t) = info.tier_threshold {
                    table.set_tier_threshold((*model).to_owned(), t);
                }
            }
        }
        table
    }

    /// Build a [`PriceTable`] from every model this catalog mapped.
    ///
    /// This is the loader's output in kGraf's existing price-table type,
    /// as `crates/kgraf-provider/src/accounting.rs` already consumes it.
    /// The context window has no home in `PriceTable` and is dropped here;
    /// callers that need it read [`ModelInfo::context_window`] via
    /// [`Self::get`].
    pub fn to_price_table(&self) -> PriceTable {
        let mut table = PriceTable::new();
        for (model, info) in &self.models {
            table.set(model.clone(), info.price);
            if let Some(t) = info.tier_threshold {
                table.set_tier_threshold(model.clone(), t);
            }
        }
        table
    }
}

/// Extract a finite `f64` from a JSON field, distinguishing "absent" from
/// "present but not a number the vendored file would ever legitimately use".
fn number(entry: &Value, field: &str) -> Option<f64> {
    match entry.get(field) {
        None | Some(Value::Null) => None,
        Some(v) => v.as_f64(),
    }
}

fn map_entry(entry: &Value) -> Result<ModelInfo, UnmappedReason> {
    let mode = entry.get("mode").and_then(Value::as_str);
    if !mode.is_some_and(|m| TOKEN_PRICED_MODES.contains(&m)) {
        return Err(UnmappedReason::NotTokenPriced {
            mode: mode.map(str::to_owned),
        });
    }

    let input =
        number(entry, "input_cost_per_token").ok_or(UnmappedReason::MissingInputOrOutputCost)?;
    let output =
        number(entry, "output_cost_per_token").ok_or(UnmappedReason::MissingInputOrOutputCost)?;
    // A missing cache-read or cache-creation rate resolves to the input rate,
    // not zero: litellm's own cost calculator
    // (litellm_core_utils/llm_cost_calc/utils.py, `_get_token_base_cost`)
    // states plainly that "a provider that publishes no cache price bills
    // cached tokens as ordinary input," and resolves the missing rate to the
    // prompt (input) rate rather than zero. 1,927 of the 3,478 chat-shaped
    // vendored entries omit `cache_read_input_token_cost`, so treating the
    // absence as zero would have undercosted every one of them. An explicit
    // `0.0` in the file, by contrast, is still a real free-cache price.
    // `output_cost_per_reasoning_token` has no such upstream default; a
    // missing reasoning rate here still resolves to the output rate, which
    // matches cost.rs's own convention that every surveyed protocol bills
    // reasoning at the output rate.
    let cache_read = number(entry, "cache_read_input_token_cost").unwrap_or(input);
    let cache_write = number(entry, "cache_creation_input_token_cost").unwrap_or(input);
    let reasoning = number(entry, "output_cost_per_reasoning_token").unwrap_or(output);

    let price = ModelPrice {
        uncached_input: micros_per_million(input).ok_or(UnmappedReason::InvalidCost)?,
        cache_read: micros_per_million(cache_read).ok_or(UnmappedReason::InvalidCost)?,
        cache_write: micros_per_million(cache_write).ok_or(UnmappedReason::InvalidCost)?,
        output: micros_per_million(output).ok_or(UnmappedReason::InvalidCost)?,
        reasoning: micros_per_million(reasoning).ok_or(UnmappedReason::InvalidCost)?,
    };

    // `max_input_tokens` is litellm's current, unambiguous field. `max_tokens`
    // is a legacy field litellm's own `sample_spec` entry documents as
    // mirroring `max_output_tokens` when the provider specifies one, and only
    // falling back to `max_input_tokens` otherwise. So a bare `max_tokens` is
    // trustworthy as the context window only when `max_output_tokens` is
    // absent; when both are present, `max_tokens` is the *output* cap
    // wearing the context-window field's name, and reporting it as the
    // context window would silently understate every model that hits this
    // case (three in the vendored file: both `gemini-gemma-2` sizes and
    // `vertex_ai/gemini-omni-1.1-flash-preview`).
    let context_window = match entry.get("max_input_tokens").and_then(Value::as_u64) {
        Some(window) => window,
        None => {
            if entry
                .get("max_output_tokens")
                .and_then(Value::as_u64)
                .is_some()
            {
                return Err(UnmappedReason::MaxTokensIsOutputCap);
            }
            entry
                .get("max_tokens")
                .and_then(Value::as_u64)
                .ok_or(UnmappedReason::MissingContextWindow)?
        }
    };

    Ok(ModelInfo {
        price,
        context_window,
        tier_threshold: tier_threshold(entry),
    })
}

/// The lowest `<N>k` in the entry's `*_above_<N>k_tokens` rate fields. Fields
/// for another service tier (`..._priority`, `..._batches`, `..._flex`) and the
/// one-hour cache-write rate do not end in `_tokens` after the size, so they do
/// not match.
fn tier_threshold(entry: &Value) -> Option<u64> {
    entry
        .as_object()?
        .keys()
        .filter(|key| key.contains("_cost_per_token_above_") || key.contains("_token_cost_above_"))
        .filter_map(|key| {
            let size = key.rsplit("_above_").next()?.strip_suffix("k_tokens")?;
            size.parse::<u64>().ok().map(|k| k * 1_000)
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real vendored file, not a fixture: a loader tested
    /// against a hand-built stand-in proves nothing about whether it parses
    /// what litellm actually ships.
    fn vendored_bytes() -> &'static [u8] {
        include_bytes!("../../../third_party/litellm/model_prices_and_context_window.json")
    }

    #[test]
    fn loads_the_vendored_file_without_erroring() {
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        assert!(
            catalog.models().count() > 1000,
            "expected thousands of chat-priced models"
        );
    }

    #[test]
    fn a_tiered_entry_carries_its_lowest_threshold_and_other_tiers_are_ignored() {
        let entry = serde_json::json!({
            "input_cost_per_token_above_272k_tokens": 1e-5,
            "input_cost_per_token_above_200k_tokens": 6e-6,
            "cache_read_input_token_cost_above_200k_tokens": 6e-7,
            "input_cost_per_token_above_200k_tokens_priority": 1e-5,
            "cache_creation_input_token_cost_above_1hr": 1e-5,
        });
        assert_eq!(tier_threshold(&entry), Some(200_000));
        assert_eq!(
            tier_threshold(&serde_json::json!({"input_cost_per_token": 1e-6})),
            None
        );
        // The vendored file has tiered entries, and the price table refuses a
        // call above the tier instead of costing it at the base rate.
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file");
        let table = catalog.to_price_table();
        let tiered = catalog
            .models
            .iter()
            .find(|(_, info)| info.tier_threshold.is_some())
            .expect("a tiered model");
        let threshold = tiered.1.tier_threshold.expect("threshold");
        let over = crate::usage::TokenUsage {
            uncached_input: threshold + 1,
            ..crate::usage::TokenUsage::default()
        };
        assert!(table.cost(tiered.0, &over).is_err());
    }

    #[test]
    fn content_hash_matches_a_direct_blake3_hash_of_the_bytes() {
        let bytes = vendored_bytes();
        let catalog = LiteLlmCatalog::load(bytes).expect("vendored file must parse");
        assert_eq!(catalog.content_hash, blake3::hash(bytes));
    }

    #[test]
    fn gpt_4o_prices_and_context_window_match_the_vendored_numbers() {
        // gpt-4o's vendored entry, at the commit third_party/litellm/UPSTREAM
        // pins: input $2.50/M, cached input $1.25/M, output $10/M, 128k
        // context. It has no `cache_creation_input_token_cost` field, so the
        // cache-write rate must resolve to the input rate ($2.50/M), per
        // litellm's own cost calculator (see `map_entry`'s cache-rate
        // comment), not zero.
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        let info = catalog.get("gpt-4o").expect("gpt-4o must be mapped");
        assert_eq!(info.price.uncached_input, 2_500_000);
        assert_eq!(info.price.cache_read, 1_250_000);
        assert_eq!(info.price.cache_write, 2_500_000);
        assert_eq!(info.price.output, 10_000_000);
        assert_eq!(info.price.reasoning, 10_000_000);
        assert_eq!(info.context_window, 128_000);
    }

    #[test]
    fn a_provider_prefixed_key_prices_the_bare_id_for_that_provider_only() {
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        // The vendored table keys this model only as `moonshot/kimi-k3`.
        assert!(catalog.get("kimi-k3").is_none());
        let plain = catalog.to_price_table();
        assert!(plain.get("kimi-k3").is_none());

        let with = catalog.to_price_table_for_providers(&[("kimi-k3", "moonshot")]);
        let expected = catalog.get("moonshot/kimi-k3").expect("prefixed key").price;
        assert_eq!(with.get("kimi-k3").copied(), Some(expected));

        // Another provider's prefix never prices it: the lookup does not guess
        // across providers.
        let wrong = catalog.to_price_table_for_providers(&[("kimi-k3", "databricks")]);
        assert!(wrong.get("kimi-k3").is_none());
        let unknown = catalog.to_price_table_for_providers(&[("kimi-k3", "nobody")]);
        assert!(unknown.get("kimi-k3").is_none());

        // An exact bare key is kept as is.
        let exact = catalog.to_price_table_for_providers(&[("gpt-4o", "moonshot")]);
        assert_eq!(exact.get("gpt-4o"), plain.get("gpt-4o"));
    }

    #[test]
    fn a_model_with_no_cache_price_at_all_bills_cache_at_the_input_rate() {
        // gpt-3.5-turbo's vendored entry has neither
        // `cache_read_input_token_cost` nor `cache_creation_input_token_cost`
        // at all. litellm's `_get_token_base_cost` resolves both to the input
        // rate in that case ("a provider that publishes no cache price bills
        // cached tokens as ordinary input"), not to zero: input $0.50/M.
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        let info = catalog
            .get("gpt-3.5-turbo")
            .expect("gpt-3.5-turbo must be mapped");
        assert_eq!(info.price.uncached_input, 500_000);
        assert_eq!(info.price.cache_read, info.price.uncached_input);
        assert_eq!(info.price.cache_write, info.price.uncached_input);
    }

    #[test]
    fn max_tokens_mirroring_max_output_tokens_is_reported_not_mapped() {
        // gemini/gemini-gemma-2-27b-it has no `max_input_tokens`, and its
        // `max_tokens` (8192) equals its `max_output_tokens` (8192): litellm's
        // own `sample_spec` documents `max_tokens` as mirroring
        // `max_output_tokens` whenever the provider specifies one. Treating
        // that 8192 as the context window would silently report an output
        // cap as an input capacity, so it must be reported unmapped instead.
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        assert!(catalog.get("gemini/gemini-gemma-2-27b-it").is_none());
        let entry = catalog
            .unmapped
            .iter()
            .find(|u| u.model == "gemini/gemini-gemma-2-27b-it")
            .expect("gemini-gemma-2-27b-it must appear in the unmapped report");
        assert_eq!(entry.reason, UnmappedReason::MaxTokensIsOutputCap);
    }

    #[test]
    fn bedrock_claude_prices_cache_read_and_write_separately() {
        // Bedrock's Claude 3.5 Sonnet v2 entry carries litellm's clearest
        // case of every one of the four token-priced fields being distinct:
        // input $3/M, cache write $3.75/M, cache read $0.30/M, output $15/M.
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        let info = catalog
            .get("anthropic.claude-3-5-sonnet-20241022-v2:0")
            .expect("bedrock claude 3.5 sonnet v2 must be mapped");
        assert_eq!(info.price.uncached_input, 3_000_000);
        assert_eq!(info.price.cache_write, 3_750_000);
        assert_eq!(info.price.cache_read, 300_000);
        assert_eq!(info.price.output, 15_000_000);
        assert_eq!(info.context_window, 1_000_000);
    }

    #[test]
    fn sample_spec_documentation_entry_is_reported_unmapped_not_priced() {
        // litellm's `sample_spec` key holds prose strings in fields real
        // entries use for numbers (its own `mode` field literally reads "one
        // of: chat, embedding, ..."). It must never be mistaken for a real,
        // zero-priced model.
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        assert!(catalog.get("sample_spec").is_none());
        let entry = catalog
            .unmapped
            .iter()
            .find(|u| u.model == "sample_spec")
            .expect("sample_spec must appear in the unmapped report");
        assert!(matches!(
            entry.reason,
            UnmappedReason::NotTokenPriced { .. }
        ));
    }

    #[test]
    fn every_vendored_entry_is_either_mapped_or_reported_unmapped() {
        // The fail-loud property this whole loader exists to keep: nothing is
        // silently dropped. Every key in the raw JSON ends up in exactly one
        // of the two buckets.
        let bytes = vendored_bytes();
        let raw: Value = serde_json::from_slice(bytes).expect("vendored file is valid JSON");
        let total = raw.as_object().expect("top level is an object").len();
        let catalog = LiteLlmCatalog::load(bytes).expect("vendored file must parse");
        assert_eq!(catalog.models().count() + catalog.unmapped.len(), total);
    }

    #[test]
    fn an_unmapped_entry_never_ends_up_in_the_price_table() {
        let catalog = LiteLlmCatalog::load(vendored_bytes()).expect("vendored file must parse");
        let table = catalog.to_price_table();
        for unmapped in &catalog.unmapped {
            assert!(
                table.get(&unmapped.model).is_none(),
                "unmapped model `{}` must not appear in the derived price table",
                unmapped.model
            );
        }
    }

    #[test]
    fn micros_per_million_matches_a_hand_computed_rate() {
        // $2.50 per million tokens is $0.0000025 per token.
        assert_eq!(micros_per_million(0.0000025), Some(2_500_000));
        assert_eq!(micros_per_million(0.0), Some(0));
        assert_eq!(micros_per_million(-1.0), None);
        assert_eq!(micros_per_million(f64::NAN), None);
    }
}
