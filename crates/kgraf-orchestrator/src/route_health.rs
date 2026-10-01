//! Route health: a route whose calls fail at the transport is taken out of
//! rotation, named, and probed again later.
//!
//! **Why.** A route whose server is down (connection refused) or drops its
//! connections fails every call it gets, and the failure says nothing about
//! the model or the contract. Before this module such an attempt counted as
//! the contract's failure, rotation kept handing the route new contracts, and
//! neither the trace nor the run output named the route. Now each such fault
//! ([`kgraf_provider::ProviderError::is_route_fault`], carried on
//! `WorkerResult::route_fault`) is recorded here; the orchestrator traces it,
//! passes the route over while it is out, moves the contract to a healthy
//! route without counting the attempt against it, and holds the contract with
//! the reason only when no other route can take it and the route has failed
//! [`HOLD_AFTER`] times in a row.
//!
//! **Recovery.** A route is out for a cooldown that doubles with each fault in
//! a row ([`RouteHealth::fault`]), from the configured base up to
//! [`MAX_COOLDOWN_MS`]. When it ends, the route is eligible again and its next
//! dispatch is the probe: a call that does not fault at the transport brings
//! it back ([`RouteHealth::success`]); another fault takes it out again for
//! longer.
//!
//! A state machine per route:
//!
//! ```text
//!   Healthy --fault--> Out(n=1, until=now+base)
//!   Out(n, until) --now >= until--> Probing(n)          (eligible again)
//!   Probing(n) --fault--> Out(n+1, until=now+base*2^n)
//!   Probing(n) | Out(n) --success--> Healthy            (traced as recovered)
//! ```
//!
//! Pure state with the clock passed in, so the rules are tested as a
//! mechanism (no I/O here). Written new: the orchestrator had per-contract
//! readiness skips (`not_ready_routes`) but nothing run-wide for a route that
//! answers readiness and then drops its calls.

use std::collections::BTreeMap;

/// Faults in a row after which a contract that no other route can take is
/// held with the reason, rather than waiting for another probe.
pub const HOLD_AFTER: u32 = 3;

/// The longest a faulted route stays out before it is probed again.
pub const MAX_COOLDOWN_MS: i64 = 600_000;

/// One route's record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteState {
    /// Transport faults since the route last answered.
    pub in_a_row: u32,
    /// Transport faults over the whole run.
    pub total: u32,
    /// The last fault's error text (persist-scrubbed by the caller).
    pub last_error: String,
    /// Until when the route is out of rotation (milliseconds on the
    /// orchestrator's clock); meaningful only while `in_a_row > 0`.
    pub out_until_ms: i64,
}

/// Every route's record, for one run.
#[derive(Clone, Debug, Default)]
pub struct RouteHealth {
    routes: BTreeMap<String, RouteState>,
    base_cooldown_ms: i64,
}

impl RouteHealth {
    pub fn new(base_cooldown_ms: i64) -> Self {
        Self {
            routes: BTreeMap::new(),
            base_cooldown_ms: base_cooldown_ms.max(1),
        }
    }

    /// Record a transport fault on `route` at `now`; returns its new state.
    pub fn fault(&mut self, route: &str, error: &str, now: i64) -> RouteState {
        let base = self.base_cooldown_ms;
        let s = self.routes.entry(route.to_owned()).or_default();
        s.in_a_row = s.in_a_row.saturating_add(1);
        s.total = s.total.saturating_add(1);
        s.last_error = error.to_owned();
        let shift = (s.in_a_row - 1).min(20);
        s.out_until_ms = now.saturating_add(base.saturating_mul(1 << shift).min(MAX_COOLDOWN_MS));
        s.clone()
    }

    /// Record that `route` answered (a call that did not fault at the
    /// transport). Returns the faults in a row it recovers from, if any.
    pub fn success(&mut self, route: &str) -> Option<u32> {
        let s = self.routes.get_mut(route)?;
        let was = s.in_a_row;
        s.in_a_row = 0;
        (was > 0).then_some(was)
    }

    /// Whether `route` is out of rotation at `now`.
    pub fn is_out(&self, route: &str, now: i64) -> bool {
        self.routes
            .get(route)
            .is_some_and(|s| s.in_a_row > 0 && now < s.out_until_ms)
    }

    pub fn state(&self, route: &str) -> Option<&RouteState> {
        self.routes.get(route)
    }

    /// One line per route that faulted during the run, for the run output.
    pub fn summary_lines(&self) -> Vec<String> {
        self.routes
            .iter()
            .filter(|(_, s)| s.total > 0)
            .map(|(route, s)| {
                let now = if s.in_a_row > 0 {
                    format!("still failing ({} in a row)", s.in_a_row)
                } else {
                    "answering again".to_owned()
                };
                format!(
                    "route `{route}`: {} call(s) failed at the transport, {now}; its contracts \
                     moved to other routes where one could take them; last error: {}",
                    s.total, s.last_error
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fault_takes_a_route_out_for_a_cooldown_that_doubles() {
        let mut h = RouteHealth::new(1_000);
        assert!(!h.is_out("r", 0));
        let s = h.fault("r", "refused", 0);
        assert_eq!((s.in_a_row, s.total, s.out_until_ms), (1, 1, 1_000));
        assert!(h.is_out("r", 999));
        assert!(!h.is_out("r", 1_000), "eligible again: the probe");
        let s = h.fault("r", "refused", 1_000);
        assert_eq!(s.out_until_ms, 3_000, "doubled");
        let s = h.fault("r", "refused", 3_000);
        assert_eq!(s.out_until_ms, 7_000);
        assert!(!h.is_out("other", 0));
    }

    #[test]
    fn the_cooldown_is_capped() {
        let mut h = RouteHealth::new(400_000);
        h.fault("r", "x", 0);
        let s = h.fault("r", "x", 0);
        assert_eq!(s.out_until_ms, MAX_COOLDOWN_MS);
    }

    #[test]
    fn an_answer_brings_a_route_back_and_says_from_how_many_faults() {
        let mut h = RouteHealth::new(1_000);
        assert_eq!(h.success("r"), None, "never faulted");
        h.fault("r", "x", 0);
        h.fault("r", "x", 0);
        assert_eq!(h.success("r"), Some(2));
        assert!(!h.is_out("r", 0));
        assert_eq!(h.success("r"), None, "already healthy");
        assert_eq!(
            h.state("r").map(|s| s.total),
            Some(2),
            "the run total stays"
        );
    }

    #[test]
    fn the_summary_names_each_route_that_faulted() {
        let mut h = RouteHealth::new(1_000);
        h.fault("llama-cpp:m", "error sending request", 0);
        h.success("ok:m");
        let lines = h.summary_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("llama-cpp:m"), "{}", lines[0]);
        assert!(lines[0].contains("1 call(s) failed at the transport"));
        assert!(lines[0].contains("error sending request"));
    }
}
