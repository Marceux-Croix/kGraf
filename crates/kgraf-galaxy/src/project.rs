//! A project's graph as a galaxy scene: the bridge from `kgraf-graph` rows to
//! renderer slots, built once from a snapshot and then kept current from change
//! batches.
//!
//! Three populations, each with its own visual channel:
//!
//! - **Code** nodes at their cached layout positions (0007, 0058), colored and sized by
//!   degree with `kgraf_graph::layout::stellar` (the port of the reference's
//!   stellar classification); edges colored by kind at the density module's
//!   same-cluster (0.25) or cross-cluster (0.06) intensity.
//! - **Agents**: every worker holding a lease. Degree owns color and size for code, so
//!   agents do not reuse either (the reference design keeps the encodings apart): a fixed teal
//!   outside the stellar table, a fixed size, a diamond sprite, hovering
//!   [`AGENT_LIFT`] above the centroid of what they act on. An agent's live edges run
//!   to the nodes it holds (0.85) and has touched in its task (0.3), and to its task.
//! - **Work items**: every task a live agent is working, as a magenta square at an
//!   anchor hashed from the task id (the roadmap's rule for work nodes: a position from
//!   identity, not a solve), linked to the agents working it. (Recorded task writes
//!   are not drawn: `task_writes` has no change-log trigger, so they could not stay
//!   live.) Leases past their expiry are not live (0048).
//!
//! - **Proxies** (decision 0068, [`crate::tree`]): one gray ring per directory (single-
//!   child chains merged) and per group of more than `fold::MAX_FANOUT` siblings, at a
//!   position that is a pure function of its key, lifted [`CLUSTER_LIFT`] above the top
//!   call-depth layer. They are what folded code hides in, and they have no edges.
//!
//! Degree counts code edges only, so a busy agent never changes a code node's star
//! class. The model keys everything by graph id; slots are the renderer's business.
//!
//! The fold hierarchy is kept beside the slots, in [`crate::tree::ProjectTree`]: the model
//! reports code nodes and `contains` edges as they come and go, and the tree keeps the
//! directory skeleton, the fan-out groups and their proxies current.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kgraf_graph::layout::density::{CROSS_CLUSTER_EDGE, SAME_CLUSTER_EDGE};
use kgraf_graph::layout::stellar::node_size;
use kgraf_graph::layout::{FIELD_RADIUS, cluster_key, directory_of};
use kgraf_graph::snapshot::{
    Activity, ActivityKind, GraphSnapshot, Resolved, SceneEdge, SceneNode,
};
use kgraf_graph::{EdgeKind, NodeKind};

use crate::error::GalaxyError;
use crate::fold::Hierarchy;
use crate::galaxy::Galaxy;
use crate::palette::Palette;
use crate::scene::{EdgeInput, NodeInput, Population, Scene, SceneInput};
use crate::tree::{CodeNames, ProjectTree, ProxyInfo, ProxyKind};

/// Agent size in the layout's units (a code module is 15, a hub up to 25).
pub const AGENT_SIZE: f32 = 18.0;
pub const WORK_SIZE: f32 = 14.0;
/// Height of agents above what they act on. The top call-depth layer spans z = -15 to
/// 15, so 150 clears it by more than two layers.
pub const AGENT_LIFT: f32 = 150.0;
/// Height of the work-item plane, above the agents.
pub const WORK_LIFT: f32 = 260.0;
/// Work-item anchors fall within this radius of the field center.
pub const WORK_RADIUS: f32 = FIELD_RADIUS * 0.4;

/// Proxy size, before a fold grows it (a code module is 15).
pub const CLUSTER_SIZE: f32 = 12.0;
/// Height of a directory proxy: above the top call-depth layer (z = -15 to 15), below
/// the agents.
pub const CLUSTER_LIFT: f32 = 40.0;

/// Edge intensity for a held lease and for an agent's task: the reference design's 0.85.
pub const LIVE_EDGE: f32 = 0.85;
/// Edge intensity for something touched earlier in the task.
pub const TRAIL_EDGE: f32 = 0.3;

/// The mutations a model applies. Implemented by [`Scene`] (immediate, for building
/// and tests) and [`Galaxy`] (animated, and uploaded in place).
pub trait SceneEdit {
    fn scene(&self) -> &Scene;
    fn insert_node(&mut self, node: NodeInput, population: Population) -> Result<u32, GalaxyError>;
    fn remove_node(&mut self, slot: u32) -> Result<(), GalaxyError>;
    fn move_node(&mut self, slot: u32, position: [f32; 3]) -> Result<(), GalaxyError>;
    fn restyle_node(&mut self, slot: u32, color: [f32; 3], size: f32) -> Result<(), GalaxyError>;
    fn insert_edge(&mut self, edge: EdgeInput) -> Result<u32, GalaxyError>;
    fn remove_edge(&mut self, slot: u32) -> Result<(), GalaxyError>;
    fn set_edge_look(
        &mut self,
        slot: u32,
        color: [f32; 3],
        intensity: f32,
    ) -> Result<(), GalaxyError>;
}

impl SceneEdit for Scene {
    fn scene(&self) -> &Scene {
        self
    }
    fn insert_node(&mut self, n: NodeInput, p: Population) -> Result<u32, GalaxyError> {
        Scene::insert_node(self, n, p)
    }
    fn remove_node(&mut self, slot: u32) -> Result<(), GalaxyError> {
        Scene::remove_node(self, slot).map(|_| ())
    }
    fn move_node(&mut self, slot: u32, position: [f32; 3]) -> Result<(), GalaxyError> {
        let mut n = *self
            .nodes()
            .get(slot as usize)
            .ok_or_else(|| GalaxyError::InvalidScene(format!("no node slot {slot}")))?;
        n.position = position;
        self.set_node(slot, n)
    }
    fn restyle_node(&mut self, slot: u32, color: [f32; 3], size: f32) -> Result<(), GalaxyError> {
        let mut n = *self
            .nodes()
            .get(slot as usize)
            .ok_or_else(|| GalaxyError::InvalidScene(format!("no node slot {slot}")))?;
        n.color = color;
        n.size = size;
        self.set_node(slot, n)
    }
    fn insert_edge(&mut self, e: EdgeInput) -> Result<u32, GalaxyError> {
        Scene::insert_edge(self, e)
    }
    fn remove_edge(&mut self, slot: u32) -> Result<(), GalaxyError> {
        Scene::remove_edge(self, slot)
    }
    fn set_edge_look(&mut self, slot: u32, c: [f32; 3], k: f32) -> Result<(), GalaxyError> {
        Scene::set_edge_look(self, slot, c, k)
    }
}

impl SceneEdit for Galaxy {
    fn scene(&self) -> &Scene {
        Galaxy::scene(self)
    }
    fn insert_node(&mut self, n: NodeInput, p: Population) -> Result<u32, GalaxyError> {
        Galaxy::insert_node(self, n, p)
    }
    fn remove_node(&mut self, slot: u32) -> Result<(), GalaxyError> {
        Galaxy::remove_node(self, slot)
    }
    fn move_node(&mut self, slot: u32, position: [f32; 3]) -> Result<(), GalaxyError> {
        Galaxy::move_node(self, slot, position)
    }
    fn restyle_node(&mut self, slot: u32, color: [f32; 3], size: f32) -> Result<(), GalaxyError> {
        Galaxy::restyle_node(self, slot, color, size)
    }
    fn insert_edge(&mut self, e: EdgeInput) -> Result<u32, GalaxyError> {
        Galaxy::insert_edge(self, e)
    }
    fn remove_edge(&mut self, slot: u32) -> Result<(), GalaxyError> {
        Galaxy::remove_edge(self, slot)
    }
    fn set_edge_look(&mut self, slot: u32, c: [f32; 3], k: f32) -> Result<(), GalaxyError> {
        Galaxy::set_edge_look(self, slot, c, k)
    }
}

/// World positions (what an agent holds, or touched).
type Positions = Vec<[f32; 3]>;
/// An overlay edge's color and intensity.
type EdgeLook = ([f32; 3], f32);

/// What a code slot needs for color, size and labels.
#[derive(Debug, Clone)]
struct CodeSlot {
    id: String,
    qualified_name: String,
    path: String,
    kind: NodeKind,
    cluster: u32,
    /// Live code edges touching it.
    degree: u32,
}

/// An overlay endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum End {
    Agent(String),
    Task(String),
    Node(String),
}

/// What an edge slot stands for, so a removal can clean up its key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum EdgeKey {
    /// A graph edge, by its id: one allocation shared with `ProjectModel::edges`.
    Code(Arc<str>),
    Overlay(End, End),
    /// A work-status edge of the plan overlay (`plan.rs`): from, to.
    Plan(String, String),
}

/// What one apply did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub inserted: usize,
    pub removed: usize,
    pub moved: usize,
    pub restyled: usize,
    pub edges_inserted: usize,
    pub edges_removed: usize,
    /// Edges whose endpoint the model does not hold. The feed reads nodes and edges in
    /// one snapshot, so this should stay 0; it is counted and shown, never hidden.
    pub edges_skipped: usize,
}

impl ApplyReport {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Deterministic 64-bit FNV-1a, for work-item anchors (same input, same place, on
/// every machine).
pub(crate) fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A work item's anchor: a pure function of its id ("a node's anchor
/// derives from a hash of its own identity").
pub fn work_anchor(task: &str) -> [f32; 3] {
    let h = fnv64(task);
    let angle = (h & 0xFFFF) as f32 / 65535.0 * std::f32::consts::TAU;
    // sqrt for an even spread over the disc, not a pile at the center.
    let r = (((h >> 16) & 0xFFFF) as f32 / 65535.0).sqrt() * WORK_RADIUS;
    [r * angle.cos(), r * angle.sin(), WORK_LIFT]
}

/// A project's scene model: which slot each graph id occupies.
#[derive(Debug, Default)]
pub struct ProjectModel {
    nodes: HashMap<String, u32>,
    /// Code slot data, by slot (`None` for overlay slots and free slots).
    code: Vec<Option<CodeSlot>>,
    clusters: HashMap<String, u32>,
    /// Graph edge id to scene slot. The id is shared with the slot's
    /// [`EdgeKey::Code`]: one allocation per edge, not two (the build's edge maps were
    /// half of it, measured 2026-09-27).
    edges: HashMap<Arc<str>, u32>,
    pub(crate) edge_keys: HashMap<u32, EdgeKey>,
    agents: HashMap<String, u32>,
    /// Work items by key: a task with live leases, a plan node, or both.
    pub(crate) tasks: HashMap<String, u32>,
    overlay: HashMap<(End, End), u32>,
    /// The last activity set, reapplied when code nodes move or appear.
    pub(crate) activity: Vec<Activity>,
    /// The plan overlay (`plan.rs`): work status from the run records.
    pub(crate) plan: HashMap<String, crate::plan::PlanNode>,
    pub(crate) plan_edges: HashMap<(String, String), u32>,
    /// Activity rows whose path is not in the index (drawn as nothing, counted).
    unindexed_activity: usize,
    /// Edge slots of `contains` edges, so a removal can re-home the child.
    contains: HashSet<u32>,
    /// The fold hierarchy and its proxies (work nodes join it in `plan.rs`).
    pub(crate) tree: ProjectTree,
    /// Every color the model gives the scene ([`Self::set_palette`]).
    pub(crate) palette: Palette,
    /// Sessions on the graph (decision 0067, `sessions.rs`): the one hook the
    /// model itself needs, so the overlay's own bookkeeping (a lease-less session's
    /// placeholder slot, its `spawned`/`connection` edges, the soft-limit badge) stays
    /// out of this file.
    pub(crate) sessions: crate::sessions::SessionOverlay,
}

impl ProjectModel {
    /// Builds the model and its scene from a snapshot. Code nodes and edges go in bulk;
    /// the overlay goes through the same diff every later change uses.
    pub fn build(snapshot: &GraphSnapshot) -> Result<(Self, Scene), GalaxyError> {
        Self::build_with(snapshot, Palette::default())
    }

    /// [`Self::build`] with the colors of `palette`.
    pub fn build_with(
        snapshot: &GraphSnapshot,
        palette: Palette,
    ) -> Result<(Self, Scene), GalaxyError> {
        let mut model = Self {
            palette,
            ..Self::default()
        };
        model.tree.set_proxy_color(palette.proxy);
        let mut index: HashMap<&str, u32> = HashMap::with_capacity(snapshot.nodes.len());
        let mut degree = vec![0u32; snapshot.nodes.len()];
        let mut cluster = Vec::with_capacity(snapshot.nodes.len());
        for (i, n) in snapshot.nodes.iter().enumerate() {
            index.insert(n.id.as_str(), i as u32);
            cluster.push(model.cluster_of(&n.path));
        }
        let mut edges = Vec::with_capacity(snapshot.edges.len());
        let mut skipped = 0usize;
        for e in &snapshot.edges {
            let (Some(&s), Some(&t)) = (index.get(e.src.as_str()), index.get(e.dst.as_str()))
            else {
                skipped += 1;
                continue;
            };
            degree[s as usize] += 1;
            degree[t as usize] += 1;
            edges.push(EdgeInput {
                source: s,
                target: t,
                color: palette.edge,
                intensity: cluster_intensity(cluster[s as usize] == cluster[t as usize]),
            });
        }
        if skipped > 0 {
            // A consistent snapshot has none; an inconsistent one is an index defect.
            return Err(GalaxyError::InvalidScene(format!(
                "{skipped} edges in the snapshot reference nodes it does not contain"
            )));
        }
        let nodes = snapshot
            .nodes
            .iter()
            .zip(&degree)
            .map(|(n, &d)| NodeInput {
                position: [n.position.x, n.position.y, n.position.z],
                color: palette.code_dot,
                size: node_size(n.kind, d),
            })
            .collect();
        let mut scene = Scene::new(SceneInput { nodes, edges })?;
        model.code = snapshot
            .nodes
            .iter()
            .zip(degree)
            .zip(cluster)
            .map(|((n, degree), cluster)| {
                Some(CodeSlot {
                    id: n.id.clone(),
                    qualified_name: n.qualified_name.clone(),
                    path: n.path.clone(),
                    kind: n.kind,
                    cluster,
                    degree,
                })
            })
            .collect();
        model.nodes = index.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        let mut contains = Vec::new();
        // Sized once: growing these maps edge by edge rehashed them about 17 times at
        // 100,000 nodes, and the edge maps were half the build (measured 2026-09-27).
        model.edges.reserve(snapshot.edges.len());
        model.edge_keys.reserve(snapshot.edges.len());
        for (slot, e) in snapshot.edges.iter().enumerate() {
            let id: Arc<str> = Arc::from(e.id.as_str());
            model.edges.insert(Arc::clone(&id), slot as u32);
            model.edge_keys.insert(slot as u32, EdgeKey::Code(id));
            if e.kind == EdgeKind::Contains {
                let edge = scene.edges()[slot];
                model.contains.insert(slot as u32);
                contains.push((edge.source, edge.target));
            }
        }
        model.tree.load(
            snapshot
                .nodes
                .iter()
                .enumerate()
                .map(|(slot, n)| (slot as u32, n.id.as_str(), n.path.as_str())),
            &contains,
        );
        model.tree.sync(&mut scene, &Names(&model.code))?;
        model.set_activity(&mut scene, snapshot.activity.clone())?;
        Ok((model, scene))
    }

    /// The palette the model colors the scene with.
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Recolors everything the model drew with `palette`: code dots, edges, proxies,
    /// agents and work items. Sizes and positions do not change.
    pub fn set_palette(
        &mut self,
        target: &mut impl SceneEdit,
        palette: Palette,
    ) -> Result<(), GalaxyError> {
        if palette == self.palette {
            return Ok(());
        }
        self.palette = palette;
        self.tree.set_proxy_color(palette.proxy);
        let n = target.scene().nodes().len() as u32;
        for slot in 0..n {
            if !target.scene().is_alive(slot) {
                continue;
            }
            let size = target.scene().nodes()[slot as usize].size;
            let color = match target.scene().population(slot) {
                Population::Code => palette.code_dot,
                Population::Cluster => palette.proxy,
                Population::Agent => palette.agent,
                Population::Work => palette.state_neutral,
            };
            target.restyle_node(slot, color, size)?;
        }
        let edges = target.scene().edges().len() as u32;
        for e in 0..edges {
            if target.scene().is_edge_alive(e) {
                let k = target.scene().edges()[e as usize].intensity;
                let color = match self.edge_keys.get(&e) {
                    Some(EdgeKey::Plan(..)) => palette.relation_arrow,
                    _ => palette.edge,
                };
                target.set_edge_look(e, color, k)?;
            }
        }
        // Work items that are plan nodes wear their state's color.
        self.restyle_plan(target)
    }

    /// The fold hierarchy over code slots and proxies.
    pub fn hierarchy(&self) -> &Hierarchy {
        self.tree.hierarchy()
    }

    /// `contains` edges refused because they would close a cycle.
    pub fn refused_contains(&self) -> usize {
        self.tree.refused_contains()
    }

    /// Work-node parents (`PlanNode::parent`) refused because they would close a cycle.
    pub fn refused_work_parents(&self) -> usize {
        self.tree.refused_work()
    }

    /// Live code nodes: the graph's own nodes, without cluster proxies or the overlay.
    pub fn code_node_count(&self) -> usize {
        self.code.iter().filter(|c| c.is_some()).count()
    }

    /// Live proxies: directories and groups.
    pub fn proxy_count(&self) -> usize {
        self.tree.proxy_count()
    }

    /// The proxy at `slot`, if one is.
    pub fn proxy_at(&self, slot: u32) -> Option<&ProxyInfo> {
        self.tree.proxy(slot)
    }

    /// Every proxy and its slot.
    pub fn proxies(&self) -> impl Iterator<Item = (u32, &ProxyInfo)> {
        self.tree.proxies()
    }

    /// A key for `slot` that survives a reload (slots do not): the graph id of a
    /// code node, the proxy key (`dir:...`, `grp:...`) for a proxy. `None` for the
    /// overlay.
    pub fn fold_key(&self, slot: u32) -> Option<String> {
        if let Some(id) = self.node_id(slot) {
            return Some(id.to_owned());
        }
        if let Some(p) = self.tree.proxy(slot) {
            return Some(p.key.clone());
        }
        self.task_at(slot).map(|k| format!("plan:{k}"))
    }

    /// The slot a [`Self::fold_key`] names now.
    pub fn slot_of_fold_key(&self, key: &str) -> Option<u32> {
        if let Some(k) = key.strip_prefix("plan:") {
            return self.task_slot(k);
        }
        self.tree.proxy_slot(key).or_else(|| self.slot_of(key))
    }

    fn cluster_of(&mut self, path: &str) -> u32 {
        let key = cluster_key(directory_of(path));
        if let Some(&c) = self.clusters.get(key) {
            return c;
        }
        let c = self.clusters.len() as u32;
        self.clusters.insert(key.to_owned(), c);
        c
    }

    /// The slot of a graph node, if the model holds it.
    pub fn slot_of(&self, node_id: &str) -> Option<u32> {
        self.nodes.get(node_id).copied()
    }

    /// The slot of a live agent.
    pub fn agent_slot(&self, agent: &str) -> Option<u32> {
        self.agents.get(agent).copied()
    }

    /// The slot of a live work item.
    pub fn task_slot(&self, task: &str) -> Option<u32> {
        self.tasks.get(task).copied()
    }

    /// The agent (lease holder) drawn at `slot`, if an agent is.
    pub fn agent_at(&self, slot: u32) -> Option<&str> {
        self.agents
            .iter()
            .find(|(_, s)| **s == slot)
            .map(|(a, _)| a.as_str())
    }

    /// The work item (task id) drawn at `slot`, if one is.
    pub fn task_at(&self, slot: u32) -> Option<&str> {
        self.tasks
            .iter()
            .find(|(_, s)| **s == slot)
            .map(|(t, _)| t.as_str())
    }

    /// Every live agent and its slot.
    pub fn agents(&self) -> impl Iterator<Item = (&str, u32)> {
        self.agents.iter().map(|(a, s)| (a.as_str(), *s))
    }

    /// The tasks each live agent works on, from the activity rows.
    pub fn agent_tasks(&self, agent: &str) -> Vec<&str> {
        let mut tasks: Vec<&str> = self
            .activity
            .iter()
            .filter(|a| a.agent == agent)
            .map(|a| a.task.as_str())
            .collect();
        tasks.sort_unstable();
        tasks.dedup();
        tasks
    }

    /// Live agents and work items.
    pub fn overlay_counts(&self) -> (usize, usize) {
        (self.agents.len(), self.tasks.len())
    }

    /// Activity rows naming a path that is not in the index.
    pub fn unindexed_activity(&self) -> usize {
        self.unindexed_activity
    }

    /// A one-line description of a slot, for the selection label.
    pub fn describe(&self, slot: u32) -> Option<String> {
        if let Some(Some(c)) = self.code.get(slot as usize) {
            return Some(format!(
                "{} {} ({}, degree {})",
                c.kind.as_str(),
                c.qualified_name,
                c.path,
                c.degree
            ));
        }
        if let Some(p) = self.tree.proxy(slot) {
            let below = self.hierarchy().children(slot).len();
            return Some(match p.kind {
                ProxyKind::Directory => format!("directory {}: {below} item(s) below", p.full),
                ProxyKind::Group => format!("group {}: {below} item(s)", p.full),
            });
        }
        let count =
            |pred: &dyn Fn(&Activity) -> bool| self.activity.iter().filter(|a| pred(a)).count();
        if let Some((agent, _)) = self.agents.iter().find(|(_, s)| **s == slot) {
            let mine = |a: &Activity, k| &a.agent == agent && a.kind == k;
            let tasks: HashSet<&str> = self
                .activity
                .iter()
                .filter(|a| &a.agent == agent)
                .map(|a| a.task.as_str())
                .collect();
            let mut tasks: Vec<&str> = tasks.into_iter().collect();
            tasks.sort_unstable();
            return Some(format!(
                "agent {agent} (task {}): holds {}, touched {}",
                tasks.join(", "),
                count(&|a| mine(a, ActivityKind::Holds)),
                count(&|a| mine(a, ActivityKind::Touched)),
            ));
        }
        if let Some((task, _)) = self.tasks.iter().find(|(_, s)| **s == slot) {
            let agents: HashSet<&str> = self
                .activity
                .iter()
                .filter(|a| &a.task == task)
                .map(|a| a.agent.as_str())
                .collect();
            return Some(format!(
                "work item {task}: {} agent(s), {} live lease(s)",
                agents.len(),
                count(&|a| &a.task == task && a.kind == ActivityKind::Holds),
            ));
        }
        None
    }

    fn restyle(&self, target: &mut impl SceneEdit, slot: u32) -> Result<(), GalaxyError> {
        if let Some(Some(c)) = self.code.get(slot as usize) {
            target.restyle_node(slot, self.palette.code_dot, node_size(c.kind, c.degree))?;
        }
        Ok(())
    }

    /// Drops the model's key for an edge slot the scene is about to lose, adjusting
    /// degrees; returns the code endpoints whose degree changed.
    fn forget_edge(&mut self, scene: &Scene, slot: u32, changed: &mut HashSet<u32>) {
        match self.edge_keys.remove(&slot) {
            Some(EdgeKey::Code(id)) => {
                self.edges.remove(&*id);
                if let Some(e) = scene.edges().get(slot as usize) {
                    for end in [e.source, e.target] {
                        if let Some(Some(c)) = self.code.get_mut(end as usize) {
                            c.degree = c.degree.saturating_sub(1);
                            changed.insert(end);
                        }
                    }
                    if self.contains.remove(&slot) {
                        // The child waits under its directory for a new parent edge.
                        self.tree.contains_removed(e.source, e.target);
                    }
                }
            }
            Some(EdgeKey::Overlay(a, b)) => {
                self.overlay.remove(&(a, b));
            }
            Some(EdgeKey::Plan(a, b)) => {
                self.plan_edges.remove(&(a, b));
            }
            None => {}
        }
    }

    /// Applies one resolved change batch: removals, then inserts and moves, then edges,
    /// then restyles for changed degrees, then the overlay.
    pub fn apply(
        &mut self,
        target: &mut impl SceneEdit,
        batch: &Resolved,
    ) -> Result<ApplyReport, GalaxyError> {
        let mut report = ApplyReport::default();
        let mut degree_changed: HashSet<u32> = HashSet::new();

        for id in &batch.removed_edges {
            if let Some(&slot) = self.edges.get(id.as_str()) {
                self.forget_edge(target.scene(), slot, &mut degree_changed);
                target.remove_edge(slot)?;
                report.edges_removed += 1;
            }
        }
        for id in &batch.removed_nodes {
            let Some(slot) = self.nodes.remove(id) else {
                continue;
            };
            let incident = target.scene().incident_edges(slot).to_vec();
            for e in incident {
                self.forget_edge(target.scene(), e, &mut degree_changed);
                report.edges_removed += 1;
            }
            self.tree.code_removed(slot);
            if let Some(c) = self.code.get_mut(slot as usize) {
                *c = None;
            }
            degree_changed.remove(&slot);
            target.remove_node(slot)?;
            report.removed += 1;
        }
        for n in &batch.nodes {
            let position = [n.position.x, n.position.y, n.position.z];
            match self.nodes.get(&n.id).copied() {
                Some(slot) => {
                    if target.scene().nodes()[slot as usize].position != position {
                        target.move_node(slot, position)?;
                        report.moved += 1;
                    }
                    if let Some(Some(c)) = self.code.get_mut(slot as usize)
                        && (c.kind != n.kind || c.qualified_name != n.qualified_name)
                    {
                        c.kind = n.kind;
                        c.qualified_name = n.qualified_name.clone();
                        degree_changed.insert(slot);
                        self.tree.code_relabeled(slot);
                    }
                }
                None => {
                    let slot = target.insert_node(
                        NodeInput {
                            position,
                            color: self.palette.code_dot,
                            size: node_size(n.kind, 0),
                        },
                        Population::Code,
                    )?;
                    self.put_code(slot, n);
                    self.tree.code_added(slot, &n.id, &n.path);
                    report.inserted += 1;
                }
            }
        }
        for e in &batch.edges {
            if self.edges.contains_key(e.id.as_str()) {
                continue;
            }
            match self.insert_code_edge(target, e)? {
                Some((s, t)) => {
                    degree_changed.insert(s);
                    degree_changed.insert(t);
                    report.edges_inserted += 1;
                }
                None => report.edges_skipped += 1,
            }
        }
        for slot in degree_changed {
            if target.scene().is_alive(slot) {
                self.restyle(target, slot)?;
                report.restyled += 1;
            }
        }
        self.tree.sync(target, &Names(&self.code))?;
        let code_changed = report.inserted + report.removed + report.moved > 0;
        match &batch.activity {
            Some(activity) => self.set_activity(target, activity.clone())?,
            // Agents hover over what they hold; if that moved, they follow.
            None if code_changed && !self.activity.is_empty() => {
                self.set_activity(target, self.activity.clone())?
            }
            None => {}
        }
        Ok(report)
    }

    fn put_code(&mut self, slot: u32, n: &SceneNode) {
        let cluster = self.cluster_of(&n.path);
        if self.code.len() <= slot as usize {
            self.code.resize(slot as usize + 1, None);
        }
        self.code[slot as usize] = Some(CodeSlot {
            id: n.id.clone(),
            qualified_name: n.qualified_name.clone(),
            path: n.path.clone(),
            kind: n.kind,
            cluster,
            degree: 0,
        });
        self.nodes.insert(n.id.clone(), slot);
    }

    fn insert_code_edge(
        &mut self,
        target: &mut impl SceneEdit,
        e: &SceneEdge,
    ) -> Result<Option<(u32, u32)>, GalaxyError> {
        let (Some(&s), Some(&t)) = (self.nodes.get(&e.src), self.nodes.get(&e.dst)) else {
            return Ok(None);
        };
        let cluster = |slot: u32| {
            self.code
                .get(slot as usize)
                .and_then(|c| c.as_ref())
                .map(|c| c.cluster)
        };
        let same = cluster(s) == cluster(t);
        let slot = target.insert_edge(EdgeInput {
            source: s,
            target: t,
            color: self.palette.edge,
            intensity: cluster_intensity(same),
        })?;
        let id: Arc<str> = Arc::from(e.id.as_str());
        self.edges.insert(Arc::clone(&id), slot);
        self.edge_keys.insert(slot, EdgeKey::Code(id));
        if e.kind == EdgeKind::Contains {
            self.contains.insert(slot);
            self.tree.contains_added(s, t);
        }
        for end in [s, t] {
            if let Some(Some(c)) = self.code.get_mut(end as usize) {
                c.degree += 1;
            }
        }
        Ok(Some((s, t)))
    }

    /// Replaces the overlay with the one `activity` describes, changing only what
    /// differs: agents and work items that stay keep their slot (and move if what
    /// they hold moved), edges that stay keep theirs.
    fn set_activity(
        &mut self,
        target: &mut impl SceneEdit,
        activity: Vec<Activity>,
    ) -> Result<(), GalaxyError> {
        // Desired state.
        let mut agent_targets: HashMap<&str, (Positions, Positions)> = HashMap::new();
        let mut tasks: HashSet<&str> = HashSet::new();
        let mut want: HashMap<(End, End), ([f32; 3], f32)> = HashMap::new();
        let mut unindexed = 0usize;
        let pos_of = |slot: u32| target.scene().nodes()[slot as usize].position;
        for a in &activity {
            let node_slot = a
                .node_id
                .as_deref()
                .and_then(|id| self.nodes.get(id).copied());
            if node_slot.is_none() {
                unindexed += 1;
            }
            let agent = &a.agent;
            let entry = agent_targets.entry(agent.as_str()).or_default();
            tasks.insert(a.task.as_str());
            want.insert(
                (End::Agent(agent.clone()), End::Task(a.task.clone())),
                (self.palette.edge, LIVE_EDGE),
            );
            if let (Some(slot), Some(id)) = (node_slot, &a.node_id) {
                let key = (End::Agent(agent.clone()), End::Node(id.clone()));
                let k = if a.kind == ActivityKind::Holds {
                    entry.0.push(pos_of(slot));
                    LIVE_EDGE
                } else {
                    entry.1.push(pos_of(slot));
                    TRAIL_EDGE
                };
                let e = want.entry(key).or_insert((self.palette.edge, k));
                e.1 = e.1.max(k);
            }
        }
        let agent_pos: HashMap<&str, [f32; 3]> = agent_targets
            .iter()
            .map(|(&agent, (held, touched))| {
                let over = if held.is_empty() { touched } else { held };
                let p = if over.is_empty() {
                    let [x, y, _] = work_anchor(agent);
                    [x, y, AGENT_LIFT]
                } else {
                    let k = over.len() as f32;
                    let s = over.iter().fold([0.0f32; 3], |acc, p| {
                        [acc[0] + p[0], acc[1] + p[1], acc[2] + p[2]]
                    });
                    // Above the highest thing it holds, so it never sinks into the field.
                    let top = over.iter().map(|p| p[2]).fold(f32::MIN, f32::max);
                    [s[0] / k, s[1] / k, top.max(s[2] / k) + AGENT_LIFT]
                };
                (agent, p)
            })
            .collect();

        // Remove what is no longer wanted: edges first, then their endpoints.
        let stale: Vec<((End, End), u32)> = self
            .overlay
            .iter()
            .filter(|(k, _)| !want.contains_key(*k))
            .map(|(k, &s)| (k.clone(), s))
            .collect();
        for (key, slot) in stale {
            self.overlay.remove(&key);
            self.edge_keys.remove(&slot);
            target.remove_edge(slot)?;
        }
        let gone_agents: Vec<(String, u32)> = self
            .agents
            .iter()
            .filter(|(a, _)| !agent_pos.contains_key(a.as_str()))
            .map(|(a, &s)| (a.clone(), s))
            .collect();
        for (agent, slot) in gone_agents {
            self.agents.remove(&agent);
            target.remove_node(slot)?;
        }
        let gone_tasks: Vec<(String, u32)> = self
            .tasks
            .iter()
            // A plan node stays while its run record has it, leases or not.
            .filter(|(t, _)| !tasks.contains(t.as_str()) && !self.plan.contains_key(*t))
            .map(|(t, &s)| (t.clone(), s))
            .collect();
        for (task, slot) in gone_tasks {
            self.tasks.remove(&task);
            self.tree.remove_work(slot);
            target.remove_node(slot)?;
        }

        // Add or move agents and work items.
        let mut agents: Vec<(&str, [f32; 3])> = agent_pos.into_iter().collect();
        agents.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for (agent, p) in agents {
            match self.agents.get(agent).copied() {
                Some(slot) => {
                    if target.scene().nodes()[slot as usize].position != p {
                        target.move_node(slot, p)?;
                    }
                }
                None => {
                    let slot = target.insert_node(
                        NodeInput {
                            position: p,
                            color: self.palette.agent,
                            size: AGENT_SIZE,
                        },
                        Population::Agent,
                    )?;
                    self.agents.insert(agent.to_owned(), slot);
                }
            }
        }
        let mut task_list: Vec<&str> = tasks.into_iter().collect();
        task_list.sort_unstable();
        for task in task_list {
            if !self.tasks.contains_key(task) {
                let slot = target.insert_node(
                    NodeInput {
                        position: work_anchor(task),
                        color: self.palette.state_neutral,
                        size: WORK_SIZE,
                    },
                    Population::Work,
                )?;
                self.tasks.insert(task.to_owned(), slot);
            }
        }

        // Add or restyle edges.
        let mut wanted: Vec<((End, End), EdgeLook)> = want.into_iter().collect();
        wanted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for ((from, to), (color, k)) in wanted {
            let end_slot = |end: &End| match end {
                End::Agent(a) => self.agents.get(a).copied(),
                End::Task(t) => self.tasks.get(t).copied(),
                End::Node(n) => self.nodes.get(n).copied(),
            };
            let (Some(s), Some(t)) = (end_slot(&from), end_slot(&to)) else {
                continue;
            };
            let key = (from, to);
            match self.overlay.get(&key).copied() {
                Some(slot) => {
                    let e = target.scene().edges()[slot as usize];
                    if e.color != color || e.intensity != k {
                        target.set_edge_look(slot, color, k)?;
                    }
                }
                None => {
                    let slot = target.insert_edge(EdgeInput {
                        source: s,
                        target: t,
                        color,
                        intensity: k,
                    })?;
                    self.edge_keys
                        .insert(slot, EdgeKey::Overlay(key.0.clone(), key.1.clone()));
                    self.overlay.insert(key, slot);
                }
            }
        }
        self.activity = activity;
        self.unindexed_activity = unindexed;
        // A work item that is also a plan node wears its plan status, not the
        // plain work color a lease alone gives it.
        self.restyle_plan(target)?;
        Ok(())
    }

    /// Every live code node: slot, qualified name, path, kind and degree (the
    /// search index reads these).
    pub fn code_nodes(&self) -> impl Iterator<Item = (u32, &str, &str, NodeKind, u32)> {
        self.code.iter().enumerate().filter_map(|(i, c)| {
            c.as_ref().map(|c| {
                (
                    i as u32,
                    c.qualified_name.as_str(),
                    c.path.as_str(),
                    c.kind,
                    c.degree,
                )
            })
        })
    }

    /// The qualified name at a code slot.
    pub fn qualified_name(&self, slot: u32) -> Option<&str> {
        self.code
            .get(slot as usize)
            .and_then(|c| c.as_ref())
            .map(|c| c.qualified_name.as_str())
    }

    /// The live activity rows: who holds or touched what.
    pub fn activity(&self) -> &[Activity] {
        &self.activity
    }

    /// The graph id at a code slot.
    pub fn node_id(&self, slot: u32) -> Option<&str> {
        self.code
            .get(slot as usize)
            .and_then(|c| c.as_ref())
            .map(|c| c.id.as_str())
    }
}

/// The model's code names for the tree ([`CodeNames`]): a file sorts by its name, a
/// chunk by its qualified name; the key is the graph id. Borrowed, never copied.
struct Names<'a>(&'a [Option<CodeSlot>]);

impl CodeNames for Names<'_> {
    fn label(&self, slot: u32) -> &str {
        match self.0.get(slot as usize) {
            Some(Some(c)) if c.kind == NodeKind::File => {
                c.path.rsplit('/').next().unwrap_or(&c.path)
            }
            Some(Some(c)) => &c.qualified_name,
            _ => "",
        }
    }

    fn key(&self, slot: u32) -> &str {
        match self.0.get(slot as usize) {
            Some(Some(c)) => &c.id,
            _ => "",
        }
    }
}

fn cluster_intensity(same_cluster: bool) -> f32 {
    if same_cluster {
        SAME_CLUSTER_EDGE
    } else {
        CROSS_CLUSTER_EDGE
    }
}
