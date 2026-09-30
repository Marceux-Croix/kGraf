//! The plan overlay: every work node's derived state on the graph (decisions 0014,
//! 0016, 0017, 0018; "see at a glance what is done, what is in flight, and
//! what is blocked and by what, including a gate node holding up the work below").
//!
//! Input is a list of [`PlanNode`]s, derived by the shell from the run records; the
//! orchestrator's record is the one truth (0007), and nothing here stores status.
//! Output is work nodes (cards) in the scene:
//!
//! - **Status on the card's border**, on work nodes only (0014: per-node indicators
//!   are bounded by plan size, never drawn on code nodes), in the stage palette of
//!   `docs/STYLE_GUIDE.md` (see [`WorkState::look`]): orange for failed, yellow for held
//!   for the user, gray for everything else.
//! - **Blocking as a first-class edge** from the waiting node to what holds it, drawn
//!   as a waits-on arrow (STG-DRAW-6). The edge says "by what"; the label says why.
//! - A node held back by the **concurrency cap**, a **blocked** node and a
//!   **colliding** one (waiting on a lease another holder has) are told apart by their
//!   words, as 0017 requires, and all are gray (COL-5).
//!
//! A plan node shares its key with the lease overlay's work item for the same task
//! (`project.rs`), so a running contract is one square, linked to its agents.
//! Positions come from [`crate::project::work_anchor`], a pure function of the key:
//! adding a node never moves another (0015).
//!
//! Written new: the states are this orchestrator's own (`waits.rs`,
//! `gates.rs`); the channel choice follows 0014's rendering section.

use std::collections::{HashMap, HashSet};

use crate::error::GalaxyError;
use crate::palette::Palette;
use crate::project::{EdgeKey, ProjectModel, SceneEdit, WORK_SIZE, work_anchor};
use crate::scene::{EdgeInput, NodeInput, Population};

/// A work node's derived state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkState {
    /// Verified.
    Done,
    /// Workers are running on it.
    InFlight,
    /// Waiting on a dependency or a gate; its edges say which.
    Blocked,
    /// Eligible and admissible, refused by the concurrency cap (0017).
    HeldByCap,
    /// Waiting on a lease another holder has (0016, 0048).
    Colliding,
    /// Held until the user acts (a failed re-grounding, exhausted retries).
    HeldForUser,
    /// Waiting on budget or readiness.
    Waiting,
    /// Planned, not yet looked at by a dispatch round.
    Planned,
    /// A quality gate (0018) waiting for what it covers.
    GatePending,
    /// A gate whose validation runs now.
    GateRunning,
    /// A gate that passed or was waived.
    GatePassed,
    /// A gate that failed or is held for the user: it holds up the work below it.
    GateHolding,
    /// A scheduled node whose schedule the claim table disabled (a missing
    /// credential, too many failures); the label gives the reason (decision 0052).
    ScheduleDisabled,
    /// A scheduled node whose schedule the user turned off in its definition.
    ScheduleOff,
    /// A contract whose handoff was re-split into children (`<id>.hN`), which
    /// replaced it; it stays as the node its children fold into.
    Split,
    /// Presumed from a roadmap or a model reading (decision 0064), no real work
    /// yet: its document says done and nothing it names is missing. A claim,
    /// not a verification.
    PresumedDone,
    /// Presumed; the document says in progress, or some of what it names
    /// exists, or it says done while something it names is missing.
    PresumedPartial,
    /// Presumed; nothing of it exists and the document does not say it started.
    PresumedRemaining,
    /// Presumed, but its stored record is damaged, so neither what it names nor
    /// whether work remains is known. Not counted as remaining.
    PresumedUnknown,
}

impl WorkState {
    /// The status word a label carries.
    pub fn word(self) -> &'static str {
        match self {
            WorkState::Done => "done",
            WorkState::InFlight => "in flight",
            WorkState::Blocked => "blocked",
            WorkState::HeldByCap => "held by the concurrency cap",
            WorkState::Colliding => "waiting on a lease (colliding)",
            WorkState::HeldForUser => "held for you",
            WorkState::Waiting => "waiting",
            WorkState::Planned => "planned",
            WorkState::GatePending => "gate pending",
            WorkState::GateRunning => "gate running",
            WorkState::GatePassed => "gate passed",
            WorkState::GateHolding => "gate holding the work below",
            WorkState::ScheduleDisabled => "schedule disabled",
            WorkState::ScheduleOff => "schedule off",
            WorkState::Split => "split into handoff parts",
            WorkState::PresumedDone => "presumed done",
            WorkState::PresumedPartial => "presumed partly done",
            WorkState::PresumedRemaining => "presumed remaining",
            WorkState::PresumedUnknown => "presumed, state unknown",
        }
    }

    /// Color (sRGB) and size of the square, per the stage palette of
    /// `docs/STYLE_GUIDE.md` 1.4: orange `stage_danger` for failed or broken (COL-7,
    /// STG-DRAW-11), yellow `needs_input` for held for the user (COL-3), and stage gray
    /// for everything else, working and every kind of waiting included (COL-5). Red is
    /// progress only (COL-1) and there is no green on the stage and cyan means only the selection
    /// (COL-6, COL-4), so blocked, the cap hold, colliding and waiting are told apart by the reason
    /// in words on the label, never by hue. Size still separates them a little.
    pub fn look(self, p: &Palette) -> ([f32; 3], f32) {
        match self {
            WorkState::Done | WorkState::GatePassed => (p.state_neutral, WORK_SIZE * 0.8),
            WorkState::InFlight | WorkState::GateRunning => (p.state_neutral, WORK_SIZE * 1.15),
            WorkState::Blocked | WorkState::HeldByCap | WorkState::Colliding => {
                (p.state_neutral, WORK_SIZE)
            }
            WorkState::HeldForUser => (p.state_needs_input, WORK_SIZE * 1.15),
            WorkState::Waiting | WorkState::Planned | WorkState::GatePending => {
                (p.state_neutral, WORK_SIZE * 0.9)
            }
            WorkState::GateHolding => (p.state_danger, WORK_SIZE * 1.4),
            WorkState::ScheduleDisabled => (p.state_danger, WORK_SIZE * 1.15),
            // Dim gray, small: switched off on purpose.
            WorkState::ScheduleOff => (p.state_off, WORK_SIZE * 0.7),
            WorkState::Split => (p.state_neutral, WORK_SIZE),
            // Presumed (0064): smaller than real work and a dim gray, so a plan read
            // from a document never looks like work kGraf ran. The stage has no green
            // or cyan for them (COL-6, COL-4): the three are told apart by their words.
            WorkState::PresumedDone
            | WorkState::PresumedPartial
            | WorkState::PresumedRemaining
            | WorkState::PresumedUnknown => (p.state_presumed, WORK_SIZE * 0.7),
        }
    }

    /// Whether the state is failed or broken (orange on the stage).
    pub fn is_failed(self) -> bool {
        matches!(self, WorkState::GateHolding | WorkState::ScheduleDisabled)
    }
}

/// Why a node waits on another: the blocking edge's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Dependency,
    Gate,
}

impl BlockKind {
    /// A waits-on arrow is thin and neutral (STG-DRAW-6, the palette's relation arrow);
    /// what it waits on is said in words on the label, so both kinds share the color.
    pub fn color(self, p: &Palette) -> [f32; 3] {
        match self {
            BlockKind::Dependency | BlockKind::Gate => p.relation_arrow,
        }
    }
}

/// Intensity of a blocking edge: the evidence's 0.85 for links outside the field.
pub const BLOCK_EDGE: f32 = 0.85;

/// One work node as the run record describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// The key: a contract's task id (shared with the lease overlay), or a gate's.
    pub key: String,
    pub state: WorkState,
    /// The label: id, intent, status word, the user's order, pending approvals.
    pub label: String,
    /// What it waits on, by key, and why.
    pub blocked_by: Vec<(String, BlockKind)>,
    /// How many tool approvals and questions wait for the user on this node now:
    /// the pending asks whose task is this node's `key`. Every constructor must
    /// set it from the pending asks, not leave it 0: each one is a needs-input notice
    /// ([`crate::fold::NoticeKind::NeedsInput`]), counted once on this node's pill and
    /// on the stage (decision 0068). 0 means none is waiting.
    pub asks: usize,
    /// The key of the work node this one folds into (decision 0068): a presumed
    /// item's component, a handoff child's split contract (`<id>.hN` under `<id>`), a
    /// gate's major contract. `None` for a top-level node. A key the plan does not
    /// draw is ignored (the node is then top-level).
    pub parent: Option<String>,
}

impl ProjectModel {
    /// Replaces the plan overlay with `nodes`, changing only what differs. Keys not
    /// in `nodes` leave, unless the lease overlay still draws them.
    pub fn set_plan(
        &mut self,
        target: &mut impl SceneEdit,
        nodes: &[PlanNode],
    ) -> Result<(), GalaxyError> {
        let wanted: HashMap<&str, &PlanNode> = nodes.iter().map(|n| (n.key.as_str(), n)).collect();
        let mut edges_wanted: HashMap<(String, String), BlockKind> = HashMap::new();
        for n in nodes {
            for (on, kind) in &n.blocked_by {
                // An edge to a node the plan does not draw has no endpoint; the
                // label still names it.
                if wanted.contains_key(on.as_str()) {
                    edges_wanted.insert((n.key.clone(), on.clone()), *kind);
                }
            }
        }

        // Edges that leave go first, then nodes.
        let stale: Vec<((String, String), u32)> = self
            .plan_edges
            .iter()
            .filter(|(k, _)| !edges_wanted.contains_key(*k))
            .map(|(k, s)| (k.clone(), *s))
            .collect();
        for (key, slot) in stale {
            self.plan_edges.remove(&key);
            self.edge_keys.remove(&slot);
            target.remove_edge(slot)?;
        }
        let lease_tasks: HashSet<&str> = self.activity.iter().map(|a| a.task.as_str()).collect();
        let gone: Vec<String> = self
            .plan
            .keys()
            .filter(|k| !wanted.contains_key(k.as_str()))
            .cloned()
            .collect();
        for key in gone {
            self.plan.remove(&key);
            if let Some(&slot) = self.tasks.get(&key) {
                self.tree.remove_work(slot);
            }
            if lease_tasks.contains(key.as_str()) {
                // Still a live work item: back to the plain work look.
                if let Some(&slot) = self.tasks.get(&key) {
                    target.restyle_node(slot, self.palette.state_neutral, WORK_SIZE)?;
                }
            } else if let Some(slot) = self.tasks.remove(&key) {
                target.remove_node(slot)?;
            }
        }

        let mut ordered: Vec<&PlanNode> = nodes.iter().collect();
        ordered.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        for n in ordered {
            let (color, size) = n.state.look(&self.palette);
            match self.tasks.get(&n.key).copied() {
                Some(slot) => {
                    let cur = target.scene().nodes()[slot as usize];
                    if cur.color != color || cur.size != size {
                        target.restyle_node(slot, color, size)?;
                    }
                }
                None => {
                    let slot = target.insert_node(
                        NodeInput {
                            position: work_anchor(&n.key),
                            color,
                            size,
                        },
                        Population::Work,
                    )?;
                    self.tasks.insert(n.key.clone(), slot);
                }
            }
            self.plan.insert(n.key.clone(), n.clone());
        }

        let mut edges: Vec<((String, String), BlockKind)> = edges_wanted.into_iter().collect();
        edges.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for ((from, to), kind) in edges {
            let (Some(&s), Some(&t)) = (self.tasks.get(&from), self.tasks.get(&to)) else {
                continue;
            };
            let color = kind.color(&self.palette);
            match self.plan_edges.get(&(from.clone(), to.clone())).copied() {
                Some(slot) => {
                    let e = target.scene().edges()[slot as usize];
                    if e.color != color {
                        target.set_edge_look(slot, color, BLOCK_EDGE)?;
                    }
                }
                None => {
                    let slot = target.insert_edge(EdgeInput {
                        source: s,
                        target: t,
                        color,
                        intensity: BLOCK_EDGE,
                    })?;
                    self.edge_keys
                        .insert(slot, EdgeKey::Plan(from.clone(), to.clone()));
                    self.plan_edges.insert((from, to), slot);
                }
            }
        }
        // The work hierarchy (decision 0068): each node under its parent when the plan
        // draws that parent, else at the top. Sorted, so the result does not depend on
        // the input order.
        let mut placed: Vec<(&str, u32, Option<u32>)> = nodes
            .iter()
            .filter_map(|n| {
                let slot = *self.tasks.get(&n.key)?;
                let parent = n
                    .parent
                    .as_deref()
                    .filter(|p| *p != n.key && wanted.contains_key(p))
                    .and_then(|p| self.tasks.get(p).copied());
                Some((n.key.as_str(), slot, parent))
            })
            .collect();
        placed.sort_unstable();
        // Nodes whose parent changes go to the top first: placing them one by one
        // could otherwise pass through a state that looks like a cycle (a parent order
        // that flips direction between two plans) and be refused for nothing.
        for &(key, slot, parent) in &placed {
            if self.tree.hierarchy().parent(slot) != parent {
                self.tree.detach_work(slot, key);
            }
        }
        for (key, slot, parent) in placed {
            self.tree.set_work(slot, parent, key);
        }
        Ok(())
    }

    /// Gives plan nodes their status look again (after the lease overlay touched
    /// a shared work item).
    pub(crate) fn restyle_plan(&self, target: &mut impl SceneEdit) -> Result<(), GalaxyError> {
        for (key, n) in &self.plan {
            if let Some(&slot) = self.tasks.get(key) {
                let (color, size) = n.state.look(&self.palette);
                let cur = target.scene().nodes()[slot as usize];
                if cur.color != color || cur.size != size {
                    target.restyle_node(slot, color, size)?;
                }
            }
        }
        Ok(())
    }

    /// The plan node at `slot`, if the slot is one.
    pub fn plan_at(&self, slot: u32) -> Option<&PlanNode> {
        let key = self.tasks.iter().find(|(_, s)| **s == slot)?.0;
        self.plan.get(key)
    }

    /// Every plan node with its slot.
    pub fn plan_nodes(&self) -> impl Iterator<Item = (&PlanNode, u32)> {
        self.plan
            .iter()
            .filter_map(|(k, n)| self.tasks.get(k).map(|&s| (n, s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;
    use kgraf_graph::snapshot::GraphSnapshot;

    fn empty() -> (ProjectModel, Scene) {
        ProjectModel::build(&GraphSnapshot::default()).expect("build")
    }

    fn node(key: &str, state: WorkState, blocked_by: &[(&str, BlockKind)]) -> PlanNode {
        PlanNode {
            key: key.into(),
            state,
            label: format!("{key}: {}", state.word()),
            blocked_by: blocked_by
                .iter()
                .map(|(k, b)| ((*k).to_owned(), *b))
                .collect(),
            asks: 0,
            parent: None,
        }
    }

    #[test]
    fn states_are_drawn_distinctly_and_blocking_is_an_edge_to_its_cause() {
        let p = Palette::default();
        let (mut m, mut scene) = empty();
        m.set_plan(
            &mut scene,
            &[
                node("a", WorkState::InFlight, &[]),
                node("b", WorkState::Blocked, &[("a", BlockKind::Dependency)]),
                node("c", WorkState::HeldByCap, &[]),
                node("d", WorkState::Colliding, &[]),
                node("g", WorkState::GateHolding, &[]),
                node("e", WorkState::Blocked, &[("g", BlockKind::Gate)]),
            ],
        )
        .expect("plan");
        assert_eq!(scene.live_node_count(), 6);
        assert_eq!(scene.live_edge_count(), 2);
        let color = |k: &str| scene.nodes()[m.task_slot(k).expect("slot") as usize].color;
        // Blocked, capped and colliding are reported distinctly (0017, 0016) by their
        // words; on the stage all three are gray (COL-5), never orange or red.
        let words: HashSet<&str> = [
            WorkState::Blocked,
            WorkState::HeldByCap,
            WorkState::Colliding,
        ]
        .iter()
        .map(|s| s.word())
        .collect();
        assert_eq!(words.len(), 3);
        for k in ["b", "c", "d"] {
            assert_eq!(color(k), p.state_neutral, "{k}");
        }
        assert_eq!(
            color("g"),
            p.state_danger,
            "a holding gate is failed: orange"
        );
        // The blocking edge runs from the blocked node to what blocks it.
        let b = m.task_slot("b").expect("b");
        let a = m.task_slot("a").expect("a");
        let edge = scene
            .incident_edges(b)
            .iter()
            .map(|&e| scene.edges()[e as usize])
            .find(|e| e.source == b)
            .expect("edge");
        assert_eq!(edge.target, a);
        assert_eq!(edge.color, BlockKind::Dependency.color(&p));
        let e = m.task_slot("e").expect("e");
        let g = m.task_slot("g").expect("g");
        assert!(
            scene
                .incident_edges(g)
                .iter()
                .any(|&x| scene.edges()[x as usize].source == e),
            "the gate holds the work below it, drawn"
        );
    }

    /// The stage palette (style guide 1.4): failed is orange, held for the user is
    /// yellow, and everything else is gray; the three hues are far apart.
    #[test]
    fn states_use_the_stage_palette() {
        let p = Palette::default();
        use WorkState::*;
        for s in [GateHolding, ScheduleDisabled] {
            assert_eq!(s.look(&p).0, p.state_danger, "{s:?}");
            assert!(s.is_failed());
        }
        assert_eq!(HeldForUser.look(&p).0, p.state_needs_input);
        for s in [
            InFlight, Blocked, HeldByCap, Colliding, Waiting, Planned, Done,
        ] {
            assert_eq!(s.look(&p).0, p.state_neutral, "{s:?}");
            assert!(!s.is_failed());
        }
        // Presumed work is dimmer and smaller than any real work (0064).
        for s in [
            PresumedDone,
            PresumedPartial,
            PresumedRemaining,
            PresumedUnknown,
        ] {
            assert_eq!(s.look(&p).0, p.state_presumed, "{s:?}");
            assert!(s.look(&p).1 < Planned.look(&p).1);
            assert!(!s.is_failed());
        }
        assert!(
            p.state_presumed[0] < p.state_neutral[0] - 0.1,
            "presumed is dimmer"
        );
        let d = |a: [f32; 3], b: [f32; 3]| {
            ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
        };
        for (a, b) in [
            (p.state_danger, p.state_needs_input),
            (p.state_danger, p.state_neutral),
            (p.state_needs_input, p.state_neutral),
        ] {
            assert!(d(a, b) > 0.2, "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_change_of_state_restyles_in_place_and_a_new_node_moves_nothing() {
        let p = Palette::default();
        let (mut m, mut scene) = empty();
        m.set_plan(&mut scene, &[node("a", WorkState::HeldByCap, &[])])
            .expect("plan");
        let a = m.task_slot("a").expect("a");
        let before = scene.nodes()[a as usize].position;
        m.set_plan(
            &mut scene,
            &[
                node("a", WorkState::InFlight, &[]),
                node("b", WorkState::Planned, &[]),
            ],
        )
        .expect("plan");
        assert_eq!(m.task_slot("a"), Some(a), "same slot");
        assert_eq!(scene.nodes()[a as usize].position, before, "did not move");
        assert_eq!(
            scene.nodes()[a as usize].color,
            WorkState::InFlight.look(&p).0
        );
        assert_eq!(
            scene.nodes()[a as usize].size,
            WorkState::InFlight.look(&p).1,
            "restyled from the cap hold's size"
        );
        assert_eq!(m.plan_at(a).map(|n| n.state), Some(WorkState::InFlight));
    }

    #[test]
    fn a_node_that_leaves_the_plan_leaves_the_scene_with_its_edges() {
        let (mut m, mut scene) = empty();
        m.set_plan(
            &mut scene,
            &[
                node("a", WorkState::Done, &[]),
                node("b", WorkState::Blocked, &[("a", BlockKind::Dependency)]),
            ],
        )
        .expect("plan");
        m.set_plan(&mut scene, &[node("a", WorkState::Done, &[])])
            .expect("plan");
        assert_eq!(scene.live_node_count(), 1);
        assert_eq!(scene.live_edge_count(), 0);
        assert!(m.task_slot("b").is_none());
        m.set_plan(&mut scene, &[]).expect("plan");
        assert_eq!(scene.live_node_count(), 0);
    }
}
