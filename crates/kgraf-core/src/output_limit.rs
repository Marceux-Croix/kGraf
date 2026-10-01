//! A model's supported output limit, from the vendored litellm table.
//!
//! The table (`third_party/litellm/model_prices_and_context_window.json`, see
//! its `UPSTREAM` file) is compiled into the binary and read once, on first
//! use, keeping only each entry's `max_output_tokens`. A run that asks for a
//! larger output cap than the model supports is clamped to this number, with a
//! notice (see `kgraf_host::limits`).
//!
//! A model the table does not list has no known limit: [`output_limit`]
//! returns `None` and the requested cap goes to the provider unchanged.

use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

const VENDORED: &[u8] =
    include_bytes!("../../../third_party/litellm/model_prices_and_context_window.json");

fn table() -> &'static BTreeMap<String, u64> {
    static TABLE: OnceLock<BTreeMap<String, u64>> = OnceLock::new();
    TABLE.get_or_init(|| {
        // The file is vendored and a test pins that it parses; an empty table
        // (no clamping) is the fallback only for a broken build.
        let Ok(Value::Object(entries)) = serde_json::from_slice::<Value>(VENDORED) else {
            return BTreeMap::new();
        };
        entries
            .into_iter()
            .filter_map(|(model, entry)| {
                let limit = entry.get("max_output_tokens")?.as_u64()?;
                (limit > 0).then_some((model, limit))
            })
            .collect()
    })
}

/// The output limit in tokens for `model` on `route`: the bare model id first,
/// then `route/model` (litellm keys some vendors only under a provider
/// prefix). A bare id is never matched to another provider's entry.
pub fn output_limit(route: &str, model: &str) -> Option<u64> {
    let t = table();
    t.get(model)
        .or_else(|| t.get(&format!("{route}/{model}")))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendored_table_gives_a_known_models_limit() {
        assert_eq!(output_limit("openai", "gpt-4o"), Some(16_384));
    }

    #[test]
    fn an_unlisted_model_has_no_limit() {
        assert_eq!(output_limit("fake", "no-such-model-xyz"), None);
    }

    #[test]
    fn the_table_is_not_empty() {
        assert!(table().len() > 500, "the vendored file parsed");
    }
}
