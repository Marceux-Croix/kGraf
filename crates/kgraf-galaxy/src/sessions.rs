//! Sessions on the galaxy graph (decision 0067): `kgraf_graph::sessions`'s
//! [`SessionGraph`] folded into the project model once the shell reads it off the
//! project's graph store.
//!
//! - **A session with no lease still gets a diamond** (0067's data model: "sessions
//!   without a lease are drawn too"). A session that already holds a lease is the
//!   activity overlay's own node (`project.rs`'s `agents` map, from `set_activity`):
//!   the two populations never draw the same identity twice, only hand off between
//!   each other as a lease comes and goes, which is why this overlay keeps its own
//!   placeholder map rather than writing into `agents` directly.
//! - **`spawned` and `connection`** are drawn as [`crate::project::EdgeKey::Overlay`]
//!   edges between two [`crate::project::End::Agent`] endpoints, the same mechanism
//!   `set_activity`'s agent-to-task and agent-to-node edges use (`project.rs`), so
//!   `ProjectModel::set_palette` recolors them with no code of its own. Only an open
//!   connection is drawn (approval is per connection, so a
//!   `pending`, `refused` or `closed` one must not look like a channel on the graph).
//! - **A subagent folds under its parent** with [`crate::tree::ProjectTree::set_work`],
//!   the same call `plan.rs` places work nodes with, so it joins the fold hierarchy
//!   [`crate::chunks::Chunks`] budgets at [`crate::fold::VISIBLE_BUDGET`] (decision
//!   0068): a run with hundreds of subagents does not multiply the drawn set unfolded
//!   (sessions must be visible in the graph). Every other
//!   session kind (orchestrator, worker, gate) stays out of the hierarchy, unfolded,
//!   as 0068 already rules for agents.
//! - **The soft-limit badge** ("an unusual number of connections or
//!   message volume raises a visible notice ... never refuses or drops a message
//!   silently") is driven from `SessionGraph::paced`: `kgraf-orchestrator`'s
//!   `connections.rs` already decides "over the soft limit" (its `Pacer`, a true
//!   sliding 60 s window per sender and connection) and records it as the delivered
//!   message's own `not_before_ms`, so this reads that fact back rather than tuning a
//!   second, independent count of its own (2026-09-28). Drawn on the actor's
//!   existing state dot (STG-DRAW-7, `badges::ActorState::Paced`, `project_view.rs`) as
//!   a gray ring (COL-5: a state, not a fault): never yellow, because yellow means
//!   "needs your input" and feeds the project dot, hamburger and bell aggregates
//!   (COL-3), and pacing needs nothing from the user.

use std::collections::{HashMap, HashSet};

use kgraf_core::session::{ConnectionState, SessionFacts, SessionKind};
use kgraf_graph::sessions::SessionGraph;

use crate::error::GalaxyError;
use crate::project::{
    AGENT_LIFT, AGENT_SIZE, EdgeKey, End, LIVE_EDGE, ProjectModel, SceneEdit, TRAIL_EDGE,
    work_anchor,
};
use crate::scene::{EdgeInput, NodeInput, Population};

/// Messages on a connection past which its edge draws at full intensity: a rendering
/// choice only (0067's data model, "its intensity follows the message rate"), unrelated
/// to any policy threshold -- the soft-limit badge below is driven from the pacer's own
/// state, not from a count.
const EDGE_INTENSITY_SATURATES_AT: u64 = 20;

/// Whether a stored edge role is a `spawned` edge (`true`) or a `connection` (`false`):
/// the two share endpoint pairs of the same [`End::Agent`] kind, so the role keeps
/// them apart in this overlay's own lookup.
type EdgeRole = bool;
const SPAWNED: EdgeRole = true;
const CONNECTION: EdgeRole = false;
/// An edge's key (role, ordered endpoint pair) and its wanted look.
type WantedEdge = ((EdgeRole, String, String), ([f32; 3], f32));

/// This overlay's own bookkeeping: which scene slot stands for a lease-less session,
/// which slot stands for a `spawned` or `connection` edge, and which sessions carry
/// the soft-limit badge. Kept apart from `ProjectModel`'s private `agents`/`overlay`
/// maps (only `project.rs` may touch those); see the module doc for why the two
/// overlays never draw the same identity.
#[derive(Debug, Default)]
pub struct SessionOverlay {
    /// Session id to its placeholder slot and kind, for a session with no lease now.
    slots: HashMap<String, (u32, SessionKind)>,
    /// `(role, a, b)`, canonically ordered for a connection, to its edge slot.
    edges: HashMap<(EdgeRole, String, String), u32>,
    /// Sessions `SessionGraph::paced` names: the orchestrator's own soft-limit fact.
    soft_limited: HashSet<String>,
    /// Every session's kind, lease-holding or not (a
    /// resolver's actor mark, style guide CMP-AV-6, needs this even while it
    /// holds a lease and so draws through `agents`, not `slots`). Rebuilt
    /// fully each `set_sessions` call, from the same `by_id` that call reads.
    kinds: HashMap<String, SessionKind>,
}

impl SessionOverlay {
    /// The placeholder slot a lease-less session draws at, if it has one now.
    pub fn slot_of(&self, session: &str) -> Option<u32> {
        self.slots.get(session).map(|&(slot, _)| slot)
    }

    /// Every lease-less session's id and placeholder slot, for a caller (`actor_marks`,
    /// `project_view.rs`) that draws every actor's state dot, lease-holding or not.
    pub fn slots(&self) -> impl Iterator<Item = (&str, u32)> {
        self.slots
            .iter()
            .map(|(id, &(slot, _))| (id.as_str(), slot))
    }

    /// Whether `session` carries the soft-limit badge:
    /// `kgraf-orchestrator`'s pacer paced a message to it past its arrival.
    pub fn is_soft_limited(&self, session: &str) -> bool {
        self.soft_limited.contains(session)
    }

    /// Style guide CMP-AV-6: whether `session` is a
    /// merge-conflict resolver, lease-holding or not. `false` for a session
    /// this fold has not seen yet.
    pub fn is_resolver(&self, session: &str) -> bool {
        self.kinds.get(session) == Some(&SessionKind::Resolver)
    }
}

/// Only an approved, still-open connection is drawn (see the module doc).
fn is_open(state: &ConnectionState) -> bool {
    matches!(state, ConnectionState::Open)
}

/// A connection edge's intensity follows its message rate (0067's data model,
/// "its intensity follows the message rate"): it starts at the trail intensity and
/// climbs to the live intensity by [`EDGE_INTENSITY_SATURATES_AT`] messages.
fn connection_intensity(messages: u64) -> f32 {
    let t = (messages as f32 / EDGE_INTENSITY_SATURATES_AT as f32).min(1.0);
    TRAIL_EDGE + (LIVE_EDGE - TRAIL_EDGE) * t
}

/// The canonical, order-independent key for an undirected pair.
fn pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_owned(), b.to_owned())
    } else {
        (b.to_owned(), a.to_owned())
    }
}

impl ProjectModel {
    /// The soft-limit overlay itself, for a caller that wants every badged session.
    pub fn session_overlay(&self) -> &SessionOverlay {
        &self.sessions
    }

    /// Folds `graph` into the model, changing only what differs from the last call:
    /// placeholder slots for lease-less sessions, `spawned` and `connection` overlay
    /// edges, the fold placement of subagents, and the soft-limit badge. See the
    /// module doc for what each of those means and why.
    pub fn set_sessions(
        &mut self,
        target: &mut impl SceneEdit,
        graph: &SessionGraph,
    ) -> Result<(), GalaxyError> {
        let by_id: HashMap<&str, &SessionFacts> =
            graph.nodes.iter().map(|s| (s.id.as_str(), s)).collect();

        // Every session's kind: rebuilt fully, since
        // `by_id` already is and a session never changes kind in place.
        self.sessions.kinds = by_id
            .iter()
            .map(|(id, s)| ((*id).to_owned(), s.kind))
            .collect();

        // --- Placeholders: exactly the sessions in `graph` that hold no lease now. ---
        let want: HashSet<&str> = by_id
            .keys()
            .copied()
            .filter(|id| self.agent_slot(id).is_none())
            .collect();
        let stale: Vec<(String, u32, SessionKind)> = self
            .sessions
            .slots
            .iter()
            .filter(|(id, _)| !want.contains(id.as_str()))
            .map(|(id, &(slot, kind))| (id.clone(), slot, kind))
            .collect();
        for (id, slot, kind) in stale {
            self.sessions.slots.remove(&id);
            if kind == SessionKind::Subagent {
                self.tree.remove_work(slot);
            }
            target.remove_node(slot)?;
        }
        let mut ids: Vec<&str> = want.into_iter().collect();
        ids.sort_unstable();
        for id in ids {
            if !self.sessions.slots.contains_key(id) {
                let [x, y, _] = work_anchor(id);
                let slot = target.insert_node(
                    NodeInput {
                        position: [x, y, AGENT_LIFT],
                        color: self.palette().agent,
                        size: AGENT_SIZE,
                    },
                    Population::Agent,
                )?;
                self.sessions
                    .slots
                    .insert(id.to_owned(), (slot, by_id[id].kind));
            }
        }

        // --- Fold: a subagent sits under its parent while the parent is drawn (0068
        // rules ordinary agents out of the hierarchy; this is the deliberate carve-out
        // the design calls for). ---
        let resolve = |m: &Self, id: &str| m.agent_slot(id).or_else(|| m.sessions.slot_of(id));
        let spawned_parent: HashMap<&str, &str> = graph
            .spawned
            .iter()
            .map(|(p, c)| (c.as_str(), p.as_str()))
            .collect();
        let mut placed: Vec<(&str, u32, Option<u32>)> = by_id
            .values()
            .filter(|s| s.kind == SessionKind::Subagent)
            .filter_map(|s| {
                let id = s.id.as_str();
                let slot = resolve(self, id)?;
                let parent = spawned_parent.get(id).and_then(|p| resolve(self, p));
                Some((id, slot, parent))
            })
            .collect();
        placed.sort_unstable();
        // Parents that changed go to the top first, as `plan.rs` does, so placing the
        // set never passes through a state a cycle check would refuse for nothing.
        for &(id, slot, parent) in &placed {
            if self.hierarchy().parent(slot) != parent {
                self.tree.detach_work(slot, id);
            }
        }
        for (id, slot, parent) in placed {
            self.tree.set_work(slot, parent, id);
        }

        // --- Edges: `spawned` (directed, parent to child) and open `connection`s
        // (undirected). Both key the same overlay mechanism (`End::Agent`, `End::Agent`)
        // that `set_activity` uses for agent-to-task and agent-to-node edges. ---
        let mut want_edges: HashMap<(EdgeRole, String, String), ([f32; 3], f32)> = HashMap::new();
        for (p, c) in &graph.spawned {
            if resolve(self, p.as_str()).is_some() && resolve(self, c.as_str()).is_some() {
                want_edges.insert(
                    (SPAWNED, p.as_str().to_owned(), c.as_str().to_owned()),
                    (self.palette().edge, LIVE_EDGE),
                );
            }
        }
        for c in &graph.connections {
            if !is_open(&c.state) {
                continue;
            }
            if resolve(self, c.a.as_str()).is_some() && resolve(self, c.b.as_str()).is_some() {
                let (a, b) = pair(c.a.as_str(), c.b.as_str());
                want_edges.insert(
                    (CONNECTION, a, b),
                    (self.palette().edge, connection_intensity(c.messages)),
                );
            }
        }
        // The soft-limit badge: `graph.paced` is
        // `kgraf-orchestrator`'s own pacer state (see the module doc), read back, not
        // re-derived from a count of our own.
        self.sessions.soft_limited = graph.paced.iter().map(|s| s.as_str().to_owned()).collect();

        let stale: Vec<((EdgeRole, String, String), u32)> = self
            .sessions
            .edges
            .iter()
            .filter(|(k, _)| !want_edges.contains_key(*k))
            .map(|(k, &s)| (k.clone(), s))
            .collect();
        for (key, slot) in stale {
            self.sessions.edges.remove(&key);
            self.edge_keys.remove(&slot);
            target.remove_edge(slot)?;
        }
        let mut wanted: Vec<WantedEdge> = want_edges.into_iter().collect();
        wanted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for ((role, a, b), (color, intensity)) in wanted {
            // `spawned`'s direction is (parent, child); `connection`'s pair is already
            // canonical, so either order resolves to the same two slots.
            let (Some(s), Some(t)) = (resolve(self, &a), resolve(self, &b)) else {
                continue;
            };
            match self
                .sessions
                .edges
                .get(&(role, a.clone(), b.clone()))
                .copied()
            {
                Some(slot) => {
                    let e = target.scene().edges()[slot as usize];
                    if e.color != color || e.intensity != intensity {
                        target.set_edge_look(slot, color, intensity)?;
                    }
                }
                None => {
                    let slot = target.insert_edge(EdgeInput {
                        source: s,
                        target: t,
                        color,
                        intensity,
                    })?;
                    self.edge_keys.insert(
                        slot,
                        EdgeKey::Overlay(End::Agent(a.clone()), End::Agent(b.clone())),
                    );
                    self.sessions.edges.insert((role, a, b), slot);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;
    use kgraf_core::session::{ConnectionBasis, ConnectionFacts, ReasoningVisibility};
    use kgraf_core::{AgentId, RunId};
    use kgraf_graph::snapshot::GraphSnapshot;

    fn empty() -> (ProjectModel, Scene) {
        ProjectModel::build(&GraphSnapshot::default()).expect("build")
    }

    fn session(id: &str, kind: SessionKind, parent: Option<&str>) -> SessionFacts {
        SessionFacts {
            id: AgentId::new(id),
            run: RunId::new("run-1"),
            kind,
            parent: parent.map(AgentId::new),
            contract: None,
            route: None,
            model: None,
            depth: 0,
            transcript: None,
            worktree: None,
            write_scope: Vec::new(),
            reasoning: ReasoningVisibility::Unknown,
            started_ms: 0,
            addressable: true,
        }
    }

    /// Builds a `SessionGraph` with no paced sessions; `graph_paced` below is the one
    /// case that needs one.
    fn graph(
        nodes: Vec<SessionFacts>,
        spawned: Vec<(AgentId, AgentId)>,
        connections: Vec<ConnectionFacts>,
    ) -> SessionGraph {
        SessionGraph {
            nodes,
            spawned,
            connections,
            paced: std::collections::BTreeSet::new(),
        }
    }

    fn connection(a: &str, b: &str, state: ConnectionState, messages: u64) -> ConnectionFacts {
        ConnectionFacts {
            id: kgraf_core::session::connection_id(&AgentId::new(a), &AgentId::new(b)),
            run: RunId::new("run-1"),
            a: AgentId::new(a),
            b: AgentId::new(b),
            basis: ConnectionBasis::Sibling,
            state,
            decided_by: None,
            decided_ms: None,
            messages,
            last_ms: None,
        }
    }

    #[test]
    fn a_lease_less_subagent_gets_a_slot_and_folds_under_its_parent() {
        let (mut m, mut scene) = empty();
        let graph = graph(
            vec![
                session("run/orch@1", SessionKind::Orchestrator, None),
                session("run/w1@1", SessionKind::Worker, None),
                session("run/w1@1.sub1", SessionKind::Subagent, Some("run/w1@1")),
            ],
            vec![(AgentId::new("run/w1@1"), AgentId::new("run/w1@1.sub1"))],
            Vec::new(),
        );
        m.set_sessions(&mut scene, &graph).expect("set_sessions");
        let sub = m
            .session_overlay()
            .slot_of("run/w1@1.sub1")
            .expect("a lease-less subagent has a slot");
        let parent = m
            .session_overlay()
            .slot_of("run/w1@1")
            .expect("the worker, also lease-less, has a slot");
        assert_eq!(
            m.hierarchy().parent(sub),
            Some(parent),
            "folds under its parent"
        );
        // The orchestrator's own session draws too, but is not folded (0068: agents
        // never fold; the subagent is the one deliberate carve-out).
        let orch = m
            .session_overlay()
            .slot_of("run/orch@1")
            .expect("the orchestrator's own session has a slot");
        assert_eq!(m.hierarchy().parent(orch), None);
    }

    #[test]
    fn an_open_connection_is_an_edge_and_a_refused_one_draws_nothing() {
        let (mut m, mut scene) = empty();
        let graph = graph(
            vec![
                session("run/w1@1", SessionKind::Worker, None),
                session("run/w2@1", SessionKind::Worker, None),
                session("run/w3@1", SessionKind::Worker, None),
            ],
            Vec::new(),
            vec![
                connection("run/w1@1", "run/w2@1", ConnectionState::Open, 1),
                connection(
                    "run/w1@1",
                    "run/w3@1",
                    ConnectionState::Refused {
                        reason: "no rule covers it".into(),
                    },
                    0,
                ),
            ],
        );
        m.set_sessions(&mut scene, &graph).expect("set_sessions");
        let a = m.session_overlay().slot_of("run/w1@1").expect("a");
        let b = m.session_overlay().slot_of("run/w2@1").expect("b");
        let c = m.session_overlay().slot_of("run/w3@1").expect("c");
        assert_eq!(scene.live_edge_count(), 1, "only the open connection draws");
        let edge_between = |x: u32, y: u32| {
            scene.incident_edges(x).iter().any(|&e| {
                scene.edges()[e as usize].source == y || scene.edges()[e as usize].target == y
            })
        };
        assert!(edge_between(a, b), "the open connection is drawn");
        assert!(!edge_between(a, c), "the refused connection draws nothing");
    }

    /// The badge follows `SessionGraph::paced` exactly: `kgraf-orchestrator`'s pacer
    /// fact, not a message or connection count this crate would tune on its own
    /// (2026-09-28).
    #[test]
    fn a_paced_session_is_badged_and_an_unpaced_one_is_not() {
        let (mut m, mut scene) = empty();
        let mut g = graph(
            vec![
                session("run/paced@1", SessionKind::Worker, None),
                session("run/quiet@1", SessionKind::Worker, None),
            ],
            Vec::new(),
            Vec::new(),
        );
        g.paced = std::collections::BTreeSet::from([AgentId::new("run/paced@1")]);
        m.set_sessions(&mut scene, &g).expect("set_sessions");
        assert!(m.session_overlay().is_soft_limited("run/paced@1"));
        assert!(!m.session_overlay().is_soft_limited("run/quiet@1"));
    }

    #[test]
    fn folding_two_thousand_subagents_still_fits_the_visible_budget() {
        use crate::fold::{VISIBLE_BUDGET, auto_expand, resolve};
        use std::collections::HashMap as Map;

        let (mut m, mut scene) = empty();
        let mut nodes = vec![session("run/w1@1", SessionKind::Worker, None)];
        let mut spawned = Vec::new();
        for i in 0..2_000 {
            let id = format!("run/w1@1.sub{i}");
            nodes.push(session(&id, SessionKind::Subagent, Some("run/w1@1")));
            spawned.push((AgentId::new("run/w1@1"), AgentId::new(id)));
        }
        let g = graph(nodes, spawned, Vec::new());
        m.set_sessions(&mut scene, &g).expect("set_sessions");
        let h = m.hierarchy();
        let alive = |s: u32| scene.is_alive(s);
        let f = resolve(
            h,
            alive,
            &auto_expand(h, alive, VISIBLE_BUDGET),
            &Map::new(),
        );
        assert!(
            f.visible_count() <= VISIBLE_BUDGET,
            "{} of 2,000 subagents stayed within the budget of {VISIBLE_BUDGET}",
            f.visible_count()
        );
    }
}
