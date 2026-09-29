//! At most two contracts of one coupling cluster in flight at once (decision
//! 0082, coordination-hardening item 6).
//!
//! Decision 0023 measured success falling monotonically with agent count
//! (68.6% / 46.5% / 30.0% at 2 / 3 / 4 agents) and 0012 measured cross-agent
//! conflict at 41.7%; both were measured across *any* concurrent agents, but
//! the risk is concentrated where two contracts touch the same code, so this
//! caps concurrency **per coupling cluster** rather than lowering the
//! process-wide ceiling (0017's work-item cap, 0067's per-machine slider):
//! uncoupled contracts, which are most of a plan, keep full concurrency.
//!
//! This module holds no clustering logic of its own: it reuses
//! [`crate::routing::cluster_contracts`] (decision 0075) so a cluster means
//! the same thing everywhere in the orchestrator (write-set overlap, a
//! dependency edge, or a co-change edge at or above threshold). It is also
//! not a budget mechanism: it never reads or writes [`crate::admission::BudgetBook`]
//! or any spend cap (kGraf does not limit spend; this is
//! concurrency, not cost).
//!
//! [`check`] is the one entry point, called from the orchestrator's dispatch
//! path (`orchestrator.rs::try_dispatch`) before [`crate::admission::AdmissionGate::try_admit`],
//! and reused unchanged whether a contract is a fresh dispatch or a
//! speculative attempt already counted in the caller's in-flight set: the cap
//! counts contract ids present in that set, whatever put them there (decision
//! 0074's speculative merge included), so a contract a speculative round is
//! actively considering counts toward its cluster's cap exactly like any
//! other in-flight member.

use std::collections::BTreeMap;

/// The starting per-cluster cap: a tuning point (0023 gives no optimum for
/// clusters specifically), not a researched number.
pub const DEFAULT_MAX_IN_FLIGHT_PER_CLUSTER: usize = 2;

/// A cap of `0` would refuse every coupled contract forever and stall a run
/// silently (silent degradation is a defect), so it is floored to `1`
/// rather than rejected at config time: `1` still lets an owner serialize a
/// cluster deliberately, and a stalled run is a worse failure mode than a
/// looser cap than asked for. The floor applies here, at the one call site,
/// so every caller sees the same effective cap without duplicating the rule.
pub fn effective_cap(configured: usize) -> usize {
    configured.max(1)
}

/// Why a contract is held at the cluster cap, for the caller's trace. The
/// caller traces it under `trace::DeferReason::Capacity` (the exhaustive
/// match in `orchestrator/coord.rs::wait_of_defer` is out of this build
/// item's edit scope) with a `"cluster cap: "`-prefixed detail, rather than a
/// new `DeferReason` variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterCapHold {
    /// The cluster index (`routing::cluster_contracts`'s dense id).
    pub cluster: usize,
    /// The cluster's other members already in flight, sorted for a
    /// reproducible trace.
    pub in_flight: Vec<String>,
    /// The effective cap (after the floor in [`effective_cap`]).
    pub cap: usize,
}

/// Whether `contract` may be admitted now, given `clusters` (as
/// [`crate::routing::cluster_contracts`] returns them) and the ids already in
/// flight. `None` admits: either `contract` is in no multi-member cluster (a
/// cluster of one is not coupled, per `routing.rs`), or its cluster has room.
/// `Some` holds it, naming the cluster and its current occupants so the
/// caller can trace and defer without deadlock (the caller re-tries once the
/// cluster drains; nothing here blocks).
pub fn check(
    contract: &str,
    clusters: &BTreeMap<String, usize>,
    in_flight_ids: impl IntoIterator<Item = String>,
    cap: usize,
) -> Option<ClusterCapHold> {
    let cap = effective_cap(cap);
    let cluster = *clusters.get(contract)?;
    let mut members: Vec<String> = in_flight_ids
        .into_iter()
        .filter(|id| id != contract && clusters.get(id) == Some(&cluster))
        .collect();
    if members.len() < cap {
        return None;
    }
    members.sort();
    Some(ClusterCapHold {
        cluster,
        in_flight: members,
        cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clusters(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(id, c)| (id.to_string(), *c)).collect()
    }

    #[test]
    fn a_cluster_under_the_cap_admits() {
        let clusters = clusters(&[("a", 0), ("b", 0), ("c", 0)]);
        let in_flight = ["a".to_owned()];
        assert!(check("b", &clusters, in_flight, 2).is_none());
    }

    #[test]
    fn a_cluster_at_the_cap_is_held() {
        let clusters = clusters(&[("a", 0), ("b", 0), ("c", 0)]);
        let in_flight = ["a".to_owned(), "b".to_owned()];
        let hold = check("c", &clusters, in_flight, 2).expect("held");
        assert_eq!(hold.cluster, 0);
        assert_eq!(hold.in_flight, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(hold.cap, 2);
    }

    /// A contract counted in-flight for any reason (a speculative round is
    /// actively considering it, same as a plain dispatch) still occupies its
    /// cluster's slot: `check` reads the caller's in-flight set at face
    /// value and does not distinguish why a member is there.
    #[test]
    fn a_speculative_attempt_counts_toward_the_cap() {
        let clusters = clusters(&[("a", 0), ("b", 0), ("c", 0)]);
        // "b" stands in for a contract a speculative round holds a snapshot
        // of: from this module's view it is just another in-flight member.
        let in_flight = ["a".to_owned(), "b".to_owned()];
        assert!(check("c", &clusters, in_flight, 2).is_some());
    }

    #[test]
    fn a_contract_in_no_known_cluster_is_never_held() {
        let clusters = clusters(&[("a", 0)]);
        let in_flight: [String; 0] = [];
        assert!(check("z", &clusters, in_flight, 1).is_none());
    }

    #[test]
    fn a_cluster_of_one_is_uncoupled_and_never_held() {
        // Contract "a" is its own cluster (id 1): it has no partner, so the
        // in-flight set (however large) never matches its cluster.
        let clusters = clusters(&[("a", 1), ("b", 0), ("c", 0)]);
        let in_flight = ["b".to_owned(), "c".to_owned()];
        assert!(check("a", &clusters, in_flight, 1).is_none());
    }

    #[test]
    fn a_configured_cap_of_zero_is_floored_to_one() {
        assert_eq!(effective_cap(0), 1);
        let clusters = clusters(&[("a", 0), ("b", 0)]);
        let in_flight = ["a".to_owned()];
        let hold = check("b", &clusters, in_flight, 0).expect("held at the floor of 1");
        assert_eq!(hold.cap, 1);
    }

    #[test]
    fn draining_the_cluster_admits_the_held_contract() {
        let clusters = clusters(&[("a", 0), ("b", 0), ("c", 0)]);
        let full = ["a".to_owned(), "b".to_owned()];
        assert!(check("c", &clusters, full, 2).is_some(), "held at the cap");
        // "a" finished and left the caller's in-flight set: no deadlock, "c"
        // is admitted on the very next check with no other change.
        let drained = ["b".to_owned()];
        assert!(
            check("c", &clusters, drained, 2).is_none(),
            "admitted once the cluster drained"
        );
    }
}
