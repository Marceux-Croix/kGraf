//! Reading the stored runs for the interface: which workers an agent on the graph
//! stands for, and where their transcripts are.
//!
//! An agent on the graph is a lease holder (`kgraf_graph::snapshot`), named by
//! the orchestrator `<run>/<lineage root>@<process nonce>`. A contract's workers are
//! in its run record (`OpenContract::workers`), and each worker's transcript is
//! `<state dir>/runs/<run>/workers/<worker>.jsonl` (`RunFiles`). This module joins
//! the two, from the records alone.

use std::path::{Path, PathBuf};

use kgraf_orchestrator::{ContractStatus, OpenContract, RunStateRecord, StoredRun};

/// A lease holder's parts: the run and the lineage root.
pub fn parse_holder(holder: &str) -> Option<(&str, &str)> {
    let (name, _nonce) = holder.rsplit_once('@').unwrap_or((holder, ""));
    name.split_once('/')
}

/// The records that parsed.
pub fn records(runs: &[StoredRun]) -> impl Iterator<Item = &RunStateRecord> {
    runs.iter().filter_map(|r| r.record.as_ref().ok())
}

/// The open contracts a lease holder works for, dispatched ones first.
pub fn contracts_for_holder<'a>(
    runs: &'a [StoredRun],
    holder: &str,
) -> Vec<(&'a RunStateRecord, &'a OpenContract)> {
    let Some((run, lineage)) = parse_holder(holder) else {
        return Vec::new();
    };
    let mut out: Vec<(&RunStateRecord, &OpenContract)> = records(runs)
        .filter(|r| r.run.as_str() == run)
        .flat_map(|r| {
            r.open_contracts
                .iter()
                .filter(move |c| c.lease_holder() == lineage)
                .map(move |c| (r, c))
        })
        .collect();
    out.sort_by_key(|(_, c)| c.status != ContractStatus::Dispatched);
    out
}

/// The worker `/compact selected` targets for a selected agent: the newest worker
/// of its dispatched contract.
pub fn worker_for_holder(runs: &[StoredRun], holder: &str) -> Option<String> {
    contracts_for_holder(runs, holder)
        .into_iter()
        .find(|(_, c)| c.status == ContractStatus::Dispatched)
        .and_then(|(_, c)| c.workers.last())
        .map(ToString::to_string)
}

/// Where a worker's transcript is (`RunFiles::worker_transcript`).
pub fn transcript_path(state_dir: &Path, run: &str, worker: &str) -> PathBuf {
    state_dir
        .join("runs")
        .join(run)
        .join("workers")
        .join(format!("{worker}.jsonl"))
}

/// The newest record's status line.
pub fn newest_status(runs: &[StoredRun]) -> Option<String> {
    crate::run_watch::newest(runs).map(RunStateRecord::status_line)
}

/// What an agent is doing now, as a short label: the `intent` argument
/// of its last tool call when the call carries one, else the tool and its first
/// string argument. The runtime does not yet add an intent argument to tool
/// schemas, so today the fallback is what shows.
pub fn action_of(turns: &[crate::transcripts::Turn]) -> Option<String> {
    use crate::transcripts::Turn;
    const MAX: usize = 48;
    let (tool, input) = turns.iter().rev().find_map(|t| match t {
        Turn::ToolCall { tool, input, .. } => Some((tool, input)),
        _ => None,
    })?;
    let args: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
    let short = |s: &str| {
        let line = s.lines().next().unwrap_or("");
        if line.chars().count() > MAX {
            format!("{}...", line.chars().take(MAX).collect::<String>())
        } else {
            line.to_owned()
        }
    };
    if let Some(intent) = args.get("intent").and_then(|v| v.as_str()) {
        return Some(short(intent));
    }
    let first = args
        .as_object()
        .and_then(|o| o.values().find_map(|v| v.as_str()))
        .map(short);
    Some(match first {
        Some(arg) => format!("{tool} {arg}"),
        None => tool.clone(),
    })
}

/// Per agent, keyed by holder without its `@nonce` (`<run>/<lineage root>`), what
/// its label adds: its current action and its pending approvals.
pub fn agent_notes(
    runs: &[StoredRun],
    corpus: &crate::transcripts::Corpus,
    asks: &[kgraf_orchestrator::PendingAsk],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for r in records(runs) {
        for c in r
            .open_contracts
            .iter()
            .filter(|c| c.status == ContractStatus::Dispatched)
        {
            let holder = format!("{}/{}", r.run, c.lease_holder());
            let mut parts = Vec::new();
            if let Some(action) = c
                .workers
                .last()
                .and_then(|w| corpus.get(r.run.as_str(), w.as_str()))
                .and_then(|t| action_of(&t.turns))
            {
                parts.push(action);
            }
            let pending = asks
                .iter()
                .filter(|a| c.workers.iter().any(|w| w.as_str() == a.worker))
                .count();
            if pending > 0 {
                parts.push(format!("{pending} approval(s) waiting"));
            }
            if !parts.is_empty() {
                out.insert(holder, parts.join(" | "));
            }
        }
    }
    out
}

/// The largest number of parallel attempts among the shown runs' open
/// contracts, and the contract that has it: a live-agent ceiling below it can
/// never admit that contract.
pub fn max_attempts(runs: &[StoredRun]) -> Option<(usize, String)> {
    shown_runs(runs)
        .into_iter()
        .flat_map(|r| {
            r.open_contracts.iter().map(move |c| {
                (
                    c.attempts as usize,
                    contract_key(r.run.as_str(), &c.contract),
                )
            })
        })
        .max_by_key(|(n, _)| *n)
}

/// A contract's work-node key: its task id, `<run>/<contract>`, which is also the
/// task its leases carry, so the lease overlay and the plan share one square.
pub fn contract_key(run: &str, contract: &str) -> String {
    format!("{run}/{contract}")
}

/// A gate's work-node key.
pub fn gate_key(run: &str, gate: &str) -> String {
    format!("{run}/gate/{gate}")
}

/// The run root node's key: the id of the run's plan in the graph (`run:<id>`),
/// so the square drawn for the root and the plan row the graph holds for it are
/// one thing. A contract's key is `<run>/<contract>`; the two never collide.
pub fn root_key(run: &str) -> String {
    kgraf_orchestrator::work_mirror::plan_id(run)
}

/// The run a root node's key names.
pub fn root_run(key: &str) -> Option<&str> {
    kgraf_orchestrator::work_mirror::run_of_plan(key)
}

/// A run's root node: what the run has done toward landing, from the land
/// record. It is the run's one place to land from (`land_card::root_node_land`).
fn root_node(r: &RunStateRecord, asks: usize) -> kgraf_galaxy::PlanNode {
    use kgraf_galaxy::{PlanNode, WorkState};
    let run = r.run.as_str();
    let verified = r.verified_contracts.len();
    let total = verified + r.open_contracts.len();
    let (state, detail) = if let Some(entry) = &r.land.landing {
        (
            WorkState::InFlight,
            format!("landing into {}", entry.target),
        )
    } else if let Some(entry) = r.land.landed.last() {
        (
            WorkState::Done,
            format!(
                "landed into {} as {}",
                entry.target,
                short_sha(&entry.merge_commit)
            ),
        )
    } else {
        (
            WorkState::Planned,
            format!("not landed; {verified} of {total} contracts verified"),
        )
    };
    let intent = intent_of(&r.original_instruction);
    PlanNode {
        key: root_key(run),
        state,
        label: format!("run {run}: {} ({detail}) | {intent}", state.word()),
        blocked_by: Vec::new(),
        asks,
        parent: None,
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Runs the graph draws: every run marked for resume, and the newest run, so a
/// finished run stays visible until another starts.
pub fn shown_runs(runs: &[StoredRun]) -> Vec<&RunStateRecord> {
    let newest = crate::run_watch::newest(runs).map(|r| r.run.as_str());
    runs.iter()
        .filter(|r| r.active || Some(r.run.as_str()) == newest)
        .filter_map(|r| r.record.as_ref().ok())
        .collect()
}

/// The first line of an instruction, as a short intent label.
pub fn intent_of(instruction: &str) -> String {
    const MAX: usize = 60;
    let line = instruction
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= MAX {
        line.to_owned()
    } else {
        let cut: String = line.chars().take(MAX).collect();
        format!("{cut}...")
    }
}

/// Every work node the shown runs describe, with its derived state (0014): done,
/// in flight, blocked (by a dependency or a gate, as edges), held by the cap,
/// colliding, held for the user, waiting, planned; gates by their own state.
/// `asks` are the approvals pending in the box, counted on their contract's node.
pub fn plan_nodes(
    runs: &[StoredRun],
    asks: &[kgraf_orchestrator::PendingAsk],
    live_agent_ceiling: Option<usize>,
) -> Vec<kgraf_galaxy::PlanNode> {
    use kgraf_galaxy::{BlockKind, PlanNode, WorkState};
    use kgraf_orchestrator::{GateStatus, WaitState};

    let mut out = Vec::new();
    for r in shown_runs(runs) {
        let run = r.run.as_str();
        let order = &r.coordination.user_order;
        let label = |id: &str, state: WorkState, detail: &str, intent: &str| {
            let key = contract_key(run, id);
            let mut text = String::new();
            if let Some(i) = order.iter().position(|c| c == id) {
                text.push_str(&format!("#{} ", i + 1));
            }
            text.push_str(&format!("{id}: {}", state.word()));
            if !detail.is_empty() {
                text.push_str(&format!(" ({detail})"));
            }
            let pending = asks.iter().filter(|a| a.task == key).count();
            if pending > 0 {
                text.push_str(&format!(" | {pending} approval(s) waiting"));
            }
            if !intent.is_empty() {
                text.push_str(&format!(" | {intent}"));
            }
            text
        };
        // `PlanNode::asks` on every node: the pending asks whose task is its key.
        let pending_for = |key: &str| asks.iter().filter(|a| a.task == key).count();
        // `PlanNode::parent` (decision 0068): a handoff child `<id>.hN` folds into the
        // contract whose handoff was split, which stays drawn as a split node.
        let split: std::collections::HashSet<&str> =
            r.split_contracts.iter().map(String::as_str).collect();
        let parent_of = |id: &str| {
            handoff_parent(id)
                .filter(|p| split.contains(p))
                .map(|p| contract_key(run, p))
        };
        for id in &r.verified_contracts {
            let key = contract_key(run, id);
            out.push(PlanNode {
                asks: pending_for(&key),
                parent: parent_of(id),
                key,
                state: WorkState::Done,
                label: label(id, WorkState::Done, "", ""),
                blocked_by: Vec::new(),
            });
        }
        for c in &r.open_contracts {
            let id = c.contract.as_str();
            let unverified: Vec<String> = c
                .depends_on
                .iter()
                .filter(|d| !r.is_verified(d))
                .cloned()
                .collect();
            let dep_edges = |on: &[String]| -> Vec<(String, BlockKind)> {
                on.iter()
                    .map(|d| (contract_key(run, d), BlockKind::Dependency))
                    .collect()
            };
            let (state, blocked_by, detail) = match (&c.status, r.coordination.waits.get(id)) {
                (ContractStatus::Held { .. }, _) | (_, Some(WaitState::Held { .. })) => {
                    (WorkState::HeldForUser, Vec::new(), String::new())
                }
                (_, Some(WaitState::Running)) => (WorkState::InFlight, Vec::new(), String::new()),
                (_, Some(WaitState::Lease { .. })) => {
                    (WorkState::Colliding, Vec::new(), String::new())
                }
                (_, Some(WaitState::Dependency { on })) => (
                    WorkState::Blocked,
                    dep_edges(on),
                    format!("by {}", on.join(", ")),
                ),
                (_, Some(WaitState::Gate { gate })) => (
                    WorkState::Blocked,
                    vec![(gate_key(run, gate), BlockKind::Gate)],
                    format!("by gate {gate}"),
                ),
                (_, Some(WaitState::Capacity { .. })) => {
                    (WorkState::HeldByCap, Vec::new(), String::new())
                }
                (_, Some(WaitState::Readiness { .. })) => {
                    (WorkState::Waiting, Vec::new(), "readiness".to_owned())
                }
                (ContractStatus::Dispatched, _) => (WorkState::InFlight, Vec::new(), String::new()),
                // No dispatch round has looked yet: derive what is already known.
                (ContractStatus::Planned, _) if !unverified.is_empty() => (
                    WorkState::Blocked,
                    dep_edges(&unverified),
                    format!("by {}", unverified.join(", ")),
                ),
                (ContractStatus::Planned, _) => (WorkState::Planned, Vec::new(), String::new()),
            };
            // A contract whose parallel attempts exceed the live-agent ceiling
            // is refused admission on every round, forever. Said on the node,
            // never left as a silent hold.
            let (state, detail) = match live_agent_ceiling {
                Some(ceiling) if c.attempts as usize > ceiling && state != WorkState::InFlight => (
                    WorkState::HeldByCap,
                    format!(
                        "can never fit: {} attempts need {} agents at once, the ceiling is {ceiling}",
                        c.attempts, c.attempts
                    ),
                ),
                _ => (state, detail),
            };
            let key = contract_key(run, id);
            out.push(PlanNode {
                asks: pending_for(&key),
                parent: parent_of(id),
                key,
                state,
                label: label(id, state, &detail, &intent_of(&c.instruction)),
                blocked_by,
            });
        }
        // Split contracts: replaced by their handoff children, kept as the node they
        // fold into (and a split child may itself have been split).
        for id in &r.split_contracts {
            let key = contract_key(run, id);
            if out.iter().any(|n| n.key == key) {
                continue;
            }
            out.push(PlanNode {
                asks: pending_for(&key),
                parent: parent_of(id),
                key,
                state: WorkState::Split,
                label: label(id, WorkState::Split, "", ""),
                blocked_by: Vec::new(),
            });
        }
        for g in &r.coordination.gates {
            let state = match &g.status {
                // An inconclusive gate is judged by the root gate: not yet
                // passed, and holding nothing, so it shows as pending.
                GateStatus::Pending | GateStatus::Inconclusive { .. } => WorkState::GatePending,
                GateStatus::Running { .. } => WorkState::GateRunning,
                GateStatus::Passed { .. } | GateStatus::Waived { .. } => WorkState::GatePassed,
                GateStatus::Failed { .. } | GateStatus::Held { .. } => WorkState::GateHolding,
            };
            let key = gate_key(run, &g.id);
            // A gate folds into the major contract it is attached to (not `root`).
            let major = (g.major != "root").then(|| contract_key(run, &g.major));
            out.push(PlanNode {
                asks: pending_for(&key),
                parent: major,
                key,
                state,
                label: format!("gate {} over {}: {}", g.id, g.major, state.word()),
                blocked_by: Vec::new(),
            });
        }
        // The run root, last: the plan's own node, with the land state.
        out.push(root_node(r, pending_for(&root_key(run))));
    }
    out
}

/// The contract a handoff child `<id>.hN` was split from, if `id` is one.
pub fn handoff_parent(id: &str) -> Option<&str> {
    let (base, n) = id.rsplit_once(".h")?;
    (!base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(base)
}

/// Adds the presumed work nodes (decision 0064) to the plan overlay.
///
/// - A presumed item not yet adopted is drawn on its own key (`work:<node id>`,
///   the key a scheduled work node uses) in a presumed look: done, partly done
///   or remaining, derived from its document's claim and the code. Its label
///   says it is presumed, what the document says, and where they disagree.
/// - A presumed item adopted by an approved plan's item is **one** piece of work
///   with the contract that item became: while a shown run (whose record names
///   that plan) draws the contract, the contract's square carries the presumed
///   node's title and the presumed node is not drawn twice. With no such run
///   shown, it is drawn as planned work, adopted.
/// - A key the runs or the schedules already draw is theirs: a scheduled
///   presumed node shows its schedule.
/// - Components, plan roots and nodes their source no longer lists are not
///   drawn: squares are work, and a gone node is kept in the graph, not shown.
pub fn fold_presumed(
    plan: &mut Vec<kgraf_galaxy::PlanNode>,
    runs: &[StoredRun],
    presumed: &[kgraf_interpret::PresumedView],
) {
    use kgraf_galaxy::{PlanNode, WorkState};
    use kgraf_interpret::PresumedState;

    let mut drawn: std::collections::HashSet<String> = plan.iter().map(|n| n.key.clone()).collect();
    // `PlanNode::parent` (decision 0068): an item folds into its component (a section
    // or phase), which is drawn when at least one of its items is.
    let components: std::collections::HashMap<&str, &kgraf_interpret::PresumedView> = presumed
        .iter()
        .filter(|v| v.kind == kgraf_graph::WorkKind::Component && !v.gone)
        .map(|v| (v.id.as_str(), v))
        .collect();
    let component_of = |v: &kgraf_interpret::PresumedView| {
        v.parent_id
            .as_deref()
            .filter(|p| components.contains_key(p))
            .map(crate::schedule_store::node_key)
    };
    let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for v in presumed {
        if v.kind != kgraf_graph::WorkKind::Item || v.gone {
            continue;
        }
        let key = crate::schedule_store::node_key(&v.id);
        if drawn.contains(&key) {
            continue;
        }
        let title = crate::schedule_store::cut(&v.title);
        if let Some(by) = &v.adopted_by {
            // `<plan>/item/<contract>`: the run started from that plan draws the
            // contract under its own key.
            let taken = by.split_once("/item/").and_then(|(plan_id, contract)| {
                shown_runs(runs)
                    .into_iter()
                    .filter(|r| r.source_plan.as_deref() == Some(plan_id))
                    .map(|r| contract_key(r.run.as_str(), contract))
                    .find(|k| drawn.contains(k))
            });
            if let Some(contract) = taken {
                if let Some(n) = plan.iter_mut().find(|n| n.key == contract) {
                    n.label
                        .push_str(&format!(" | presumed from {}: {title}", v.source));
                    // A seeded plan's contract folds into the component its presumed
                    // item came from, unless it already has a parent.
                    if n.parent.is_none()
                        && let Some(c) = component_of(v)
                    {
                        used.insert(c.clone());
                        n.parent = Some(c);
                    }
                }
                continue;
            }
            plan.push(PlanNode {
                key: key.clone(),
                state: WorkState::Planned,
                label: format!("{title}: {} (adopted by {by})", WorkState::Planned.word()),
                blocked_by: Vec::new(),
                // A presumed node has no worker, so nothing can wait on it.
                asks: 0,
                parent: component_of(v).inspect(|c| {
                    used.insert(c.clone());
                }),
            });
            drawn.insert(key);
            continue;
        }
        let state = match v.state {
            PresumedState::Done => WorkState::PresumedDone,
            PresumedState::Partial => WorkState::PresumedPartial,
            PresumedState::Remaining => WorkState::PresumedRemaining,
            PresumedState::Unknown => WorkState::PresumedUnknown,
        };
        let mut label = format!("{title}: {}", state.word());
        if !v.claim_text.is_empty() {
            label.push_str(&format!(
                " | {} says: {}",
                v.source,
                crate::schedule_store::cut(&v.claim_text)
            ));
        }
        if let Some(d) = &v.detail {
            label.push_str(&format!(" | {}", crate::schedule_store::cut(d)));
        }
        if v.edited {
            label.push_str(" | changed by you; re-reads leave it");
        }
        plan.push(PlanNode {
            key: key.clone(),
            state,
            label,
            blocked_by: Vec::new(),
            // A presumed node has no worker, so nothing can wait on it.
            asks: 0,
            parent: component_of(v).inspect(|c| {
                used.insert(c.clone());
            }),
        });
        drawn.insert(key);
    }
    // The components that hold drawn items, as the nodes those items fold into.
    for key in used {
        if drawn.contains(&key) {
            continue;
        }
        let Some(v) = key.strip_prefix("work:").and_then(|id| components.get(id)) else {
            continue;
        };
        let state = match v.state {
            PresumedState::Done => WorkState::PresumedDone,
            PresumedState::Partial => WorkState::PresumedPartial,
            PresumedState::Remaining => WorkState::PresumedRemaining,
            PresumedState::Unknown => WorkState::PresumedUnknown,
        };
        plan.push(PlanNode {
            key: key.clone(),
            state,
            label: format!(
                "{}: {} (section)",
                crate::schedule_store::cut(&v.title),
                state.word()
            ),
            blocked_by: Vec::new(),
            asks: 0,
            parent: None,
        });
        drawn.insert(key);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kgraf_core::{AgentId, RunId};

    /// Decision 0068: the work hierarchy the galaxy folds. Handoff children under the
    /// split contract (drawn as a split node), a gate under its major contract, a
    /// presumed item under its component (drawn because it has an item), and a seeded
    /// plan's contract under the component of the item it adopted.
    #[test]
    fn plan_nodes_carry_their_parents() {
        use kgraf_galaxy::WorkState;
        use kgraf_interpret::PresumedState;
        use kgraf_orchestrator::{GateNode, GateStatus};
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.source_plan = Some("plan-1".into());
        r.split_contracts = vec!["a".into()];
        r.open_contracts = vec![
            contract("a.h1", ContractStatus::Dispatched, &["w1"]),
            contract("a.h2", ContractStatus::Planned, &[]),
            contract("import", ContractStatus::Planned, &[]),
            contract("b.h1", ContractStatus::Planned, &[]),
        ];
        r.coordination.gates = vec![GateNode {
            id: "ga".into(),
            major: "a".into(),
            covers: vec!["a.h1".into(), "a.h2".into()],
            after: vec![],
            region: vec![],
            tools: vec![],
            status: GateStatus::Pending,
            runs: 0,
            remediations: vec![],
            allowance: 0,
            decisions: vec![],
            ran_on: vec![],
        }];
        let runs = [stored(r)];
        let mut plan = plan_nodes(&runs, &[], None);
        let mut phase = presumed("p/phase", PresumedState::Partial, None);
        phase.kind = kgraf_graph::WorkKind::Component;
        let mut item = presumed("p/rest", PresumedState::Remaining, None);
        item.parent_id = Some("p/phase".into());
        let mut adopted = presumed(
            "p/import",
            PresumedState::Partial,
            Some("plan-1/item/import"),
        );
        adopted.parent_id = Some("p/phase".into());
        let mut lone = presumed("p/lonely", PresumedState::Remaining, None);
        lone.kind = kgraf_graph::WorkKind::Component;
        fold_presumed(&mut plan, &runs, &[phase, item, adopted, lone]);
        let get = |k: &str| {
            plan.iter()
                .find(|n| n.key == k)
                .unwrap_or_else(|| panic!("no {k} in {plan:#?}"))
        };
        assert_eq!(get("r1/a").state, WorkState::Split);
        assert_eq!(get("r1/a").parent, None);
        assert_eq!(get("r1/a.h1").parent.as_deref(), Some("r1/a"));
        assert_eq!(get("r1/a.h2").parent.as_deref(), Some("r1/a"));
        assert_eq!(
            get("r1/b.h1").parent,
            None,
            "b was not split: no parent drawn"
        );
        assert_eq!(get("r1/gate/ga").parent.as_deref(), Some("r1/a"));
        assert_eq!(get("work:p/rest").parent.as_deref(), Some("work:p/phase"));
        assert_eq!(get("work:p/phase").state, WorkState::PresumedPartial);
        assert_eq!(get("r1/import").parent.as_deref(), Some("work:p/phase"));
        assert!(
            !plan.iter().any(|n| n.key == "work:p/lonely"),
            "a component with nothing drawn under it is not drawn"
        );
        assert_eq!(handoff_parent("a.h12"), Some("a"));
        assert_eq!(handoff_parent("a.hx"), None);
        assert_eq!(handoff_parent(".h1"), None);
        assert_eq!(handoff_parent("a"), None);
    }

    /// `PlanNode::asks` on every kind of node the record draws: a verified
    /// contract, a split contract and a gate each count the asks whose task is their
    /// key. Setting any of them to 0 in `plan_nodes` fails this.
    #[test]
    fn asks_reach_verified_split_and_gate_nodes() {
        use kgraf_orchestrator::{GateNode, GateStatus, PendingAsk};
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.verified_contracts = vec!["v".into()];
        r.split_contracts = vec!["s".into()];
        r.open_contracts = vec![contract("s.h1", ContractStatus::Planned, &[])];
        r.coordination.gates = vec![GateNode {
            id: "g".into(),
            major: "root".into(),
            covers: vec!["v".into()],
            after: vec![],
            region: vec![],
            tools: vec![],
            status: GateStatus::Pending,
            runs: 0,
            remediations: vec![],
            allowance: 0,
            decisions: vec![],
            ran_on: vec![],
        }];
        let ask = |id: u64, task: &str| PendingAsk {
            id,
            worker: "w1".into(),
            task: task.into(),
            tool: "shell".into(),
            question: "run it?".into(),
            asked_by: "shell".into(),
            input: "{}".into(),
            raised_at_ms: 0,
        };
        let asks = vec![
            ask(1, "r1/v"),
            ask(2, "r1/s"),
            ask(3, "r1/gate/g"),
            ask(4, "r1/gate/g"),
        ];
        let nodes = plan_nodes(&[stored(r)], &asks, None);
        let asks_of = |k: &str| {
            nodes
                .iter()
                .find(|n| n.key == k)
                .unwrap_or_else(|| panic!("no {k}"))
                .asks
        };
        assert_eq!(asks_of("r1/v"), 1, "verified");
        assert_eq!(asks_of("r1/s"), 1, "split");
        assert_eq!(asks_of("r1/gate/g"), 2, "gate");
        assert_eq!(asks_of("r1/s.h1"), 0);
    }

    /// The masthead bell ([`crate::projects::Signals::needs`]) is the one count. The
    /// stage's pills (decision 0068) count the same items where a node carries them: asks
    /// on contracts and gates, a held contract, a held gate and a failed gate, each once.
    #[test]
    fn the_stage_pills_and_the_bell_count_the_same_items() {
        use crate::projects::{Signals, collect_failures, record_flags};
        use kgraf_galaxy::fold::NoticeKind;
        use kgraf_orchestrator::{GateNode, GateStatus, PendingAsk};
        let gate = |id: &str, status: GateStatus| GateNode {
            id: id.into(),
            major: "root".into(),
            covers: vec!["a".into()],
            after: vec![],
            region: vec![],
            tools: vec![],
            status,
            runs: 0,
            remediations: vec![],
            allowance: 0,
            decisions: vec![],
            ran_on: vec![],
        };
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.open_contracts = vec![
            contract("a", ContractStatus::Planned, &[]),
            contract(
                "h",
                ContractStatus::Held {
                    reason: "your call".into(),
                },
                &[],
            ),
        ];
        r.coordination.gates = vec![
            gate(
                "failed",
                GateStatus::Failed {
                    pinned: None,
                    at_ms: 1,
                    remediation: "fix".into(),
                },
            ),
            gate(
                "held",
                GateStatus::Held {
                    reason: "too many".into(),
                },
            ),
        ];
        let ask = |id: u64, task: &str| PendingAsk {
            id,
            worker: "w1".into(),
            task: task.into(),
            tool: "shell".into(),
            question: "run it?".into(),
            asked_by: "shell".into(),
            input: "{}".into(),
            raised_at_ms: 0,
        };
        let asks = vec![ask(1, "r1/a"), ask(2, "r1/h"), ask(3, "r1/gate/failed")];
        let runs = [stored(r)];

        let stage = plan_nodes(&runs, &asks, None)
            .iter()
            .enumerate()
            .flat_map(|(i, n)| {
                kgraf_galaxy::notices::notices_of(n, i as u32)
                    .filter(|x| matches!(x.kind, NoticeKind::Failed | NoticeKind::NeedsInput))
                    .collect::<Vec<_>>()
            })
            .count();
        let flags = record_flags(&runs);
        let bell = Signals {
            approvals: asks.len(),
            held: flags.held,
            failures: collect_failures(None, None, None, &[], &runs, &[]).len(),
            ..Signals::default()
        }
        .needs();
        // 3 asks, the held contract, the held gate, the failed gate.
        assert_eq!(bell, 6);
        assert_eq!(stage, bell, "the pills and the bell agree");
    }

    pub(crate) fn stored(record: RunStateRecord) -> StoredRun {
        StoredRun {
            run: record.run.to_string(),
            active: true,
            record: Ok(record),
        }
    }

    pub(crate) fn contract(id: &str, status: ContractStatus, workers: &[&str]) -> OpenContract {
        let mut c = OpenContract::planned(id, id.into(), format!("make {id}"), "tests pass");
        c.status = status;
        c.workers = workers.iter().map(|w| AgentId::new(*w)).collect();
        c
    }

    fn presumed(
        id: &str,
        state: kgraf_interpret::PresumedState,
        adopted_by: Option<&str>,
    ) -> kgraf_interpret::PresumedView {
        kgraf_interpret::PresumedView {
            id: id.into(),
            plan_id: "presumed/roadmap/ROADMAP.md".into(),
            kind: kgraf_graph::WorkKind::Item,
            parent_id: None,
            title: format!("title {id}"),
            source_kind: "roadmap".into(),
            source: "ROADMAP.md".into(),
            section: "Phases > row 1".into(),
            parser: "kgraf roadmap-md/1".into(),
            claim: kgraf_interpret::Claim::Unstated,
            claim_text: "IN PROGRESS".into(),
            state,
            present: Vec::new(),
            missing: Vec::new(),
            detail: None,
            adopted_by: adopted_by.map(str::to_owned),
            edited: false,
            gone: false,
        }
    }

    /// Decision 0064: presumed nodes are drawn in their own looks, labeled as
    /// presumed; an adopted one is the contract's square while a run draws it.
    #[test]
    fn presumed_nodes_join_the_plan_and_an_adopted_one_is_its_contract() {
        use kgraf_galaxy::{PlanNode, WorkState};
        use kgraf_interpret::PresumedState;
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.source_plan = Some("plan-1".into());
        r.open_contracts = vec![contract("import", ContractStatus::Dispatched, &["w1"])];
        let runs = [stored(r)];
        let mut plan = plan_nodes(&runs, &[], None);
        // A scheduled node on a presumed node's key keeps its schedule look.
        plan.push(PlanNode {
            key: "work:p/sched".into(),
            state: WorkState::Waiting,
            label: "sched".into(),
            blocked_by: Vec::new(),
            asks: 0,
            parent: None,
        });
        let mut gone = presumed("p/gone", PresumedState::Remaining, None);
        gone.gone = true;
        let mut component = presumed("p/c", PresumedState::Partial, None);
        component.kind = kgraf_graph::WorkKind::Component;
        let views = [
            presumed("p/done", PresumedState::Done, None),
            presumed("p/part", PresumedState::Partial, None),
            presumed("p/rest", PresumedState::Remaining, None),
            presumed("p/sched", PresumedState::Remaining, None),
            presumed(
                "p/import",
                PresumedState::Partial,
                Some("plan-1/item/import"),
            ),
            presumed(
                "p/later",
                PresumedState::Remaining,
                Some("plan-2/item/later"),
            ),
            gone,
            component,
        ];
        fold_presumed(&mut plan, &runs, &views);
        let state = |k: &str| {
            plan.iter()
                .find(|n| n.key == k)
                .map(|n| n.state)
                .unwrap_or_else(|| panic!("no {k} in {plan:#?}"))
        };
        assert_eq!(state("work:p/done"), WorkState::PresumedDone);
        assert_eq!(state("work:p/part"), WorkState::PresumedPartial);
        assert_eq!(state("work:p/rest"), WorkState::PresumedRemaining);
        assert_eq!(state("work:p/sched"), WorkState::Waiting, "the schedule's");
        assert_eq!(
            state("work:p/later"),
            WorkState::Planned,
            "adopted, no run yet"
        );
        assert!(
            !plan.iter().any(|n| n.key == "work:p/import"),
            "one square for the adopted node and its running contract"
        );
        let contract = plan
            .iter()
            .find(|n| n.key == "r1/import")
            .expect("contract");
        assert_eq!(contract.state, WorkState::InFlight);
        assert!(
            contract
                .label
                .contains("presumed from ROADMAP.md: title p/import"),
            "{}",
            contract.label
        );
        assert!(
            !plan
                .iter()
                .any(|n| n.key == "work:p/gone" || n.key == "work:p/c")
        );
        let part = plan.iter().find(|n| n.key == "work:p/part").expect("part");
        assert!(
            part.label
                .starts_with("title p/part: presumed partly done | ROADMAP.md says: IN PROGRESS"),
            "{}",
            part.label
        );
        assert_eq!(plan.iter().filter(|n| n.key == "work:p/sched").count(), 1);
    }

    /// Every derived state the record can hold reaches the graph as its own state,
    /// with blocking as edges to the cause and the user's order on the label.
    #[test]
    fn the_record_becomes_work_nodes_with_derived_state() {
        use kgraf_galaxy::{BlockKind, WorkState};
        use kgraf_orchestrator::{GateNode, GateStatus, PendingAsk, WaitState};
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.verified_contracts = vec!["v".into()];
        let mut b = contract("b", ContractStatus::Planned, &[]);
        b.depends_on = vec!["a".into()];
        r.open_contracts = vec![
            contract("a", ContractStatus::Dispatched, &["w1"]),
            b,
            contract("c", ContractStatus::Planned, &[]),
            contract("d", ContractStatus::Planned, &[]),
            contract("e", ContractStatus::Planned, &[]),
            contract(
                "h",
                ContractStatus::Held {
                    reason: "retries exhausted".into(),
                },
                &[],
            ),
        ];
        let waits = &mut r.coordination.waits;
        waits.insert("a".into(), WaitState::Running);
        waits.insert(
            "b".into(),
            WaitState::Dependency {
                on: vec!["a".into()],
            },
        );
        waits.insert(
            "c".into(),
            WaitState::Capacity {
                detail: "cap 1".into(),
            },
        );
        waits.insert(
            "d".into(),
            WaitState::Lease {
                detail: "src/x.rs held".into(),
            },
        );
        waits.insert("e".into(), WaitState::Gate { gate: "g".into() });
        r.coordination.gates = vec![GateNode {
            id: "g".into(),
            major: "root".into(),
            covers: vec!["v".into()],
            after: vec![],
            region: vec![],
            tools: vec![],
            status: GateStatus::Held {
                reason: "failed twice".into(),
            },
            runs: 2,
            remediations: vec![],
            allowance: 0,
            decisions: vec![],
            ran_on: vec![],
        }];
        r.coordination.user_order = vec!["c".into()];
        let asks = vec![PendingAsk {
            id: 1,
            worker: "w1".into(),
            task: "r1/a".into(),
            tool: "shell".into(),
            question: "run it?".into(),
            asked_by: "shell".into(),
            input: "{}".into(),
            raised_at_ms: 0,
        }];
        let nodes = plan_nodes(&[stored(r)], &asks, None);
        let get = |k: &str| {
            nodes
                .iter()
                .find(|n| n.key == k)
                .unwrap_or_else(|| panic!("{k} in {nodes:?}"))
        };
        assert_eq!(get("r1/v").state, WorkState::Done);
        assert_eq!(get("r1/a").state, WorkState::InFlight);
        assert!(get("r1/a").label.contains("1 approval(s) waiting"));
        // The count reaches the galaxy's notices as data, not only as label text.
        assert_eq!(get("r1/a").asks, 1);
        assert_eq!(get("r1/b").asks, 0);
        assert_eq!(get("r1/v").asks, 0);
        assert_eq!(get("r1/b").state, WorkState::Blocked);
        assert_eq!(
            get("r1/b").blocked_by,
            [("r1/a".to_owned(), BlockKind::Dependency)]
        );
        assert_eq!(get("r1/c").state, WorkState::HeldByCap);
        assert!(
            get("r1/c")
                .label
                .starts_with("#1 c: held by the concurrency cap")
        );
        assert_eq!(get("r1/d").state, WorkState::Colliding);
        assert_eq!(get("r1/e").state, WorkState::Blocked);
        assert_eq!(
            get("r1/e").blocked_by,
            [("r1/gate/g".to_owned(), BlockKind::Gate)]
        );
        assert_eq!(get("r1/gate/g").state, WorkState::GateHolding);
        assert_eq!(get("r1/h").state, WorkState::HeldForUser);
        // Blocked, capped and colliding are three different states on the graph.
        let states: std::collections::HashSet<_> = ["r1/b", "r1/c", "r1/d"]
            .iter()
            .map(|k| get(k).state)
            .collect();
        assert_eq!(states.len(), 3);
    }

    #[test]
    fn an_agents_label_carries_its_action_and_pending_approvals() {
        use crate::transcripts::{Corpus, Turn, turns};
        let call = |tool: &str, input: serde_json::Value| Turn::ToolCall {
            call_id: "c".into(),
            tool: tool.into(),
            input: serde_json::to_string_pretty(&input).expect("json"),
        };
        // The intent argument wins when a call carries one.
        assert_eq!(
            action_of(&[call(
                "shell",
                serde_json::json!({"intent": "run the parser tests", "command": ["cargo", "test"]})
            )])
            .as_deref(),
            Some("run the parser tests")
        );
        assert_eq!(
            action_of(&[
                Turn::Assistant("thinking".into()),
                call("read_file", serde_json::json!({"path": "src/lib.rs"}))
            ])
            .as_deref(),
            Some("read_file src/lib.rs")
        );
        assert_eq!(action_of(&[Turn::Assistant("no tools".into())]), None);

        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.open_contracts = vec![contract("a", ContractStatus::Dispatched, &["r1-a-r0-a0"])];
        let mut corpus = Corpus::default();
        let mut t = Vec::new();
        turns(
            &format!(
                "{}\n",
                serde_json::json!({"seq": 0, "event": {"type": "tool_call_started",
                    "call_id": "t1", "tool": "apply_patch", "input": {"patch": "*** Begin Patch"}}})
            ),
            1,
            &mut t,
        );
        let path = std::path::PathBuf::from("w.jsonl");
        corpus.transcripts.insert(
            ("r1".into(), "r1-a-r0-a0".into()),
            std::sync::Arc::new(crate::transcripts::Transcript::for_test(
                "r1",
                "r1-a-r0-a0",
                path,
                t,
            )),
        );
        let asks = vec![kgraf_orchestrator::PendingAsk {
            id: 1,
            worker: "r1-a-r0-a0".into(),
            task: "r1/a".into(),
            tool: "shell".into(),
            question: "run it?".into(),
            asked_by: "shell".into(),
            input: "{}".into(),
            raised_at_ms: 0,
        }];
        let notes = agent_notes(&[stored(r)], &corpus, &asks);
        assert_eq!(
            notes.get("r1/a").map(String::as_str),
            Some("apply_patch *** Begin Patch | 1 approval(s) waiting")
        );
    }

    #[test]
    fn a_holder_names_its_run_and_lineage_and_resolves_to_the_running_worker() {
        assert_eq!(parse_holder("r1/a@n9"), Some(("r1", "a")));
        assert_eq!(parse_holder("no-slash@n"), None);
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.open_contracts = vec![
            contract(
                "a",
                ContractStatus::Dispatched,
                &["r1-a-r0-a0", "r1-a-r0-a1"],
            ),
            contract("b", ContractStatus::Planned, &[]),
        ];
        let runs = vec![stored(r)];
        assert_eq!(
            worker_for_holder(&runs, "r1/a@n9").as_deref(),
            Some("r1-a-r0-a1")
        );
        assert_eq!(worker_for_holder(&runs, "r1/b@n9"), None, "not dispatched");
        assert_eq!(worker_for_holder(&runs, "r2/a@n9"), None, "other run");
        assert_eq!(
            transcript_path(Path::new("/p/.kgraf"), "r1", "w"),
            Path::new("/p/.kgraf/runs/r1/workers/w.jsonl")
        );
    }

    fn landed_entry() -> kgraf_orchestrator::land_record::LandingEntry {
        kgraf_orchestrator::land_record::LandingEntry {
            run_branch: "kgraf/r1".into(),
            candidate: "a".repeat(40),
            merge_commit: "b".repeat(40),
            target: "main".into(),
            target_old: "c".repeat(40),
            gate: kgraf_orchestrator::land_record::LandGate::Passed {
                candidate: "a".repeat(40),
                tree: "d".repeat(40),
            },
            started_ms: 1,
            target_moved: true,
            checkout_interrupted: false,
            include_changes: Vec::new(),
            programs_digest: None,
            fold: None,
            finished_ms: Some(2),
        }
    }

    /// Every shown run has a root node keyed by its plan id in the graph, with
    /// its land state from the land record: not landed, landing, landed.
    #[test]
    fn a_run_has_a_root_node_with_its_land_state() {
        use kgraf_galaxy::WorkState;
        let mut r = RunStateRecord::new(RunId::new("r1"), "make the thing\nfast", 1);
        r.verified_contracts = vec!["v".into()];
        r.open_contracts = vec![contract("a", ContractStatus::Planned, &[])];
        let root_of = |r: &RunStateRecord| {
            let nodes = plan_nodes(&[stored(r.clone())], &[], None);
            let root: Vec<_> = nodes
                .iter()
                .filter(|n| n.key == "run:r1")
                .cloned()
                .collect();
            assert_eq!(root.len(), 1, "one root: {nodes:#?}");
            assert_eq!(nodes.len(), 3, "the run's two contracts and its root");
            root[0].clone()
        };
        let root = root_of(&r);
        assert_eq!(root.state, WorkState::Planned);
        assert!(
            root.label.contains("not landed; 1 of 2 contracts verified"),
            "{}",
            root.label
        );
        assert!(root.label.contains("make the thing"), "{}", root.label);
        assert_eq!(root_run(&root.key), Some("r1"));

        r.land.landing = Some(landed_entry());
        let root = root_of(&r);
        assert_eq!(root.state, WorkState::InFlight);
        assert!(root.label.contains("landing into main"), "{}", root.label);

        r.land.landing = None;
        r.land.landed = vec![landed_entry()];
        let root = root_of(&r);
        assert_eq!(root.state, WorkState::Done);
        assert!(
            root.label
                .contains(&format!("landed into main as {}", "b".repeat(12))),
            "{}",
            root.label
        );
    }

    /// A run and its mirror in the graph draw each square once: the contracts
    /// on `<run>/<contract>` and the root on `run:<run>`, and no key of the
    /// mirror's item rows (`run:<run>/item/<contract>`). The mirror rows are
    /// queryable data, not drawn again.
    #[test]
    fn a_run_and_its_mirror_draw_no_duplicate_squares() {
        let dir = tempfile::tempdir().expect("dir");
        let graph = std::sync::Arc::new(std::sync::Mutex::new(
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph"),
        ));
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.verified_contracts = vec!["v".into()];
        r.plan_specs.insert(
            "v".into(),
            serde_json::from_str::<kgraf_orchestrator::ContractSpec>(
                r#"{"id":"v","instruction":"do v","acceptance":"ok"}"#,
            )
            .expect("spec"),
        );
        let mut b = contract("b", ContractStatus::Planned, &[]);
        b.depends_on = vec!["a".into()];
        r.open_contracts = vec![contract("a", ContractStatus::Dispatched, &["w1"]), b];
        kgraf_orchestrator::RunStore::new(graph.clone())
            .save(&r)
            .expect("save");
        // The mirror is there, as rows.
        let g = graph.lock().expect("lock");
        let mirrored = g.work_nodes("run:r1").expect("nodes");
        assert_eq!(mirrored.len(), 4, "the root and three items: {mirrored:#?}");
        // The same read the window makes.
        let runs = kgraf_orchestrator::read_runs(&g).expect("runs");
        let presumed = kgraf_interpret::presumed::views(&g).expect("presumed");
        let schedules = crate::schedule_store::views(&g).expect("schedules");
        drop(g);
        let mut plan = plan_nodes(&runs, &[], None);
        plan.extend(crate::schedule_store::plan_nodes(&schedules, &|_: &str| {
            None
        }));
        fold_presumed(&mut plan, &runs, &presumed);
        let mut keys: Vec<&str> = plan.iter().map(|n| n.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["r1/a", "r1/b", "r1/v", "run:r1"], "{plan:#?}");
        assert!(
            plan.iter().all(|n| !n.key.contains("/item/")),
            "no mirror row is drawn: {plan:#?}"
        );
    }
}
