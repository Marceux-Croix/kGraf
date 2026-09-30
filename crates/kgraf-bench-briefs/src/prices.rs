//! Where the bench's price table comes from.
//!
//! `kgraf orchestrate` and `kgraf worker` currently require an explicit
//! `--prices <path>` JSON file (`kgraf_host::config::read_prices`); there is
//! no compiled-in default anywhere in the tree today (`kgraf spend` says so
//! outright: "no price list is compiled in"). But kGraf already vendors one:
//! `third_party/litellm/model_prices_and_context_window.json`, loaded by
//! `kgraf_core::litellm::LiteLlmCatalog` and turned into exactly the
//! `PriceTable` type every price cap and cost readout in the project already
//! consumes. This bench defaults to that vendored catalog rather than
//! demanding its own separate price file: a bench run over the routes
//! `kgraf providers` already knows about should not also need the caller to
//! hand-maintain a price list for them. `--prices` (`read_override`) stays
//! available to *override or add* entries the vendored table gets wrong or
//! does not have (a self-hosted model, a very recent release).

use anyhow::{Context, Result};
use kgraf_core::cost::PriceTable;
use kgraf_core::litellm::LiteLlmCatalog;
use std::path::Path;

/// The vendored litellm table's bytes, at the commit `third_party/litellm/UPSTREAM`
/// pins. The same file `kgraf_core::litellm`'s own tests load.
const VENDORED: &[u8] =
    include_bytes!("../../../third_party/litellm/model_prices_and_context_window.json");

/// kGraf's own vendored price table (see the module docs). Fails only if the
/// vendored file itself does not parse, a build-breaking problem upstream,
/// not a per-model gap (an unpriced model is reported by
/// [`kgraf_core::litellm::LiteLlmCatalog::unmapped`], which never applies
/// here since this bench only ever looks a model up by name).
pub fn default_price_table() -> Result<PriceTable> {
    default_price_table_for(&[])
}

/// [`default_price_table`], plus a bare-id price for each `(model, litellm
/// provider)` pair the vendored table only keys under that provider's prefix
/// (`kimi-k3` on the platform route, keyed `moonshot/kimi-k3`). Only the
/// route's own provider is tried; see
/// [`kgraf_core::litellm::LiteLlmCatalog::to_price_table_for_providers`].
pub fn default_price_table_for(wanted: &[(&str, &str)]) -> Result<PriceTable> {
    default_price_table_for_routes(wanted, &[])
}

/// A model served on a route that is priced only under its route-scoped key: an
/// Azure deployment and the model the account owner mapped it to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedPrice {
    /// The catalog route id.
    pub route: String,
    /// The name a request carries (the deployment name).
    pub model: String,
    /// The vendored table's key for the model behind it (`azure/<model>`).
    pub litellm_key: String,
}

/// [`default_price_table_for`], plus a route-scoped price (`<model>@<route>`) for
/// each [`ScopedPrice`] whose `litellm_key` the vendored table has.
///
/// A route in [`kgraf_fleet::route_scoped_price_routes`] is never priced by a bare
/// model entry: a deployment named like a catalog model can serve another one. A
/// scoped price the table lacks leaves that deployment unpriced, never guessed.
/// The vendored `azure/<model>` entry is the standard deployment rate: the
/// deployment type (data-zone, regional, provisioned) is not known here, so a
/// user entry in `--prices` is the way to price one of those.
pub fn default_price_table_for_routes(
    wanted: &[(&str, &str)],
    scoped: &[ScopedPrice],
) -> Result<PriceTable> {
    let catalog = LiteLlmCatalog::load(VENDORED)
        .context("kGraf's vendored litellm price table does not parse")?;
    let mut table = catalog.to_price_table_for_providers(wanted);
    for route in kgraf_fleet::route_scoped_price_routes() {
        table.require_route_scoped(route);
    }
    for entry in scoped {
        let Some(info) = catalog.get(&entry.litellm_key) else {
            continue;
        };
        let key = PriceTable::route_key(&entry.model, &entry.route);
        table.set(key.clone(), info.price);
        if let Some(threshold) = info.tier_threshold {
            table.set_tier_threshold(key, threshold);
        }
    }
    Ok(table)
}

/// Read a JSON price table at `path` (the same shape `kgraf orchestrate
/// --prices` reads) and set every entry in it onto `base`, replacing that
/// model's vendored price where the two overlap. An override is additive: a
/// model the vendored table already prices and the override does not mention
/// keeps its vendored price.
pub fn apply_override(base: &mut PriceTable, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the price override {}", path.display()))?;
    let over: PriceTable = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a valid price table", path.display()))?;
    for model in over.models() {
        let price = *over
            .get(model)
            .expect("model came from over.models(), so over.get(model) must return it");
        base.set(model, price);
    }
    Ok(())
}

/// How many catalog keys an unpriced-model warning names.
const CLOSEST_KEYS: usize = 3;

/// The catalog keys closest to `model`, closest first, at most `limit`. A
/// key is compared whole and by its last `/` segment (litellm keys often
/// carry a provider prefix, `openrouter/...`), case-insensitively, by
/// normalized Damerau-Levenshtein similarity. A key whose last segment is
/// the model id cut at a `-`, `.`, `:` or `@` (a dated or tagged release of
/// it, `gpt-4o-2099` for `gpt-4o`) ranks above every key that is not. Ties
/// go to the shorter key, then alphabetical order, so the list is stable.
pub fn closest_keys<'a>(prices: &'a PriceTable, model: &str, limit: usize) -> Vec<&'a str> {
    let wanted = model.to_lowercase();
    let score = |key: &str| {
        let key = key.to_lowercase();
        let tail = key.rsplit('/').next().unwrap_or(&key);
        let release_of = wanted
            .strip_prefix(tail)
            .is_some_and(|rest| rest.starts_with(['-', '.', ':', '@']));
        let similarity = strsim::normalized_damerau_levenshtein(&wanted, &key)
            .max(strsim::normalized_damerau_levenshtein(&wanted, tail));
        similarity + if release_of { 1.0 } else { 0.0 }
    };
    let mut scored: Vec<(f64, &str)> = prices.models().map(|k| (score(k), k)).collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(b.1))
    });
    scored.into_iter().take(limit).map(|(_, k)| k).collect()
}

/// A warning for a model the table does not price, naming the closest
/// catalog keys, or `None` when the model is priced. The lookup is exact, so
/// a near miss (`gpt-4o-2024` for `gpt-4o`, a missing provider prefix) runs
/// unpriced; the warning says which key to pass in a `--prices` override. It
/// never picks a price: an unpriced model stays unpriced, never `$0`.
pub fn unpriced_warning(prices: &PriceTable, model: &str) -> Option<String> {
    if prices.get(model).is_some() {
        return None;
    }
    let near = closest_keys(prices, model, CLOSEST_KEYS);
    let near = if near.is_empty() {
        "the price table is empty".to_owned()
    } else {
        format!(
            "closest catalog keys: {}",
            near.iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Some(format!(
        "warning: model `{model}` has no price entry, so its cost is reported as unpriced ({near}). \
         If one of them is the same model, price `{model}` with --prices."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::cost::ModelPrice;

    fn price(n: i64) -> ModelPrice {
        ModelPrice {
            uncached_input: n,
            cache_read: n,
            cache_write: n,
            output: n,
            reasoning: n,
        }
    }

    #[test]
    fn a_priced_model_gets_no_warning() {
        let table = default_price_table().expect("vendored table");
        assert_eq!(unpriced_warning(&table, "gpt-4o"), None);
    }

    #[test]
    fn an_unpriced_near_miss_names_the_closest_keys_and_stays_unpriced() {
        let mut table = PriceTable::new();
        table.set("gpt-4o", price(1));
        // Closer by edit distance to `gpt-4o-2024` than `gpt-4o` is.
        table.set("gpt-4o-mini", price(1));
        table.set("openrouter/anthropic/claude-haiku-4-5", price(2));
        table.set("mistral-large", price(3));
        table.set("text-embedding-3-small", price(4));

        assert_eq!(
            closest_keys(&table, "claude-haiku-4.5", 1),
            ["openrouter/anthropic/claude-haiku-4-5"]
        );
        assert_eq!(closest_keys(&table, "GPT-4O-2024", 1), ["gpt-4o"]);
        assert_eq!(closest_keys(&table, "gpt-4o-2024", 10).len(), 5);

        let warning = unpriced_warning(&table, "gpt-4o-2024").expect("unpriced");
        assert!(warning.contains("`gpt-4o-2024`"), "{warning}");
        assert!(warning.contains("`gpt-4o`"), "{warning}");
        assert_eq!(
            warning.matches("`, `").count(),
            CLOSEST_KEYS - 1,
            "{warning}"
        );
        // Warning only: the lookup is still exact and the model unpriced.
        assert!(table.get("gpt-4o-2024").is_none());
    }

    #[test]
    fn an_empty_table_says_so() {
        let warning = unpriced_warning(&PriceTable::new(), "m").expect("unpriced");
        assert!(warning.contains("empty"), "{warning}");
    }

    fn scoped(route: &str, model: &str, underlying: &str) -> ScopedPrice {
        ScopedPrice {
            route: route.to_owned(),
            model: model.to_owned(),
            litellm_key: kgraf_fleet::azure_litellm_key(underlying),
        }
    }

    #[test]
    fn an_azure_deployment_named_like_a_catalog_model_is_not_priced_by_the_bare_key() {
        // `gpt-4o` is a bare catalog key; a deployment called `gpt-4o` may serve
        // anything.
        let table = default_price_table_for_routes(&[], &[]).expect("table");
        assert!(table.get("gpt-4o").is_some());
        assert!(!table.is_priced_on(Some("azure-openai"), "gpt-4o"));
        assert!(!table.is_priced_on(Some("azure-openai-responses"), "gpt-4o"));
        assert!(table.is_priced_on(Some("openai"), "gpt-4o"));
    }

    #[test]
    fn an_azure_deployment_is_priced_through_its_mapped_model_under_a_scoped_key() {
        let underlying = kgraf_fleet::azure_litellm_key("gpt-4o-2024-08-06");
        let catalog = LiteLlmCatalog::load(VENDORED).expect("catalog");
        let listed = catalog
            .get(&underlying)
            .expect("the vendored table lists it");
        let table = default_price_table_for_routes(
            &[],
            &[
                scoped("azure-openai", "gpt-4o", "gpt-4o-2024-08-06"),
                // No such model: stays unpriced, never priced by a near key.
                scoped("azure-openai", "mystery", "no-such-model-2099"),
            ],
        )
        .expect("table");
        assert!(table.is_priced_on(Some("azure-openai"), "gpt-4o"));
        assert!(!table.is_priced_on(Some("azure-openai"), "mystery"));
        let usage = kgraf_core::TokenUsage {
            uncached_input: 1_000_000,
            ..kgraf_core::TokenUsage::default()
        };
        let cost = table
            .cost_on(Some("azure-openai"), "gpt-4o", &usage)
            .expect("priced");
        assert_eq!(
            cost.0, listed.price.uncached_input,
            "the mapped model's rate"
        );
        // The other Azure route is a different key and stays unpriced.
        assert!(!table.is_priced_on(Some("azure-openai-responses"), "gpt-4o"));
    }

    #[test]
    fn a_route_with_a_known_provider_prices_a_prefix_only_model() {
        assert!(
            default_price_table()
                .expect("table")
                .get("kimi-k3")
                .is_none()
        );
        let table = default_price_table_for(&[("kimi-k3", "moonshot")]).expect("table");
        assert!(table.get("kimi-k3").is_some());
        assert!(table.get("gpt-4o").is_some());
    }

    #[test]
    fn the_vendored_table_loads_and_prices_a_known_model() {
        let table = default_price_table().expect("vendored table");
        assert!(table.get("gpt-4o").is_some());
    }

    #[test]
    fn a_flat_override_of_a_tiered_model_is_not_refused_above_the_tier() {
        let mut base = default_price_table().expect("vendored table");
        let huge = kgraf_core::TokenUsage {
            uncached_input: 100_000_000,
            ..kgraf_core::TokenUsage::default()
        };
        let tiered = base
            .models()
            .find(|m| base.cost(m, &huge).is_err())
            .expect("the vendored table has a tiered model")
            .to_owned();
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("prices.json");
        std::fs::write(
            &path,
            serde_json::json!({
                tiered.clone(): {
                    "uncached_input": 1, "cache_read": 1, "cache_write": 1,
                    "output": 1, "reasoning": 1
                }
            })
            .to_string(),
        )
        .expect("write");
        apply_override(&mut base, &path).expect("override");
        assert!(base.cost(&tiered, &huge).is_ok());
    }

    #[test]
    fn an_override_replaces_a_vendored_price_and_adds_an_unlisted_model() {
        let mut base = default_price_table().expect("vendored table");
        let vendored_gpt4o = *base.get("gpt-4o").expect("gpt-4o priced");
        let vendored_count = base.models().count();
        // Some other vendored model, untouched by the override below.
        let other_model = base
            .models()
            .find(|m| *m != "gpt-4o")
            .expect("more than one vendored model")
            .to_owned();
        let other_price = *base.get(&other_model).expect("priced");

        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("prices.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "gpt-4o": {
                    "uncached_input": 1, "cache_read": 1, "cache_write": 1,
                    "output": 1, "reasoning": 1
                },
                "my-local-model": {
                    "uncached_input": 2, "cache_read": 2, "cache_write": 2,
                    "output": 2, "reasoning": 2
                }
            })
            .to_string(),
        )
        .expect("write");

        apply_override(&mut base, &path).expect("override");
        assert_ne!(base.get("gpt-4o").copied(), Some(vendored_gpt4o));
        assert_eq!(
            base.get("gpt-4o").copied(),
            Some(ModelPrice {
                uncached_input: 1,
                cache_read: 1,
                cache_write: 1,
                output: 1,
                reasoning: 1,
            })
        );
        assert!(base.get("my-local-model").is_some());
        assert_eq!(base.models().count(), vendored_count + 1);
        // A model the override never mentions keeps its vendored price.
        assert_eq!(base.get(&other_model).copied(), Some(other_price));
    }
}
