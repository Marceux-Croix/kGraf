//! Limits derived from the parallelism slider (decision 0067, "Derived
//! limits").
//!
//! `kgraf-capacity` computes `max_agents`, `max_writers` and `exec_slots`
//! from the machine and the configured routes' rate limits; that computation
//! depends on measured, per-process state (`sysinfo`, `getrlimit`, the
//! ledger), so it does not belong in this dependency-light crate. What
//! [`ParallelismLimits::derive`] adds is pure: given the slider value `N`
//! (`max_agents`), the handoff depth, the soft-connection ceiling, the
//! soft-children-per-parent ceiling and the soft message rate follow by
//! formula alone. Keeping that formula here, rather than in `kgraf-capacity`,
//! is what lets the other crates depend on one small, already-stable type instead
//! of waiting on the capacity computation.

/// The bounds a run's guards and the shell's slider read from one number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParallelismLimits {
    /// Agent sessions live at once, process-wide (the slider value).
    pub max_agents: u32,
    /// Writer subagents and attempts live at once (free-disk bound).
    pub max_writers: u32,
    /// Shell-tool processes running at once (core and memory bound).
    pub exec_slots: u32,
    /// The deepest a delegation tree may go before `TooDeep`.
    pub max_depth: u32,
    /// Above this many open connections on one session, a soft notice.
    pub soft_connections: u32,
    /// Above this many children under one parent, a soft notice.
    pub soft_children: u32,
    /// Above this many messages per minute on one connection, delivery paces.
    pub soft_messages_per_min: u32,
}

impl ParallelismLimits {
    /// Derive every bound from the slider value `N` (`max_agents`) and the two
    /// bounds `kgraf-capacity` measures independently.
    ///
    /// - depth `= clamp(ceil(log2 N) + 1, 3, 16)`: a tree that covers `N`
    ///   agents with a branching of at least 2 is `log2 N` deep; deeper than
    ///   that at the same size is a chain, the runaway shape the guard exists
    ///   to stop (design rationale, "Derived limits").
    /// - soft connections per session `= max(8, ceil(2 * sqrt N))`.
    /// - soft children per parent `= ceil(sqrt N)`, floored at 1 so a
    ///   single-agent machine still gets a notice on its second child.
    /// - soft message rate: 12 per minute per sender per connection, fixed
    ///   (not derived from `N`; the design states it as a constant).
    pub fn derive(max_agents: u32, max_writers: u32, exec_slots: u32) -> Self {
        let n = f64::from(max_agents.max(1));
        let depth = (n.log2().ceil() as i64 + 1).clamp(3, 16) as u32;
        let soft_connections = ((2.0 * n.sqrt()).ceil() as u32).max(8);
        let soft_children = (n.sqrt().ceil() as u32).max(1);
        Self {
            max_agents,
            max_writers,
            exec_slots,
            max_depth: depth,
            soft_connections,
            soft_children,
            soft_messages_per_min: 12,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_follows_log2_n_plus_one_clamped_to_three_and_sixteen() {
        assert_eq!(ParallelismLimits::derive(4, 0, 0).max_depth, 3);
        assert_eq!(ParallelismLimits::derive(64, 0, 0).max_depth, 7);
        assert_eq!(ParallelismLimits::derive(1_000, 0, 0).max_depth, 11);
        // A single agent still gets the floor, not depth 1.
        assert_eq!(ParallelismLimits::derive(1, 0, 0).max_depth, 3);
    }

    #[test]
    fn soft_connections_has_a_floor_of_eight() {
        assert_eq!(ParallelismLimits::derive(1, 0, 0).soft_connections, 8);
        assert_eq!(ParallelismLimits::derive(64, 0, 0).soft_connections, 16);
    }

    #[test]
    fn soft_children_is_ceil_sqrt_n() {
        assert_eq!(ParallelismLimits::derive(9, 0, 0).soft_children, 3);
        assert_eq!(ParallelismLimits::derive(10, 0, 0).soft_children, 4);
    }

    #[test]
    fn the_message_rate_is_fixed_at_twelve_per_minute() {
        assert_eq!(ParallelismLimits::derive(1, 0, 0).soft_messages_per_min, 12);
        assert_eq!(
            ParallelismLimits::derive(1_000, 0, 0).soft_messages_per_min,
            12
        );
    }

    #[test]
    fn max_writers_and_exec_slots_pass_through_unchanged() {
        let limits = ParallelismLimits::derive(10, 3, 5);
        assert_eq!(limits.max_writers, 3);
        assert_eq!(limits.exec_slots, 5);
    }
}
