//! `kgraf-bench-briefs`: run the fixed brief suite against one or more
//! provider routes and record the tallies into a `ProfileStore`.
//!
//! Building a provider goes through `kgraf-fleet`, exactly as `kgraf worker`
//! does (`kgraf-cli`'s `worker_cmd.rs`): `entry` -> `credential_from_env` ->
//! `build`. Nothing here talks to a provider directly.

use anyhow::{Context, Result, bail};
use clap::Parser;
use kgraf_bench_briefs::{
    BenchOptions, RouteHandle, ScopedPrice, WorkerBudget, apply_override, brief,
    default_price_table_for_routes, report, run_bench, suite_store, unpriced_warning,
};
use kgraf_core::cost::{Money, PriceTable};
use kgraf_host::worker::config::{
    DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_STEP_BUDGET,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Whether a catalog entry is local: no credential and a localhost base URL
/// (`kgraf-fleet`'s `ollama`/`llama-cpp` entries, or any route configured
/// the same way; the same test `kgraf-fleet`'s own
/// `local_entries_need_no_credential_and_build_without_one` checks). Affects
/// only an unpriced attempt's note (decision 0041: not special-cased to
/// free).
fn is_local(entry: &kgraf_fleet::Entry) -> bool {
    !entry.requires_credential && entry.base_url.starts_with("http://localhost")
}

/// `route:model`, e.g. `kimi:kimi-k2`. The route id is `kgraf-fleet`'s
/// catalog id (see `kgraf providers`).
#[derive(Clone, Debug)]
struct RouteSpec {
    route: String,
    model: String,
}

impl std::str::FromStr for RouteSpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (route, model) = s
            .split_once(':')
            .ok_or_else(|| format!("`{s}` is not route:model"))?;
        if route.is_empty() || model.is_empty() {
            return Err(format!("`{s}` is not route:model"));
        }
        Ok(Self {
            route: route.to_owned(),
            model: model.to_owned(),
        })
    }
}

/// A per-model brief-following bench: fixed small briefs, mechanically
/// checked, feeding kgraf-core's `ProfileStore` (decision 0075).
#[derive(Parser, Debug)]
struct Args {
    /// A route to bench, `route:model` (repeatable). The route id is
    /// `kgraf-fleet`'s catalog id.
    #[arg(long = "route", required = true)]
    routes: Vec<RouteSpec>,
    /// Only run briefs whose id contains this substring (repeatable; a
    /// brief matching any filter runs). With none given, every brief runs.
    #[arg(long = "brief")]
    brief_filter: Vec<String>,
    /// How many times to repeat each route/brief pair.
    #[arg(long, default_value_t = 1)]
    reps: u32,
    /// An optional price table (the same JSON shape `kgraf orchestrate
    /// --prices` reads) whose entries override kGraf's own vendored litellm
    /// price catalog, the default source: only needed for a model the
    /// vendored table does not have, or gets wrong.
    #[arg(long)]
    prices: Option<PathBuf>,
    /// Where the `ProfileStore` (and each attempt's throwaway ledger) is
    /// read from and written to.
    #[arg(long = "state-dir")]
    state_dir: PathBuf,
    /// List planned attempts and an estimated cost; make no provider call.
    #[arg(long)]
    dry_run: bool,
    /// Write every attempt's full record (including token counts and wall
    /// time, which the printed table and the `ProfileStore` tally do not
    /// carry) as a JSON array to this path. Written in both a dry run and a
    /// real one.
    #[arg(long = "json-out")]
    json_out: Option<PathBuf>,
    /// Model rounds per attempt, including the final answer round. The
    /// default is `kgraf worker`'s own.
    #[arg(long, default_value_t = DEFAULT_STEP_BUDGET)]
    budget: u32,
    /// Output ceiling per round. The default is `kgraf worker`'s own.
    #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_TOKENS)]
    max_tokens: u32,
    /// The model's context window in tokens. The default is `kgraf
    /// worker`'s own.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_WINDOW_TOKENS)]
    window: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // A route whose vendor litellm keys under a provider prefix gets its bare
    // model id priced from that prefix; a route with no known provider gets none.
    let provider_ids: Vec<(&str, &str)> = args
        .routes
        .iter()
        .filter_map(|spec| {
            kgraf_fleet::litellm_provider(&spec.route).map(|p| (spec.model.as_str(), p))
        })
        .collect();
    // An Azure deployment is priced through the model it is mapped to, under the
    // route-scoped key; a route with no mapping in the environment stays unpriced.
    let scoped: Vec<ScopedPrice> = args
        .routes
        .iter()
        .filter_map(|spec| {
            let entry = kgraf_fleet::entry(&spec.route).ok()?;
            let deployments = kgraf_fleet::azure_deployments(entry, kgraf_core::legacy::env_var)?;
            let (_, underlying) = deployments.into_iter().find(|(d, _)| *d == spec.model)?;
            Some(ScopedPrice {
                route: spec.route.clone(),
                model: spec.model.clone(),
                litellm_key: kgraf_fleet::azure_litellm_key(&underlying),
            })
        })
        .collect();
    let mut prices = default_price_table_for_routes(&provider_ids, &scoped)?;
    if let Some(path) = &args.prices {
        apply_override(&mut prices, path)?;
    }

    let mut briefs = brief::suite();
    if !args.brief_filter.is_empty() {
        briefs.retain(|b| args.brief_filter.iter().any(|f| b.id.contains(f.as_str())));
        if briefs.is_empty() {
            bail!("--brief filters matched no brief in the suite");
        }
    }

    let mut routes = Vec::new();
    for spec in &args.routes {
        let entry = kgraf_fleet::entry(&spec.route)
            .with_context(|| format!("unknown route `{}`", spec.route))?;
        let local = is_local(entry);
        let route = if args.dry_run {
            // A dry run never needs a credential or a live provider: the
            // catalog entry alone names the vendor.
            RouteHandle {
                route: spec.route.clone(),
                vendor: entry.vendor.to_owned(),
                model: spec.model.clone(),
                provider: Arc::new(NeverCalled),
                local,
            }
        } else {
            let credential = kgraf_fleet::credential_from_env(entry)?;
            let provider: Arc<dyn kgraf_provider::Provider> = Arc::from(kgraf_fleet::build(
                &spec.route,
                &credential,
                vec![spec.model.clone()],
            )?);
            RouteHandle {
                route: spec.route.clone(),
                vendor: entry.vendor.to_owned(),
                model: spec.model.clone(),
                provider,
                local,
            }
        };
        routes.push(route);
    }

    // Prices are looked up by exact model key; a near miss runs unpriced, and
    // this says so before any call, naming the keys it may have meant.
    for spec in &args.routes {
        if kgraf_fleet::route_scoped_price_only(&spec.route) {
            if !prices.is_priced_on(Some(&spec.route), &spec.model) {
                eprintln!(
                    "warning: deployment `{}` on `{}` has no price entry, so its cost is reported \
                     as unpriced. Azure deployments are priced only under `{}` (from the model \
                     the deployment is mapped to, or --prices).",
                    spec.model,
                    spec.route,
                    PriceTable::route_key(&spec.model, &spec.route)
                );
            } else {
                // The vendored `azure/<model>` entry is the standard deployment
                // rate. Data-zone, regional and provisioned deployments bill
                // differently, and a deployment's type is not in the mapping.
                eprintln!(
                    "note: deployment `{}` on `{}` is priced as a standard deployment of its \
                     mapped model. A data-zone, regional or provisioned deployment bills \
                     differently: set `{}` with --prices.",
                    spec.model,
                    spec.route,
                    PriceTable::route_key(&spec.model, &spec.route)
                );
            }
        } else if let Some(warning) = unpriced_warning(&prices, &spec.model) {
            eprintln!("{warning}");
        }
    }

    let opts = BenchOptions {
        reps: args.reps.max(1),
        dry_run: args.dry_run,
        budget: WorkerBudget {
            step_budget: args.budget,
            max_output_tokens: args.max_tokens,
            context_window_tokens: args.window,
        },
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let outcomes =
        runtime.block_on(run_bench(&routes, &briefs, &prices, &args.state_dir, &opts))?;
    runtime.shutdown_background();

    if let Some(path) = &args.json_out {
        let json = serde_json::to_string_pretty(&outcomes)
            .context("cannot serialize the attempt records")?;
        std::fs::write(path, json).with_context(|| format!("cannot write {}", path.display()))?;
    }

    if args.dry_run {
        println!(
            "dry run: {} planned attempt(s), no provider call made",
            outcomes.len()
        );
        for o in &outcomes {
            println!(
                "  {} {} rep {}: brief {} ({})  estimated {}",
                o.route,
                o.model,
                o.rep,
                o.brief_id,
                o.kind.as_str(),
                display_cost(o.cost),
            );
        }
        return Ok(());
    }

    for o in &outcomes {
        println!(
            "{} {} rep {}: {} ({})  {}  {}{}",
            o.route,
            o.model,
            o.rep,
            o.brief_id,
            o.kind.as_str(),
            if o.errored {
                "ERROR (not tallied)"
            } else if o.satisfied {
                "satisfied"
            } else {
                "unmet"
            },
            display_cost(o.cost),
            o.note
                .as_ref()
                .map_or_else(String::new, |n| format!("  note: {n}")),
        );
    }
    let hash = brief::suite_hash();
    let view = suite_store::inspect(&args.state_dir, &hash)?;
    println!(
        "\n{}",
        report::render_with_stale(&view.store, &hash, view.stale.as_deref())
    );
    Ok(())
}

/// `cost` as printed: a real amount when priced, "unpriced" when not (never
/// a fabricated `$0` -- see `runner`'s module docs).
fn display_cost(cost: Option<Money>) -> String {
    cost.map_or_else(|| "unpriced".to_owned(), |m| m.to_string())
}

/// The dry-run placeholder provider: [`run_bench`] never calls it (a dry run
/// returns before touching any [`RouteHandle::provider`]), so a call here is
/// a bug, not a benign no-op, it fails loud instead of silently
/// answering.
struct NeverCalled;

#[async_trait::async_trait]
impl kgraf_provider::Provider for NeverCalled {
    fn adapter(&self) -> &'static str {
        "dry-run-never-called"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        _request: &kgraf_provider::Request,
        _token: &kgraf_provider::CallToken,
    ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
        unreachable!("a dry run must never call a provider")
    }
}
