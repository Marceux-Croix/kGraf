//! Why each open contract is not running: the one-line status, where a worker waiting on a
//! lease is visibly distinct.
//!
//! Decision 0016 separates eligibility (dependencies met) from admissibility (no
//! write-set collision) and requires the interface to tell a blocked node from a
//! colliding one; 0017 adds the concurrency cap; 0018 adds the gate halt, "distinct
//! from a dependency block and from a concurrency-cap hold". Each is a different
//! [`WaitState`], kept per contract in the run record (so a status answer and a
//! restart read the same thing) and traced when a contract's state changes kind
//! (so a long lease wait is one record, not one per tick).
//!
//! Written new: the state is this orchestrator's own dispatch order, which no
//! reference implementation shares.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Why an open contract is where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "wait", rename_all = "snake_case")]
pub enum WaitState {
    /// Workers are running on it.
    Running,
    /// Admissible later: its write set is leased by another holder (0048).
    Lease { detail: String },
    /// Not eligible: these dependencies are not verified yet (0016).
    Dependency { on: Vec<String> },
    /// Not eligible: a quality gate over what it depends on has not passed (0018).
    Gate { gate: String },
    /// Eligible and admissible, refused by the work-item cap or the live-agent
    /// ceiling (0017).
    Capacity { detail: String },
    /// A transient readiness failure (§A7).
    Readiness { detail: String },
    /// Held for the user; `category` never carries model or verifier text.
    Held { category: String },
    /// Decision 0067, point 4: a worker's `ask_user_question` is waiting for
    /// the user (the orchestrator itself could not answer it).
    AwaitingAnswer,
    /// Decision 0067, point 6: a message is over the soft rate on its
    /// connection and its delivery is paced, never dropped (`waits.rs`'s own
    /// doc comment on `WaitState`).
    PacedDelivery { detail: String },
    /// Open, and not yet looked at by a dispatch round (just planned, just
    /// resumed, or just returned to planned). Shown, never left out.
    Unchecked,
    /// A merge conflict dispatched a resolver worker in its place (owner
    /// request 17, decision 0073); `resolver` is that contract's id. Distinct
    /// from `Held`: nothing here needs the user, only the resolver's outcome.
    Resolver { resolver: String },
}

/// The kind alone, for tracing and counting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    Running,
    Lease,
    Dependency,
    Gate,
    Capacity,
    Readiness,
    Held,
    AwaitingAnswer,
    PacedDelivery,
    Unchecked,
    Resolver,
}

impl WaitKind {
    /// The status-line word.
    pub fn as_str(self) -> &'static str {
        match self {
            WaitKind::Running => "running",
            WaitKind::Lease => "waiting on a lease",
            WaitKind::Dependency => "blocked on a dependency",
            WaitKind::Gate => "blocked on a gate",
            WaitKind::Capacity => "concurrency-capped",
            WaitKind::Readiness => "waiting on readiness",
            WaitKind::Held => "held",
            WaitKind::AwaitingAnswer => "awaiting an answer",
            WaitKind::PacedDelivery => "delivery paced",
            WaitKind::Unchecked => "not yet checked",
            WaitKind::Resolver => "a resolver is working on a merge conflict",
        }
    }
}

impl WaitState {
    pub fn kind(&self) -> WaitKind {
        match self {
            WaitState::Running => WaitKind::Running,
            WaitState::Lease { .. } => WaitKind::Lease,
            WaitState::Dependency { .. } => WaitKind::Dependency,
            WaitState::Gate { .. } => WaitKind::Gate,
            WaitState::Capacity { .. } => WaitKind::Capacity,
            WaitState::Readiness { .. } => WaitKind::Readiness,
            WaitState::Held { .. } => WaitKind::Held,
            WaitState::AwaitingAnswer => WaitKind::AwaitingAnswer,
            WaitState::PacedDelivery { .. } => WaitKind::PacedDelivery,
            WaitState::Unchecked => WaitKind::Unchecked,
            WaitState::Resolver { .. } => WaitKind::Resolver,
        }
    }

    /// The detail a trace record carries.
    pub fn detail(&self) -> String {
        match self {
            WaitState::Running | WaitState::Unchecked | WaitState::AwaitingAnswer => String::new(),
            WaitState::Lease { detail }
            | WaitState::Capacity { detail }
            | WaitState::Readiness { detail }
            | WaitState::PacedDelivery { detail } => detail.clone(),
            WaitState::Dependency { on } => on.join(", "),
            WaitState::Gate { gate } => gate.clone(),
            WaitState::Held { category } => category.clone(),
            WaitState::Resolver { resolver } => resolver.clone(),
        }
    }

    /// One contract's status fragment, safe for a channel reply: dependency and
    /// gate ids are the orchestrator's own, and lease, capacity, budget and
    /// readiness details are not repeated (they can name paths and holders).
    pub fn fragment(&self, contract: &str) -> String {
        match self {
            WaitState::Dependency { on } => {
                format!("{contract} {} ({})", self.kind().as_str(), on.join(", "))
            }
            WaitState::Gate { gate } => format!("{contract} {} ({gate})", self.kind().as_str()),
            WaitState::Held { category } => {
                format!("{contract} {} ({category})", self.kind().as_str())
            }
            _ => format!("{contract} {}", self.kind().as_str()),
        }
    }
}

/// The one-line status over every open contract, in contract order.
pub fn one_line(waits: &BTreeMap<String, WaitState>) -> String {
    if waits.is_empty() {
        return "nothing waiting".to_owned();
    }
    waits
        .iter()
        .map(|(c, w)| w.fragment(c))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Record `state` for `contract`. Returns whether its kind changed, which is when
/// a trace record is due.
pub fn set(waits: &mut BTreeMap<String, WaitState>, contract: &str, state: WaitState) -> bool {
    let changed = waits.get(contract).map(WaitState::kind) != Some(state.kind());
    waits.insert(contract.to_owned(), state.scrubbed());
    changed
}

impl WaitState {
    /// A detail can quote an error from a provider, a server or a lease
    /// holder, and is stored and shown (decision 0071, H2): persist-scrubbed.
    fn scrubbed(self) -> Self {
        let s = kgraf_secrets::scrub_persist_owned;
        match self {
            WaitState::Lease { detail } => WaitState::Lease { detail: s(detail) },
            WaitState::Capacity { detail } => WaitState::Capacity { detail: s(detail) },
            WaitState::Readiness { detail } => WaitState::Readiness { detail: s(detail) },
            WaitState::PacedDelivery { detail } => WaitState::PacedDelivery { detail: s(detail) },
            other @ (WaitState::Running
            | WaitState::Dependency { .. }
            | WaitState::Gate { .. }
            | WaitState::Held { .. }
            | WaitState::Unchecked
            | WaitState::AwaitingAnswer
            | WaitState::Resolver { .. }) => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_waits_render_as_three_different_words() {
        let mut w = BTreeMap::new();
        set(
            &mut w,
            "a",
            WaitState::Lease {
                detail: "src/x.rs held by r1/b@n until 9".into(),
            },
        );
        set(
            &mut w,
            "b",
            WaitState::Dependency {
                on: vec!["c".into()],
            },
        );
        set(
            &mut w,
            "d",
            WaitState::Capacity {
                detail: "work-item cap 1 reached".into(),
            },
        );
        let line = one_line(&w);
        assert_eq!(
            line,
            "a waiting on a lease; b blocked on a dependency (c); d concurrency-capped"
        );
        // The lease detail names a path and a holder; a channel reply does not
        // repeat it.
        assert!(!line.contains("src/x.rs"));
    }

    #[test]
    fn an_open_contract_with_no_noted_state_is_shown_not_dropped() {
        let mut w = BTreeMap::new();
        w.insert("a".to_owned(), WaitState::Unchecked);
        let line = one_line(&w);
        assert_eq!(line, "a not yet checked");
        assert_ne!(line, "nothing waiting");
    }

    #[test]
    fn only_a_change_of_kind_is_a_new_trace_record() {
        let mut w = BTreeMap::new();
        let lease = |until: i64| WaitState::Lease {
            detail: format!("held until {until}"),
        };
        assert!(set(&mut w, "a", lease(1)));
        // A renewal moves the expiry; the contract is still waiting on a lease.
        assert!(!set(&mut w, "a", lease(2)));
        assert_eq!(w["a"], lease(2), "the latest detail is kept");
        assert!(set(&mut w, "a", WaitState::Running));
    }
}
