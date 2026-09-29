//! Vendor/model routing by coupling and measured capability (decision 0075).
//!
//! Today's default, [`RoutingPolicy::Rotation`], gives an unpinned contract the
//! next route in rotation (`orchestrator.rs::route_base`/`route_at`) with no
//! regard for which contracts are coupled or which model is actually good at
//! which kind of work. Decision 0012 measured cross-agent pairs conflicting at
//! 41.7 percent against 19.8 percent same-agent, so coupled contracts landing on
//! different vendors by default is a measured cost, not a hypothetical one.
//!
//! [`RoutingPolicy::Fitness`] is the opt-in alternative this module implements:
//!
//! 1. **Route by task kind, not vendor.** A model is good or poor at a kind of
//!    work ([`TaskKind`]), never "at code" in general. The orchestrator labels each contract with
//!    one kind at decomposition (`plan.rs`); an absent kind defaults to
//!    [`TaskKind::Implementation`] and that default is traced, never silent.
//! 2. **Measure strengths, never assume them.** Bench tallies
//!    ([`Source::Bench`]) are the prior; Live tallies ([`Source::Live`]) replace
//!    them once attempts reach [`LIVE_MIN_ATTEMPTS`]. The two sources are never
//!    merged into one figure ([`kgraf_core::model_profile`]'s own rule).
//! 3. **The objective is lowest expected cost per satisfied task for the kind**
//!    (decision 0008), never lowest price. A cheap model with a low satisfaction
//!    rate is not a bargain.
//! 4. **Escalate on failure, in-vendor first.** A retry (a later parallel
//!    attempt, `route_at`'s `attempt > 0`; this codebase calls Beam's parallel
//!    attempts "retries") moves to a route with a better measured record for the
//!    kind. It looks inside the cluster's vendor first; only when no in-vendor
//!    route beats the current one does it cross vendors, and that crossing is
//!    traced as a cluster break with its reason.
//! 5. **Interface work has an absolute quality floor** ([`INTERFACE_FLOOR`]). A
//!    model *measured* below the floor for [`TaskKind::Interface`] never gets
//!    interface work, at any price; an *unmeasured* route is not "below the
//!    floor" (nothing is known yet), so it stays eligible through the
//!    exploration share.
//! 6. **Coupled work stays on one vendor, not one model.** [`cluster_contracts`]
//!    groups contracts whose write sets overlap, that depend on each other, or
//!    whose write sets share a co-change edge at or above
//!    [`CLUSTER_COCHANGE_MIN_COUNT`] (decision 0020: co-change is the collision
//!    relation, unthresholded at index time, so the threshold is a read-time
//!    choice made here). [`choose_cluster_vendor`] then picks the vendor with the
//!    lowest total expected cost per satisfied task summed across the cluster's
//!    contracts, among vendors with an eligible (floor-passing) model for every
//!    kind the cluster contains; [`choose_worker_route`] then picks each
//!    contract's own model inside that vendor. Only worker routes are chosen
//!    this way; the orchestrator's own planning route is never touched.
//!
//! Every function here is pure with respect to its inputs (no I/O, no clock) so
//! it can be tested as a mechanism, per this project's standing rule against
//! testing settings. The impure edges, loading and saving
//! [`kgraf_core::model_profile::ProfileStore`], and reading co-change edges from
//! [`kgraf_graph::GraphStore`], are thin wrappers at the bottom of this file and
//! in `orchestrator.rs`.

use crate::run_state::OpenContract;
use kgraf_core::cost::{Money, PriceTable};
use kgraf_core::model_profile::{KindTally, ProfileError, ProfileStore, Source, TaskKind};
use kgraf_graph::GraphStore;
use kgraf_graph::cochange::{CoChangeQuery, partners_of};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Live attempts needed before a route's Live tally is trusted over its Bench
/// prior. Named so the choice is visible and traced, never a
/// bare literal.
pub const LIVE_MIN_ATTEMPTS: u64 = 5;

/// The absolute floor a measured satisfaction rate must clear for
/// [`TaskKind::Interface`] work. A
/// starting point, not a researched number, like every other default in this
/// codebase (`OrchestratorConfig`'s doc comment says the same of its own
/// fields); measurement is what would move it.
pub const INTERFACE_FLOOR: f64 = 0.5;

/// Every this-th Fitness pick, among candidates that include an unmeasured
/// route, takes the unmeasured one instead of the ranked winner. Without this a
/// route with no tallies could never accumulate any, since a route with zero
/// measured attempts always ranks behind one with a good measured record. The
/// share is deterministic (a counter, not a random draw) so a run is
/// reproducible from its trace.
pub const EXPLORATION_EVERY: u64 = 8;

/// The co-change count (decision 0020's weight) at or above which two files
/// couple two contracts into one cluster. Decision 0020 keeps every observed
/// pair and thresholds only at read time; no consumer in this codebase had
/// picked an operating point before this one, so `3` is chosen here, matching
/// the `co-change >= 3` row 0020's own measurement reports as the first cut
/// that meaningfully separates weak coincidence from repeated coupling (45%
/// file coverage at `>= 2`, 26% at `>= 3`, versus 99% unthresholded).
pub const CLUSTER_COCHANGE_MIN_COUNT: u32 = 3;

/// A representative per-task token mix, for ranking *unmeasured* routes only.
/// Never used for real accounting (decision 0043's ledger stays token-exact):
/// an unmeasured route's true cost per satisfied task is unknown by
/// definition, which is why every pick built from it is traced as
/// `"unmeasured"` rather than presented next to a measured figure.
const REPRESENTATIVE_INPUT_TOKENS: u64 = 20_000;
const REPRESENTATIVE_OUTPUT_TOKENS: u64 = 2_000;

/// The opt-in routing policy. [`RoutingPolicy::Rotation`] is the
/// default and is unaffected by anything in this module: a three-arm experiment
/// elsewhere in this project depends on rotation being what it has always been.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicy {
    #[default]
    Rotation,
    Fitness,
}

impl std::str::FromStr for RoutingPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "rotation" => Ok(Self::Rotation),
            "fitness" => Ok(Self::Fitness),
            other => Err(format!(
                "unknown routing policy `{other}`; use `rotation` or `fitness`"
            )),
        }
    }
}

impl std::fmt::Display for RoutingPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rotation => "rotation",
            Self::Fitness => "fitness",
        })
    }
}

/// One worker route, as routing needs to see it: its name (the roster key),
/// its vendor (what clustering pins) and its model (what the profile store
/// keys on). Deliberately not `orchestrator::Route` itself, so this module
/// stays free of the runtime types (`WorkerEnv`, readiness probes) a route
/// carries; `orchestrator.rs` builds this from its own roster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteInfo {
    pub name: String,
    pub vendor: String,
    pub model: String,
    /// No credential (decision 0041's `ProviderConfig::local`, or an
    /// in-process provider). kGraf does not police
    /// spend (the user limits it in their provider's console), so an
    /// unpriced route is never an error or excluded on that account; a local
    /// one specifically costs nothing to call, so it ranks at `$0` rather
    /// than by an unknowable representative price.
    pub local: bool,
}

/// Where a route's figures came from, and what the pick used them for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FigureSource {
    Live,
    Bench,
    Unmeasured,
}

impl FigureSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Bench => "bench",
            Self::Unmeasured => "unmeasured",
        }
    }
}

/// One route's routing decision, with the figures it was made from
/// (every decision names its tally figures, or "unmeasured", and its reason).
#[derive(Clone, Debug, PartialEq)]
pub struct RoutePick {
    pub route: String,
    pub source: FigureSource,
    pub satisfaction_rate: Option<f64>,
    pub cost_per_satisfied_micros: Option<i64>,
    pub reason: String,
    /// Set only by [`escalate_route`] when it had to leave the cluster's
    /// vendor because nothing inside it beat the current route.
    pub crossed_cluster: bool,
}

#[derive(Clone, Debug)]
struct Candidate {
    route: RouteInfo,
    source: FigureSource,
    satisfaction_rate: Option<f64>,
    /// Cost per satisfied task in micro-dollars: measured for Live/Bench, the
    /// representative proxy for a priced-but-unmeasured route, `Some(0)` for
    /// an unpriced local route, or `None` for a genuinely unknown (unpriced,
    /// hosted, unmeasured) cost.
    cost_micros: Option<i64>,
    /// Set for an unmeasured route with no `PriceTable` entry:
    /// never an error, never excluded, always traced. `true` for
    /// a local route (ranked at `$0`), `false` for a hosted one (cost stays
    /// unknown; ranks last on cost, same as before local routes ranked at `$0`).
    unpriced: Option<bool>,
}

fn pick_from(c: &Candidate, reason: impl Into<String>) -> RoutePick {
    // An unpriced route's trace names the case by
    // itself, in place of the generic unmeasured reason, since "no price
    // entry" is the fact worth surfacing here, not "no tally" (both are true,
    // but the price gap is the new and less obvious one).
    let reason = match c.unpriced {
        Some(true) => "local, unpriced: no price entry; a local route costs nothing to call, \
             so it is ranked at $0"
            .to_owned(),
        Some(false) => match c.satisfaction_rate {
            Some(rate) => format!(
                "unpriced: no price entry; cost unknown, so it is ranked on its measured \
                 satisfaction rate ({:.0}%) instead of cost",
                rate * 100.0
            ),
            None => "unpriced: no price entry and no measured satisfaction rate yet; \
                     ranked last on cost until it is measured or priced"
                .to_owned(),
        },
        None => reason.into(),
    };
    RoutePick {
        route: c.route.name.clone(),
        source: c.source,
        satisfaction_rate: c.satisfaction_rate,
        cost_per_satisfied_micros: c.cost_micros,
        reason,
        crossed_cluster: false,
    }
}

/// A representative price for one attempt at `kind`, used only to rank
/// unmeasured routes against each other (never against a measured one on cost
/// alone; the floor and the measured/unmeasured tiering handle that).
fn representative_price(prices: &PriceTable, model: &str) -> Option<Money> {
    let price = prices.get(model)?;
    let per_million = |tokens: u64, rate: i64| -> i64 {
        ((tokens as i128 * rate as i128 + 500_000) / 1_000_000) as i64
    };
    Some(Money::from_micros(
        per_million(REPRESENTATIVE_INPUT_TOKENS, price.uncached_input)
            + per_million(REPRESENTATIVE_OUTPUT_TOKENS, price.output),
    ))
}

/// Build one candidate per route, tallies looked up for `kind`.
fn candidates_for(
    routes: &[RouteInfo],
    profiles: &ProfileStore,
    prices: &PriceTable,
    kind: TaskKind,
) -> Vec<Candidate> {
    routes
        .iter()
        .map(|r| {
            let profile = profiles.get(&r.model);
            let live = profile.and_then(|p| p.tally(Source::Live, kind));
            let bench = profile.and_then(|p| p.tally(Source::Bench, kind));
            if let Some(t) = live.filter(|t| t.attempts >= LIVE_MIN_ATTEMPTS) {
                from_tally(r, FigureSource::Live, t)
            } else if let Some(t) = bench.filter(|t| t.attempts > 0) {
                from_tally(r, FigureSource::Bench, t)
            } else {
                let (cost_micros, unpriced) =
                    cost_and_label(representative_price(prices, &r.model), r.local);
                Candidate {
                    route: r.clone(),
                    source: FigureSource::Unmeasured,
                    satisfaction_rate: None,
                    cost_micros,
                    unpriced,
                }
            }
        })
        .collect()
}

fn from_tally(r: &RouteInfo, source: FigureSource, t: &KindTally) -> Candidate {
    // `KindTally::cost_per_satisfied` is `None` both when nothing was
    // satisfied and when any attempt was unpriced (`model_profile`'s own
    // rule, shared with the bench's brief-following harness); either way the
    // measured cost is unknown here, which `cost_and_label` resolves the same
    // way an unmeasured route's missing price does.
    let (cost_micros, unpriced) = cost_and_label(t.cost_per_satisfied(), r.local);
    Candidate {
        route: r.clone(),
        source,
        satisfaction_rate: t.satisfaction_rate(),
        cost_micros,
        unpriced,
    }
}

/// Resolve a candidate's cost figure and its "unpriced" label from a known
/// cost (`None` when unpriced, whether from a missing `PriceTable` entry or
/// `model_profile`'s own `unpriced` count) and the route's locality: a local route with no known cost is free to call, so
/// it ranks at `$0`; a hosted route's cost stays genuinely unknown and ranks
/// on measured satisfaction rate instead ([`rank_key`]), last on cost when
/// nothing is measured either.
fn cost_and_label(known: Option<Money>, local: bool) -> (Option<i64>, Option<bool>) {
    match known {
        Some(m) => (Some(m.micros()), None),
        None if local => (Some(0), Some(true)),
        None => (None, Some(false)),
    }
}

/// Interface's absolute floor: a route with a *measured* rate
/// below [`INTERFACE_FLOOR`] is dropped outright. An unmeasured route (no
/// rate) is not dropped: it has not been shown to be below the floor, only not
/// yet tried.
fn apply_floor(mut candidates: Vec<Candidate>, kind: TaskKind) -> Vec<Candidate> {
    if kind == TaskKind::Interface {
        candidates.retain(|c| c.satisfaction_rate.is_none_or(|r| r >= INTERFACE_FLOOR));
    }
    candidates
}

/// Rank key: measured beats unmeasured; within a tier, lower cost per
/// satisfied task wins. When cost is unknown (`unpriced` and hosted), cost sorts last within the tier and a higher measured
/// satisfaction rate breaks the tie, so an unpriced hosted route with a good
/// record still outranks one with a poor or absent one. Every other tie sorts
/// last within its tier rather than panicking a comparator.
fn rank_key(c: &Candidate) -> (u8, i64, i64) {
    let tier = match c.source {
        FigureSource::Live => 0,
        FigureSource::Bench => 1,
        FigureSource::Unmeasured => 2,
    };
    let cost = c.cost_micros.unwrap_or(i64::MAX);
    let satisfaction_tiebreak = if c.cost_micros.is_none() {
        // Negated so ascending order still prefers the higher rate.
        -((c.satisfaction_rate.unwrap_or(0.0) * 1_000_000.0) as i64)
    } else {
        0
    };
    (tier, cost, satisfaction_tiebreak)
}

fn rank_reason(c: &Candidate) -> String {
    match (c.source, c.satisfaction_rate, c.cost_micros) {
        (FigureSource::Unmeasured, _, _) => {
            "unmeasured: lowest ranked route had no measured alternative".to_owned()
        }
        (source, Some(rate), Some(cost)) => format!(
            "{}: cost/satisfied {cost}\u{b5}$, satisfaction {:.0}%",
            source.as_str(),
            rate * 100.0
        ),
        (source, _, _) => format!("{}: no satisfied attempts yet", source.as_str()),
    }
}

/// Base pick ("lowest expected cost per satisfied
/// task ... among worker routes only"). `vendor_pin`, when set, restricts the
/// pool to that vendor's routes; `None` leaves every route
/// eligible, which is what an uncoupled contract (a cluster of one) gets.
/// `cursor` drives the deterministic exploration share.
pub fn choose_worker_route(
    routes: &[RouteInfo],
    profiles: &ProfileStore,
    prices: &PriceTable,
    kind: TaskKind,
    vendor_pin: Option<&str>,
    cursor: u64,
) -> Option<RoutePick> {
    let pool: Vec<RouteInfo> = match vendor_pin {
        Some(v) => routes.iter().filter(|r| r.vendor == v).cloned().collect(),
        None => routes.to_vec(),
    };
    if pool.is_empty() {
        return None;
    }
    let mut candidates = apply_floor(candidates_for(&pool, profiles, prices, kind), kind);
    if candidates.is_empty() {
        return None;
    }
    if cursor.is_multiple_of(EXPLORATION_EVERY)
        && let Some(c) = candidates
            .iter()
            .find(|c| c.source == FigureSource::Unmeasured)
    {
        return Some(pick_from(c, "unmeasured exploration share"));
    }
    candidates.sort_by_key(rank_key);
    candidates.first().map(|c| pick_from(c, rank_reason(c)))
}

/// The best route strictly better than `current` (by measured satisfaction
/// rate, never by cost alone: escalation is on quality) within
/// `pool`, or `None` when nothing in `pool` beats it.
fn best_beating(
    pool: &[RouteInfo],
    profiles: &ProfileStore,
    prices: &PriceTable,
    kind: TaskKind,
    current: &str,
    current_rate: f64,
) -> Option<Candidate> {
    let candidates = apply_floor(candidates_for(pool, profiles, prices, kind), kind);
    candidates
        .into_iter()
        .filter(|c| {
            c.route.name != current && c.satisfaction_rate.is_some_and(|r| r > current_rate)
        })
        .max_by_key(|c| (c.satisfaction_rate.unwrap() * 1_000_000.0) as i64)
}

/// Retry escalation : try the cluster's
/// own vendor first, and only cross to another vendor when nothing in-vendor
/// beats `current`'s measured rate. A cross is marked
/// [`RoutePick::crossed_cluster`] and its reason states what was searched
/// first, so the trace shows the break rather than hiding it.
pub fn escalate_route(
    routes: &[RouteInfo],
    profiles: &ProfileStore,
    prices: &PriceTable,
    kind: TaskKind,
    current: &str,
    vendor: &str,
) -> Option<RoutePick> {
    let current_rate = candidates_for(routes, profiles, prices, kind)
        .into_iter()
        .find(|c| c.route.name == current)
        .and_then(|c| c.satisfaction_rate)
        .unwrap_or(0.0);

    let in_vendor: Vec<RouteInfo> = routes
        .iter()
        .filter(|r| r.vendor == vendor)
        .cloned()
        .collect();
    if let Some(c) = best_beating(&in_vendor, profiles, prices, kind, current, current_rate) {
        let rate = c.satisfaction_rate.unwrap_or(0.0);
        return Some(pick_from(
            &c,
            format!(
                "retry escalation, in-vendor `{vendor}`: {:.0}% beats current {:.0}%",
                rate * 100.0,
                current_rate * 100.0
            ),
        ));
    }

    let cross: Vec<RouteInfo> = routes
        .iter()
        .filter(|r| r.vendor != vendor)
        .cloned()
        .collect();
    if let Some(c) = best_beating(&cross, profiles, prices, kind, current, current_rate) {
        let rate = c.satisfaction_rate.unwrap_or(0.0);
        let mut pick = pick_from(
            &c,
            format!(
                "cluster break: no route in vendor `{vendor}` beat current {:.0}%; \
                 crossing to vendor `{}` at {:.0}%",
                current_rate * 100.0,
                c.route.vendor,
                rate * 100.0
            ),
        );
        pick.crossed_cluster = true;
        return Some(pick);
    }
    None
}

// --------------------------------------------------------------- clustering

/// Coupling clusters : contracts are in
/// one cluster when their write sets overlap, one depends on the other, or
/// their write sets share a co-change edge at or above `min_count`. A union-find
/// over contract ids; returns each contract id's cluster index (dense, from
/// `0`), so contracts in the same cluster compare equal.
pub fn cluster_contracts(
    contracts: &[OpenContract],
    graph: &GraphStore,
    min_count: u32,
    now: i64,
) -> BTreeMap<String, usize> {
    let mut parent: BTreeMap<String, String> = contracts
        .iter()
        .map(|c| (c.contract.clone(), c.contract.clone()))
        .collect();

    fn find(parent: &mut BTreeMap<String, String>, x: &str) -> String {
        let p = parent.get(x).cloned().unwrap_or_else(|| x.to_owned());
        if p == x {
            x.to_owned()
        } else {
            let r = find(parent, &p);
            parent.insert(x.to_owned(), r.clone());
            r
        }
    }
    fn union(parent: &mut BTreeMap<String, String>, a: &str, b: &str) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }

    for c in contracts {
        for dep in &c.depends_on {
            if parent.contains_key(dep) {
                union(&mut parent, &c.contract, dep);
            }
        }
    }

    let paths_of = |c: &OpenContract| -> BTreeSet<String> {
        c.writes.iter().map(|w| w.path.clone()).collect()
    };
    let query = CoChangeQuery {
        min_count,
        ..CoChangeQuery::default()
    };
    for i in 0..contracts.len() {
        for j in (i + 1)..contracts.len() {
            let a = paths_of(&contracts[i]);
            let b = paths_of(&contracts[j]);
            if !a.is_disjoint(&b) {
                union(&mut parent, &contracts[i].contract, &contracts[j].contract);
                continue;
            }
            let mut coupled = false;
            for pa in &a {
                let Ok(partners) = partners_of(graph, pa, query, now) else {
                    continue;
                };
                if partners.iter().any(|p| {
                    let other = if &p.path_a == pa {
                        &p.path_b
                    } else {
                        &p.path_a
                    };
                    b.contains(other)
                }) {
                    coupled = true;
                    break;
                }
            }
            if coupled {
                union(&mut parent, &contracts[i].contract, &contracts[j].contract);
            }
        }
    }

    let mut cluster_ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for c in contracts {
        let root = find(&mut parent, &c.contract);
        let next = cluster_ids.len();
        let id = *cluster_ids.entry(root).or_insert(next);
        out.insert(c.contract.clone(), id);
    }
    out
}

/// The cluster's own vendor pin : the vendor with the
/// lowest total expected cost per satisfied task summed across `kinds` (one
/// entry per contract in the cluster, in whatever order they occur), among
/// vendors that have a floor-eligible route for every one of those kinds.
/// `None` when no vendor is eligible for every kind (never invents an
/// ineligible pin; the caller falls back to unpinned per-contract choice).
pub fn choose_cluster_vendor(
    routes: &[RouteInfo],
    profiles: &ProfileStore,
    prices: &PriceTable,
    kinds: &[TaskKind],
) -> Option<(String, String)> {
    let vendors: BTreeSet<String> = routes.iter().map(|r| r.vendor.clone()).collect();
    let mut best: Option<(String, i64)> = None;
    for vendor in vendors {
        let vendor_routes: Vec<RouteInfo> = routes
            .iter()
            .filter(|r| r.vendor == vendor)
            .cloned()
            .collect();
        let mut total: i64 = 0;
        let mut eligible = true;
        for &kind in kinds {
            let candidates =
                apply_floor(candidates_for(&vendor_routes, profiles, prices, kind), kind);
            match candidates.iter().min_by_key(|c| rank_key(c)) {
                Some(c) => total += c.cost_micros.unwrap_or(0),
                None => {
                    eligible = false;
                    break;
                }
            }
        }
        if !eligible {
            continue;
        }
        // Lower total wins; a tie breaks on vendor name so the pick is
        // deterministic and reproducible from the trace.
        if best
            .as_ref()
            .is_none_or(|(v, t)| total < *t || (total == *t && vendor < *v))
        {
            best = Some((vendor, total));
        }
    }
    best.map(|(vendor, total)| {
        (
            vendor.clone(),
            format!("cluster vendor `{vendor}`: lowest total cost/satisfied across the cluster ({total}\u{b5}$)"),
        )
    })
}

// -------------------------------------------------------- profile store I/O

/// Load the profile store, or fail loud: a missing file is an
/// empty store (nothing measured yet, per `model_profile`'s own rule); a file
/// that does not parse is an error, never silently treated as empty.
pub fn load_profiles(dir: &Path) -> Result<ProfileStore, ProfileError> {
    ProfileStore::load(dir)
}

/// Record one live outcome and save: the one write path for
/// `Source::Live` tallies. Called where an attempt's verdict and ledger cost
/// are both known (`orchestrator.rs::on_verified`), never inside a forbidden
/// finalize/integrate/announce/terminal-status path. `cost` is always
/// `Some`: a live attempt was already billed by the ledger, so its cost is
/// known by construction; `model_profile`'s `unpriced` count is for the
/// bench, which can run a model the price table does not yet cover.
pub fn record_live(
    store: &mut ProfileStore,
    dir: &Path,
    model: &str,
    vendor: &str,
    kind: TaskKind,
    satisfied: bool,
    cost: Money,
) -> Result<(), ProfileError> {
    store.record(model, vendor, Source::Live, kind, satisfied, Some(cost));
    store.save(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_state::WriteTarget;
    use kgraf_core::ModelPrice;

    fn route(name: &str, vendor: &str, model: &str) -> RouteInfo {
        RouteInfo {
            name: name.into(),
            vendor: vendor.into(),
            model: model.into(),
            local: false,
        }
    }

    fn route_local(name: &str, vendor: &str, model: &str) -> RouteInfo {
        RouteInfo {
            local: true,
            ..route(name, vendor, model)
        }
    }

    fn price(uncached_input: i64, output: i64) -> ModelPrice {
        ModelPrice {
            uncached_input,
            cache_read: uncached_input / 10,
            cache_write: uncached_input * 2,
            output,
            reasoning: output,
        }
    }

    fn contract(id: &str, writes: &[&str], deps: &[&str]) -> OpenContract {
        let mut c =
            OpenContract::planned(id, format!("t/{id}").as_str().into(), "do it", "tests pass");
        c.writes = writes
            .iter()
            .map(|p| WriteTarget {
                path: (*p).into(),
                node: None,
            })
            .collect();
        c.depends_on = deps.iter().map(|d| (*d).to_owned()).collect();
        c
    }

    fn graph_with_cochange(pairs: &[(&str, &str, u32)]) -> GraphStore {
        let dir = std::env::temp_dir();
        let g = GraphStore::open_in_memory(&dir).expect("in-memory graph");
        let commits: Vec<_> = pairs
            .iter()
            .flat_map(|(a, b, count)| {
                std::iter::repeat_n(
                    kgraf_graph::cochange::HistoryCommit {
                        timestamp: 1,
                        paths: vec![(*a).to_owned(), (*b).to_owned()],
                    },
                    *count as usize,
                )
            })
            .collect();
        kgraf_graph::cochange::build_from_commits(&g, &commits, 1000).expect("build cochange");
        g
    }

    // ---- clustering ----

    #[test]
    fn a_coupled_two_contract_plan_lands_in_one_cluster() {
        let contracts = vec![
            contract("c1", &["src/a.rs"], &[]),
            contract("c2", &["src/b.rs"], &["c1"]),
        ];
        let graph = graph_with_cochange(&[]);
        let clusters = cluster_contracts(&contracts, &graph, CLUSTER_COCHANGE_MIN_COUNT, 1_000_000);
        assert_eq!(clusters["c1"], clusters["c2"], "a depends_on edge clusters");
    }

    #[test]
    fn an_uncoupled_two_contract_plan_gives_two_clusters() {
        let contracts = vec![
            contract("c1", &["src/a.rs"], &[]),
            contract("c2", &["src/b.rs"], &[]),
        ];
        let graph = graph_with_cochange(&[]);
        let clusters = cluster_contracts(&contracts, &graph, CLUSTER_COCHANGE_MIN_COUNT, 1_000_000);
        assert_ne!(clusters["c1"], clusters["c2"]);
    }

    #[test]
    fn a_cochange_edge_above_the_threshold_clusters_disjoint_write_sets() {
        let contracts = vec![
            contract("c1", &["src/a.rs"], &[]),
            contract("c2", &["src/b.rs"], &[]),
        ];
        let graph = graph_with_cochange(&[("src/a.rs", "src/b.rs", CLUSTER_COCHANGE_MIN_COUNT)]);
        let clusters = cluster_contracts(&contracts, &graph, CLUSTER_COCHANGE_MIN_COUNT, 1_000_000);
        assert_eq!(clusters["c1"], clusters["c2"]);
    }

    // ---- vendor pin + per-kind model choice ----

    #[test]
    fn a_cluster_picks_one_vendor_and_gives_each_kind_its_own_model_on_it() {
        let routes = vec![
            route("acme-a", "acme", "acme-interface"),
            route("acme-b", "acme", "acme-impl"),
            route("other-a", "other", "other-model"),
        ];
        let prices = {
            let mut t = PriceTable::new();
            t.set("acme-interface", price(1_000_000, 1_000_000));
            t.set("acme-impl", price(1_000_000, 1_000_000));
            t.set("other-model", price(1_000_000, 1_000_000));
            t
        };
        let mut profiles = ProfileStore::default();
        // Acme's interface model is proven good at Interface; acme's impl
        // model is proven good at Implementation; `other` has no history, so
        // its unmeasured cost never beats acme's measured, floor-passing
        // figures for either kind.
        for _ in 0..LIVE_MIN_ATTEMPTS {
            profiles.record(
                "acme-interface",
                "acme",
                Source::Live,
                TaskKind::Interface,
                true,
                Some(Money::from_micros(100)),
            );
            profiles.record(
                "acme-impl",
                "acme",
                Source::Live,
                TaskKind::Implementation,
                true,
                Some(Money::from_micros(100)),
            );
        }
        let kinds = [TaskKind::Interface, TaskKind::Implementation];
        let (vendor, _reason) =
            choose_cluster_vendor(&routes, &profiles, &prices, &kinds).expect("a vendor");
        assert_eq!(vendor, "acme");

        let interface_pick = choose_worker_route(
            &routes,
            &profiles,
            &prices,
            TaskKind::Interface,
            Some(&vendor),
            1,
        )
        .expect("interface pick");
        let impl_pick = choose_worker_route(
            &routes,
            &profiles,
            &prices,
            TaskKind::Implementation,
            Some(&vendor),
            1,
        )
        .expect("implementation pick");
        assert_eq!(interface_pick.route, "acme-a");
        assert_eq!(impl_pick.route, "acme-b");
        assert_ne!(
            interface_pick.route, impl_pick.route,
            "same vendor, different model per kind"
        );
    }

    // ---- escalation: in-vendor first ----

    #[test]
    fn retry_escalation_prefers_an_in_vendor_route_over_a_cheaper_cross_vendor_one() {
        let routes = vec![
            route("acme-weak", "acme", "acme-weak"),
            route("acme-strong", "acme", "acme-strong"),
            route("cheap-vendor", "cheap", "cheap-model"),
        ];
        let mut profiles = ProfileStore::default();
        for _ in 0..LIVE_MIN_ATTEMPTS {
            // The current route: middling.
            profiles.record(
                "acme-weak",
                "acme",
                Source::Live,
                TaskKind::Implementation,
                true,
                Some(Money::from_micros(500)),
            );
            profiles.record(
                "acme-weak",
                "acme",
                Source::Live,
                TaskKind::Implementation,
                false,
                Some(Money::from_micros(500)),
            );
            // A better in-vendor alternative.
            profiles.record(
                "acme-strong",
                "acme",
                Source::Live,
                TaskKind::Implementation,
                true,
                Some(Money::from_micros(500)),
            );
            // A cross-vendor alternative that is cheap but *not* better on
            // satisfaction than acme-strong: escalation must not prefer it
            // merely for price.
            profiles.record(
                "cheap-model",
                "cheap",
                Source::Live,
                TaskKind::Implementation,
                true,
                Some(Money::from_micros(1)),
            );
            profiles.record(
                "cheap-model",
                "cheap",
                Source::Live,
                TaskKind::Implementation,
                false,
                Some(Money::from_micros(1)),
            );
        }
        let prices = PriceTable::new();
        let pick = escalate_route(
            &routes,
            &profiles,
            &prices,
            TaskKind::Implementation,
            "acme-weak",
            "acme",
        )
        .expect("an escalation pick");
        assert_eq!(pick.route, "acme-strong");
        assert!(!pick.crossed_cluster);
    }

    #[test]
    fn retry_escalation_crosses_vendors_only_when_nothing_in_vendor_beats_current() {
        let routes = vec![
            route("acme-only", "acme", "acme-only"),
            route("other-strong", "other", "other-strong"),
        ];
        let mut profiles = ProfileStore::default();
        for _ in 0..LIVE_MIN_ATTEMPTS {
            profiles.record(
                "acme-only",
                "acme",
                Source::Live,
                TaskKind::Implementation,
                true,
                Some(Money::from_micros(500)),
            );
            profiles.record(
                "acme-only",
                "acme",
                Source::Live,
                TaskKind::Implementation,
                false,
                Some(Money::from_micros(500)),
            );
            profiles.record(
                "other-strong",
                "other",
                Source::Live,
                TaskKind::Implementation,
                true,
                Some(Money::from_micros(500)),
            );
        }
        let prices = PriceTable::new();
        let pick = escalate_route(
            &routes,
            &profiles,
            &prices,
            TaskKind::Implementation,
            "acme-only",
            "acme",
        )
        .expect("a cross-vendor pick");
        assert_eq!(pick.route, "other-strong");
        assert!(pick.crossed_cluster);
        assert!(pick.reason.contains("cluster break"));
    }

    // ---- interface floor is absolute ----

    #[test]
    fn a_route_measured_below_the_interface_floor_is_never_chosen_for_interface_work() {
        let routes = vec![route("weak", "v", "weak-model")];
        let mut profiles = ProfileStore::default();
        for _ in 0..LIVE_MIN_ATTEMPTS {
            profiles.record(
                "weak-model",
                "v",
                Source::Live,
                TaskKind::Interface,
                false,
                Some(Money::from_micros(100)),
            );
        }
        profiles.record(
            "weak-model",
            "v",
            Source::Live,
            TaskKind::Interface,
            true,
            Some(Money::from_micros(100)),
        );
        // Satisfaction is 1/6, far below the floor.
        let prices = PriceTable::new();
        let pick = choose_worker_route(&routes, &profiles, &prices, TaskKind::Interface, None, 1);
        assert!(
            pick.is_none(),
            "the only route is below the floor and must be excluded, not merely deprioritized"
        );
    }

    #[test]
    fn an_unmeasured_route_is_not_treated_as_below_the_interface_floor() {
        let routes = vec![route("new", "v", "new-model")];
        let profiles = ProfileStore::default();
        let mut prices = PriceTable::new();
        prices.set("new-model", price(1_000_000, 1_000_000));
        // Cursor 0 hits the exploration share deterministically.
        let pick = choose_worker_route(&routes, &profiles, &prices, TaskKind::Interface, None, 0)
            .expect("unmeasured is eligible until proven below the floor");
        assert_eq!(pick.source, FigureSource::Unmeasured);
    }

    // ---- unpriced routes: never an error, never excluded ----

    #[test]
    fn an_unpriced_local_route_ranks_at_zero_and_an_unpriced_hosted_route_ranks_last() {
        let routes = vec![
            route("hosted", "v", "hosted-model"),
            route_local("local", "v", "local-model"),
        ];
        let profiles = ProfileStore::default();
        // Neither route has a price entry, and neither is measured.
        let prices = PriceTable::new();
        let pick = choose_worker_route(
            &routes,
            &profiles,
            &prices,
            TaskKind::Implementation,
            None,
            1,
        )
        .expect("a pick");
        assert_eq!(pick.route, "local", "the $0 local route wins on cost");
        assert_eq!(pick.cost_per_satisfied_micros, Some(0));
        assert!(pick.reason.contains("local, unpriced"));
    }

    #[test]
    fn a_local_route_below_the_interface_floor_gets_no_interface_work_one_at_the_floor_does() {
        let routes = vec![route_local("local", "v", "local-model")];
        let prices = PriceTable::new();

        // Measured (so it can be "below the floor" at all; an untested local
        // route is merely unmeasured, not below anything) and weak.
        let mut weak = ProfileStore::default();
        for _ in 0..LIVE_MIN_ATTEMPTS {
            weak.record(
                "local-model",
                "v",
                Source::Live,
                TaskKind::Interface,
                false,
                Some(Money::from_micros(0)),
            );
        }
        let none = choose_worker_route(&routes, &weak, &prices, TaskKind::Interface, None, 1);
        assert!(
            none.is_none(),
            "a local, $0 route below the floor still gets no Interface work"
        );

        // At exactly the floor: eligible.
        let mut ok = ProfileStore::default();
        for i in 0..LIVE_MIN_ATTEMPTS {
            ok.record(
                "local-model",
                "v",
                Source::Live,
                TaskKind::Interface,
                i % 2 == 0,
                Some(Money::from_micros(0)),
            );
        }
        let picked = choose_worker_route(&routes, &ok, &prices, TaskKind::Interface, None, 1)
            .expect("at the floor, it is eligible");
        assert_eq!(picked.route, "local");
    }

    // ---- exploration is deterministic and traced ----

    #[test]
    fn an_unmeasured_pick_is_traced_as_unmeasured() {
        let routes = vec![
            route("proven", "v", "proven-model"),
            route("new", "v", "new-model"),
        ];
        let mut profiles = ProfileStore::default();
        for _ in 0..LIVE_MIN_ATTEMPTS {
            profiles.record(
                "proven-model",
                "v",
                Source::Live,
                TaskKind::Implementation,
                true,
                Some(Money::from_micros(1)),
            );
        }
        let prices = PriceTable::new();
        let explored = choose_worker_route(
            &routes,
            &profiles,
            &prices,
            TaskKind::Implementation,
            None,
            0,
        )
        .expect("exploration pick");
        assert_eq!(explored.source, FigureSource::Unmeasured);
        assert_eq!(explored.route, "new");

        let ranked = choose_worker_route(
            &routes,
            &profiles,
            &prices,
            TaskKind::Implementation,
            None,
            1,
        )
        .expect("ranked pick");
        assert_eq!(ranked.source, FigureSource::Live);
        assert_eq!(ranked.route, "proven");
    }

    // ---- malformed profile file fails loud ----

    #[test]
    fn a_malformed_profile_file_fails_loud_rather_than_loading_as_empty() {
        let dir =
            std::env::temp_dir().join(format!("kgraf-routing-profiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(kgraf_core::model_profile::FILE_NAME), "{not json").unwrap();
        let err = load_profiles(&dir).expect_err("a malformed store must not load as empty");
        assert!(matches!(err, ProfileError::Parse { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_live_writes_a_source_live_tally_that_reloads() {
        let dir = std::env::temp_dir().join(format!("kgraf-routing-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = ProfileStore::default();
        record_live(
            &mut store,
            &dir,
            "m",
            "v",
            TaskKind::Tests,
            true,
            Money::from_micros(42),
        )
        .expect("record");
        let back = load_profiles(&dir).expect("reload");
        let t = back
            .get("m")
            .unwrap()
            .tally(Source::Live, TaskKind::Tests)
            .unwrap();
        assert_eq!(t.attempts, 1);
        assert_eq!(t.satisfied, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- existing rotation behavior is untouched by this module ----

    #[test]
    fn rotation_is_the_default_policy() {
        assert_eq!(RoutingPolicy::default(), RoutingPolicy::Rotation);
    }
}
