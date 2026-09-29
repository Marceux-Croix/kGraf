//! Delegation guards: what must hold before a contract may spawn a child contract.
//!
//! Ported from zhayujie/CowAgent `agent/tools/agent_delegate/agent_delegate.py`
//! (`DelegationPolicy` and `AgentDelegateTool._delegate`), commit
//! 01a89d1ce8ce08c6651f5b06aa465d68fd7f3b72, MIT (license read at the source
//! 2026-09-26; see `third_party/cowagent/UPSTREAM`).
//!
//! The six guards, in upstream's order:
//!
//! 1. the task is present and within `max_task_chars` (upstream default 8,000);
//! 2. the target is on the roster, not merely allowed by policy;
//! 3. the delegation trace ends with the source;
//! 4. a target already in the trace is a cycle and is rejected;
//! 5. the source-to-target pair is allowed by the policy table;
//! 6. the depth does not exceed `max_depth` (upstream default 3, range 1 to 8;
//!    here 1 to 16, decision 0067).
//!
//! Upstream's `@` prefix strip on the target is kept (a model often copies the
//! roster's `@id` form).
//!
//! **What changed in the port, and why.** CowAgent's agents are standing
//! identities, so its trace is a list of agents and a cycle is A to B to A. In
//! kGraf the delegating unit is a contract, and a handoff's remainder is
//! re-split onto the same route more often than not, so "the same agent again" is
//! the normal case, not a loop. The trace here is the contract lineage, the
//! roster is the set of configured routes, and a cycle is a child whose task is
//! the same work as an ancestor's (compared after whitespace and case
//! normalization): the work came back around, which is the loop the guard exists
//! to stop. Upstream's policy default (`source != target`) becomes "every
//! configured pair", for the same reason. Upstream's timeout is not ported; a
//! worker's run is bounded by its step budget and the admission limits instead.

use std::collections::BTreeMap;

/// Upstream's default maximum depth.
pub const DEFAULT_MAX_DEPTH: u32 = 3;
/// The deepest a policy may allow. Upstream stops at 8; decision 0067 derives
/// the depth from the parallelism slider (`ParallelismLimits::derive`, which
/// clamps to 3..=16), so the range here widens to match what it can produce.
pub const MAX_POLICY_DEPTH: u32 = 16;
/// Upstream's default maximum task length, in characters.
pub const DEFAULT_MAX_TASK_CHARS: usize = 8_000;

/// Which routes may delegate to which, and how deep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationPolicy {
    /// Source route to allowed target routes; `"*"` allows any. `None` allows
    /// every configured pair.
    allowed: Option<BTreeMap<String, Vec<String>>>,
    max_depth: u32,
    max_task_chars: usize,
}

/// A policy value outside the ranges upstream validates.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("max_depth {0} is outside 1..=16")]
    Depth(u32),
    #[error("max_task_chars {0} is outside 1..=100000")]
    TaskChars(usize),
}

impl Default for DelegationPolicy {
    fn default() -> Self {
        Self {
            allowed: None,
            max_depth: DEFAULT_MAX_DEPTH,
            max_task_chars: DEFAULT_MAX_TASK_CHARS,
        }
    }
}

impl DelegationPolicy {
    /// A policy with upstream's range checks.
    pub fn new(
        allowed: Option<BTreeMap<String, Vec<String>>>,
        max_depth: u32,
        max_task_chars: usize,
    ) -> Result<Self, PolicyError> {
        if !(1..=MAX_POLICY_DEPTH).contains(&max_depth) {
            return Err(PolicyError::Depth(max_depth));
        }
        if !(1..=100_000).contains(&max_task_chars) {
            return Err(PolicyError::TaskChars(max_task_chars));
        }
        Ok(Self {
            allowed,
            max_depth,
            max_task_chars,
        })
    }

    pub fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// This policy with its depth taken from the parallelism limits (decision
    /// 0067, point 8: the handoff depth and the subagent depth are no longer
    /// constants). The routing table and the task bound are kept. The derived
    /// depth is already within 3..=16; it is clamped again so a hand-built
    /// `ParallelismLimits` can never produce a policy `new` would refuse.
    pub fn with_limits(&self, limits: &kgraf_core::parallel::ParallelismLimits) -> Self {
        Self {
            max_depth: limits.max_depth.clamp(1, MAX_POLICY_DEPTH),
            ..self.clone()
        }
    }

    /// Whether `source` may delegate to `target`.
    pub fn allows(&self, source: &str, target: &str) -> bool {
        match &self.allowed {
            None => true,
            Some(table) => table
                .get(source)
                .is_some_and(|targets| targets.iter().any(|t| t == "*" || t == target)),
        }
    }
}

/// One proposed delegation.
#[derive(Clone, Debug)]
pub struct DelegationRequest<'a> {
    /// The contract that delegates.
    pub source_contract: &'a str,
    /// The route the source ran on.
    pub source_route: &'a str,
    /// The route asked for; a leading `@` is stripped.
    pub target_route: &'a str,
    /// The child's task.
    pub task: &'a str,
    /// Contract ids from the root to the source, inclusive.
    pub trace: &'a [String],
    /// The tasks of every contract in `trace`, in the same order.
    pub trace_tasks: &'a [String],
}

/// A delegation that passed every guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delegation {
    pub target_route: String,
    /// The child's depth: 1 for a child of a root contract.
    pub depth: u32,
}

/// Why a delegation was refused. Each names what it checked, so the refusal can
/// be shown to the model that asked and corrected in one step (upstream's intent).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuardRefusal {
    #[error("a delegation needs a target and a task")]
    Empty,
    #[error("the delegated task is {len} characters; the limit is {max}")]
    TaskTooLong { len: usize, max: usize },
    #[error("`{target}` is not a configured route; the routes are {roster:?}")]
    NotInRoster { target: String, roster: Vec<String> },
    #[error("the delegation trace does not end with the source `{source_contract}`")]
    TraceMismatch { source_contract: String },
    #[error("delegation cycle rejected: {path}")]
    Cycle { path: String },
    #[error("route `{source_route}` may not delegate to `{target}`")]
    NotAllowed {
        source_route: String,
        target: String,
    },
    #[error("delegation depth {depth} exceeds the maximum {max}")]
    TooDeep { depth: u32, max: u32 },
}

/// Whitespace- and case-insensitive form of a task, for the cycle guard.
pub fn normalize_task(task: &str) -> String {
    task.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run the six guards.
pub fn check(
    policy: &DelegationPolicy,
    roster: &[String],
    request: &DelegationRequest<'_>,
) -> Result<Delegation, GuardRefusal> {
    let target = request.target_route.trim().trim_start_matches('@');
    let task = request.task.trim();
    // Guard 1.
    if target.is_empty() || task.is_empty() {
        return Err(GuardRefusal::Empty);
    }
    let len = task.chars().count();
    if len > policy.max_task_chars {
        return Err(GuardRefusal::TaskTooLong {
            len,
            max: policy.max_task_chars,
        });
    }
    // Guard 2.
    if !roster.iter().any(|r| r == target) {
        return Err(GuardRefusal::NotInRoster {
            target: target.to_owned(),
            roster: roster.to_vec(),
        });
    }
    // Guard 3.
    if request.trace.last().map(String::as_str) != Some(request.source_contract)
        || request.trace.len() != request.trace_tasks.len()
    {
        return Err(GuardRefusal::TraceMismatch {
            source_contract: request.source_contract.to_owned(),
        });
    }
    // Guard 4. Compared in the stored form (decision 0071, H2): the trace's
    // tasks are stored persist-scrubbed and `task` arrives raw, so a secret
    // in it would otherwise hide a cycle. Scrubbing a stored task again
    // leaves it unchanged, and intake tags (review 4, M-2) are dropped so two
    // intakes of one task compare equal.
    let stored_form = |t: &str| {
        normalize_task(&kgraf_secrets::without_tags(&kgraf_secrets::scrub_persist(
            t,
        )))
    };
    let normalized = stored_form(task);
    if let Some(i) = request
        .trace_tasks
        .iter()
        .position(|t| stored_form(t) == normalized)
    {
        let mut path: Vec<&str> = request.trace.iter().map(String::as_str).collect();
        path.push("(same task as ");
        let path = format!("{} {})", path.join(" -> "), request.trace[i]);
        return Err(GuardRefusal::Cycle { path });
    }
    // Guard 5.
    if !policy.allows(request.source_route, target) {
        return Err(GuardRefusal::NotAllowed {
            source_route: request.source_route.to_owned(),
            target: target.to_owned(),
        });
    }
    // Guard 6. The trace holds the root, so a child of the root is depth 1.
    let depth = request.trace.len() as u32;
    if depth > policy.max_depth {
        return Err(GuardRefusal::TooDeep {
            depth,
            max: policy.max_depth,
        });
    }
    Ok(Delegation {
        target_route: target.to_owned(),
        depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<String> {
        vec!["kimi".into(), "anthropic".into()]
    }

    fn trace(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    fn request<'a>(
        target: &'a str,
        task: &'a str,
        t: &'a [String],
        tasks: &'a [String],
    ) -> DelegationRequest<'a> {
        DelegationRequest {
            source_contract: t.last().map(String::as_str).unwrap_or(""),
            source_route: "kimi",
            target_route: target,
            task,
            trace: t,
            trace_tasks: tasks,
        }
    }

    /// Decision 0071, H2: the trace's tasks are stored scrubbed, and a
    /// handoff item arrives raw. A secret in the task must not hide a cycle.
    #[test]
    fn a_cycle_is_found_when_the_task_holds_a_secret() {
        let token = format!("ghp_{}", "Cy1Cl2Eg3Ua4Rd5Tk6Sc7Rt8Hi9De0Ab1Cd2");
        let raw = format!("guard cycle: rotate {token}");
        let t = trace(&["c1"]);
        let tasks = vec![kgraf_secrets::intake(&raw)];
        assert!(matches!(
            check(
                &DelegationPolicy::default(),
                &roster(),
                &request("kimi", &raw, &t, &tasks)
            ),
            Err(GuardRefusal::Cycle { .. })
        ));
    }

    #[test]
    fn a_valid_delegation_passes_and_reports_its_depth() {
        let t = trace(&["c1"]);
        let tasks = trace(&["port the parser"]);
        let ok = check(
            &DelegationPolicy::default(),
            &roster(),
            &request("@anthropic", "wire the parser into the CLI", &t, &tasks),
        )
        .expect("allowed");
        assert_eq!(ok.target_route, "anthropic");
        assert_eq!(ok.depth, 1);
    }

    #[test]
    fn each_guard_refuses_what_it_checks() {
        let policy = DelegationPolicy::default();
        let t = trace(&["c1"]);
        let tasks = trace(&["port the parser"]);

        assert_eq!(
            check(&policy, &roster(), &request("kimi", "  ", &t, &tasks)),
            Err(GuardRefusal::Empty)
        );
        let long = "x".repeat(DEFAULT_MAX_TASK_CHARS + 1);
        assert!(matches!(
            check(&policy, &roster(), &request("kimi", &long, &t, &tasks)),
            Err(GuardRefusal::TaskTooLong { .. })
        ));
        assert!(matches!(
            check(&policy, &roster(), &request("openai", "x", &t, &tasks)),
            Err(GuardRefusal::NotInRoster { .. })
        ));
        let mut bad = request("kimi", "x", &t, &tasks);
        bad.source_contract = "c9";
        assert!(matches!(
            check(&policy, &roster(), &bad),
            Err(GuardRefusal::TraceMismatch { .. })
        ));
        assert!(matches!(
            check(
                &policy,
                &roster(),
                &request("kimi", "Port   the PARSER", &t, &tasks)
            ),
            Err(GuardRefusal::Cycle { .. })
        ));
        let mut table = BTreeMap::new();
        table.insert("kimi".to_owned(), vec!["kimi".to_owned()]);
        let narrow = DelegationPolicy::new(Some(table), 3, 8_000).expect("policy");
        assert!(matches!(
            check(&narrow, &roster(), &request("anthropic", "x", &t, &tasks)),
            Err(GuardRefusal::NotAllowed { .. })
        ));
        assert!(check(&narrow, &roster(), &request("kimi", "x", &t, &tasks)).is_ok());
        let deep = trace(&["c1", "c2", "c3", "c4"]);
        let deep_tasks = trace(&["a", "b", "c", "d"]);
        assert_eq!(
            check(
                &policy,
                &roster(),
                &request("kimi", "e", &deep, &deep_tasks)
            ),
            Err(GuardRefusal::TooDeep { depth: 4, max: 3 })
        );
    }

    #[test]
    fn policy_ranges_are_upstreams() {
        assert_eq!(
            DelegationPolicy::new(None, 0, 10),
            Err(PolicyError::Depth(0))
        );
        // Decision 0067 widened upstream's 1..=8 to 1..=16.
        assert!(DelegationPolicy::new(None, 9, 10).is_ok());
        assert!(DelegationPolicy::new(None, 16, 10).is_ok());
        assert_eq!(
            DelegationPolicy::new(None, 17, 10),
            Err(PolicyError::Depth(17))
        );
        assert_eq!(
            DelegationPolicy::new(None, 3, 0),
            Err(PolicyError::TaskChars(0))
        );
        let mut table = BTreeMap::new();
        table.insert("a".to_owned(), vec!["*".to_owned()]);
        let p = DelegationPolicy::new(Some(table), 8, 100_000).expect("policy");
        assert!(p.allows("a", "anything"));
        assert!(!p.allows("b", "a"));
    }

    /// Decision 0067: at N=8 the depth limit is 4 (`ceil(log2 8) + 1`), so a
    /// fourth hop passes and a fifth is `TooDeep`; the table is kept.
    #[test]
    fn the_depth_follows_the_parallelism_slider_and_the_table_is_kept() {
        let limits = kgraf_core::parallel::ParallelismLimits::derive(8, 8, 1);
        let mut table = BTreeMap::new();
        table.insert("kimi".to_owned(), vec!["kimi".to_owned()]);
        let base = DelegationPolicy::new(Some(table), 3, 8_000).expect("policy");
        let policy = base.with_limits(&limits);
        assert_eq!(policy.max_depth(), 4);
        assert!(!policy.allows("kimi", "anthropic"), "the table is kept");
        let four = trace(&["c1", "c2", "c3", "c4"]);
        let four_tasks = trace(&["a", "b", "c", "d"]);
        assert!(
            check(
                &policy,
                &roster(),
                &request("kimi", "e", &four, &four_tasks)
            )
            .is_ok()
        );
        let five = trace(&["c1", "c2", "c3", "c4", "c5"]);
        let five_tasks = trace(&["a", "b", "c", "d", "e"]);
        assert_eq!(
            check(
                &policy,
                &roster(),
                &request("kimi", "f", &five, &five_tasks)
            ),
            Err(GuardRefusal::TooDeep { depth: 5, max: 4 })
        );
    }
}
