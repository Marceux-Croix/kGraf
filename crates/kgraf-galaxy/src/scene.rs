//! The renderer's input: baked positions, colors, sizes and edges, held in stable
//! slots so a live graph can change one node without rebuilding the rest.
//!
//! Decision 0010: layout is computed and cached in the core; the renderer never runs a
//! force simulation. This module is the whole contract between the two. It is defined
//! here, not borrowed from `kgraf-graph`, so the layout's data functions can change
//! without reaching into the renderer; [`crate::project`] converts graph rows into it.
//!
//! Slots (decision 0015, the graph changes while the user watches): a node or edge
//! keeps its index for its whole life, and a removed one leaves a free slot that the
//! next insert reuses. GPU buffers are laid out in slot order, so a change rewrites
//! only the changed slots' bytes. A free slot is drawn as nothing (see
//! [`crate::selection`]).

use crate::error::GalaxyError;

/// One node as the layout baked it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeInput {
    /// World position.
    pub position: [f32; 3],
    /// sRGB color, each channel 0..=1 (the layout's hex colors divided by 255).
    pub color: [f32; 3],
    /// The layout's node size (label base plus degree boost). The drawn core radius is
    /// this times the selection scale (0.5 normally, 0.2 dimmed), as in the reference.
    /// Must be finite and positive.
    pub size: f32,
}

/// One edge between two nodes, by slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeInput {
    pub source: u32,
    pub target: u32,
    /// sRGB color, each channel 0..=1 (the reference design colors edges by type).
    pub color: [f32; 3],
    /// Base intensity 0..=1 before density scaling. The evidence uses 0.25 within a
    /// cluster and 0.06 across; a call edge's confidence belongs here too (0010).
    pub intensity: f32,
}

/// Which population a node belongs to. Each has its own visual channel: code nodes
/// are gray discs sized by connections (STG-DRAW-2); agents and work items must not
/// reuse that encoding (the reference design keeps the encodings apart), so they get their own
/// sprite shape and a fixed palette color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Population {
    /// A code node from the graph: a shaded disc.
    Code,
    /// A worker agent: a dark disk with a lighter border (style guide STG-DRAW-7),
    /// above the code field; its letter and state dot are drawn over it.
    Agent,
    /// A work item (a task): a rounded card with a dark fill and its state as the
    /// border (STG-DRAW-5).
    Work,
    /// A proxy (decision 0068): a directory, or a group of more siblings than the fan-out
    /// allows, that folded code hides in. Drawn as a gray ring: a shape of its own, not
    /// the item dot (style guide STG-DRAW-1).
    Cluster,
}

impl Population {
    /// The sprite shape code the shader reads from the instance's color alpha.
    /// 0 is reserved for "free slot, draw nothing".
    pub fn shape(self) -> f32 {
        match self {
            Self::Code => 1.0,
            Self::Agent => 2.0,
            Self::Work => 3.0,
            Self::Cluster => 4.0,
        }
    }
}

/// Everything the renderer draws, for building a scene in one go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SceneInput {
    pub nodes: Vec<NodeInput>,
    pub edges: Vec<EdgeInput>,
}

/// A validated scene in stable slots, with each node's incident edges, so a
/// selection's one-hop neighborhood is a short walk, not a scan.
#[derive(Debug, Clone)]
pub struct Scene {
    nodes: Vec<NodeInput>,
    population: Vec<Population>,
    alive: Vec<bool>,
    free_nodes: Vec<u32>,
    edges: Vec<EdgeInput>,
    edge_alive: Vec<bool>,
    free_edges: Vec<u32>,
    /// Live edge slots per node slot.
    incident: Vec<Vec<u32>>,
    live_nodes: usize,
    live_edges: usize,
}

fn check_node(node: &NodeInput, what: impl std::fmt::Display) -> Result<(), GalaxyError> {
    let finite = node
        .position
        .iter()
        .chain(&node.color)
        .all(|v| v.is_finite());
    if !finite || !node.size.is_finite() || node.size <= 0.0 {
        return Err(GalaxyError::InvalidScene(format!(
            "{what} has a non-finite value or a non-positive size: {node:?}"
        )));
    }
    Ok(())
}

fn check_edge_values(e: &EdgeInput, what: impl std::fmt::Display) -> Result<(), GalaxyError> {
    if !e.intensity.is_finite() || e.color.iter().any(|v| !v.is_finite()) {
        return Err(GalaxyError::InvalidScene(format!(
            "{what} has a non-finite color or intensity"
        )));
    }
    Ok(())
}

impl Scene {
    /// Validates `input` (every node is a code node) and indexes its edges. A bad index
    /// or a non-finite number is an error, never a silently dropped node.
    pub fn new(input: SceneInput) -> Result<Self, GalaxyError> {
        let populations = vec![Population::Code; input.nodes.len()];
        Self::with_populations(input, populations)
    }

    /// [`Self::new`] with each node's population given, in node order.
    pub fn with_populations(
        input: SceneInput,
        populations: Vec<Population>,
    ) -> Result<Self, GalaxyError> {
        let n = input.nodes.len();
        if u32::try_from(n).is_err() || u32::try_from(input.edges.len()).is_err() {
            return Err(GalaxyError::InvalidScene(format!(
                "{n} nodes or {} edges exceed the u32 index range",
                input.edges.len()
            )));
        }
        if populations.len() != n {
            return Err(GalaxyError::InvalidScene(format!(
                "{} populations for {n} nodes",
                populations.len()
            )));
        }
        for (i, node) in input.nodes.iter().enumerate() {
            check_node(node, format_args!("node {i}"))?;
        }
        let mut incident = vec![Vec::new(); n];
        for (i, e) in input.edges.iter().enumerate() {
            let (s, t) = (e.source as usize, e.target as usize);
            if s >= n || t >= n {
                return Err(GalaxyError::InvalidScene(format!(
                    "edge {i} references node {} of {n}",
                    s.max(t)
                )));
            }
            check_edge_values(e, format_args!("edge {i}"))?;
            incident[s].push(i as u32);
            if t != s {
                incident[t].push(i as u32);
            }
        }
        let live_edges = input.edges.len();
        Ok(Self {
            alive: vec![true; n],
            population: populations,
            free_nodes: Vec::new(),
            edge_alive: vec![true; live_edges],
            free_edges: Vec::new(),
            nodes: input.nodes,
            edges: input.edges,
            incident,
            live_nodes: n,
            live_edges,
        })
    }

    /// Every node slot, free ones included; check [`Self::is_alive`].
    pub fn nodes(&self) -> &[NodeInput] {
        &self.nodes
    }

    /// Every edge slot, free ones included; check [`Self::is_edge_alive`].
    pub fn edges(&self) -> &[EdgeInput] {
        &self.edges
    }

    pub fn is_alive(&self, node: u32) -> bool {
        self.alive.get(node as usize).copied().unwrap_or(false)
    }

    pub fn is_edge_alive(&self, edge: u32) -> bool {
        self.edge_alive.get(edge as usize).copied().unwrap_or(false)
    }

    /// The node's population (`Code` for an out-of-range slot).
    pub fn population(&self, node: u32) -> Population {
        self.population
            .get(node as usize)
            .copied()
            .unwrap_or(Population::Code)
    }

    /// Nodes that exist (slots minus free slots).
    pub fn live_node_count(&self) -> usize {
        self.live_nodes
    }

    /// Edges that exist.
    pub fn live_edge_count(&self) -> usize {
        self.live_edges
    }

    /// Slots of the live edges incident to `node`.
    pub fn incident_edges(&self, node: u32) -> &[u32] {
        self.incident
            .get(node as usize)
            .map_or(&[][..], |v| v.as_slice())
    }

    /// Undirected neighbors of `node` over live edges (may repeat on parallel edges).
    pub fn neighbors(&self, node: u32) -> impl Iterator<Item = u32> + '_ {
        self.incident_edges(node).iter().map(move |&e| {
            let edge = &self.edges[e as usize];
            if edge.source == node {
                edge.target
            } else {
                edge.source
            }
        })
    }

    /// Live edges incident to `node`.
    pub fn degree(&self, node: u32) -> usize {
        self.incident_edges(node).len()
    }

    /// Adds a node, reusing a free slot when one exists. Returns its slot.
    pub fn insert_node(
        &mut self,
        node: NodeInput,
        population: Population,
    ) -> Result<u32, GalaxyError> {
        check_node(&node, "inserted node")?;
        let slot = match self.free_nodes.pop() {
            Some(slot) => {
                let i = slot as usize;
                self.nodes[i] = node;
                self.population[i] = population;
                self.alive[i] = true;
                slot
            }
            None => {
                let slot = u32::try_from(self.nodes.len()).map_err(|_| {
                    GalaxyError::InvalidScene("node slots exceed the u32 index range".into())
                })?;
                self.nodes.push(node);
                self.population.push(population);
                self.alive.push(true);
                self.incident.push(Vec::new());
                slot
            }
        };
        self.live_nodes += 1;
        Ok(slot)
    }

    /// Replaces a live node's position, color and size.
    pub fn set_node(&mut self, slot: u32, node: NodeInput) -> Result<(), GalaxyError> {
        self.require_node(slot)?;
        check_node(&node, format_args!("node {slot}"))?;
        self.nodes[slot as usize] = node;
        Ok(())
    }

    /// Removes a live node and every edge incident to it. Returns the removed edges'
    /// slots.
    pub fn remove_node(&mut self, slot: u32) -> Result<Vec<u32>, GalaxyError> {
        self.require_node(slot)?;
        let edges = self.incident[slot as usize].clone();
        for &e in &edges {
            self.remove_edge(e)?;
        }
        self.alive[slot as usize] = false;
        self.free_nodes.push(slot);
        self.live_nodes -= 1;
        Ok(edges)
    }

    /// Adds an edge between two live nodes, reusing a free slot. Returns its slot.
    pub fn insert_edge(&mut self, edge: EdgeInput) -> Result<u32, GalaxyError> {
        for end in [edge.source, edge.target] {
            if !self.is_alive(end) {
                return Err(GalaxyError::InvalidScene(format!(
                    "edge endpoint {end} is not a live node"
                )));
            }
        }
        check_edge_values(&edge, "inserted edge")?;
        let slot = match self.free_edges.pop() {
            Some(slot) => {
                self.edges[slot as usize] = edge;
                self.edge_alive[slot as usize] = true;
                slot
            }
            None => {
                let slot = u32::try_from(self.edges.len()).map_err(|_| {
                    GalaxyError::InvalidScene("edge slots exceed the u32 index range".into())
                })?;
                self.edges.push(edge);
                self.edge_alive.push(true);
                slot
            }
        };
        self.incident[edge.source as usize].push(slot);
        if edge.target != edge.source {
            self.incident[edge.target as usize].push(slot);
        }
        self.live_edges += 1;
        Ok(slot)
    }

    /// Changes a live edge's color and intensity.
    pub fn set_edge_look(
        &mut self,
        slot: u32,
        color: [f32; 3],
        intensity: f32,
    ) -> Result<(), GalaxyError> {
        self.require_edge(slot)?;
        let e = &mut self.edges[slot as usize];
        let next = EdgeInput {
            color,
            intensity,
            ..*e
        };
        check_edge_values(&next, format_args!("edge {slot}"))?;
        *e = next;
        Ok(())
    }

    /// Removes a live edge.
    pub fn remove_edge(&mut self, slot: u32) -> Result<(), GalaxyError> {
        self.require_edge(slot)?;
        let e = self.edges[slot as usize];
        for end in [e.source, e.target] {
            self.incident[end as usize].retain(|&x| x != slot);
        }
        self.edge_alive[slot as usize] = false;
        self.free_edges.push(slot);
        self.live_edges -= 1;
        Ok(())
    }

    fn require_node(&self, slot: u32) -> Result<(), GalaxyError> {
        if self.is_alive(slot) {
            Ok(())
        } else {
            Err(GalaxyError::InvalidScene(format!(
                "node slot {slot} is not live"
            )))
        }
    }

    fn require_edge(&self, slot: u32) -> Result<(), GalaxyError> {
        if self.is_edge_alive(slot) {
            Ok(())
        } else {
            Err(GalaxyError::InvalidScene(format!(
                "edge slot {slot} is not live"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(x: f32) -> NodeInput {
        NodeInput {
            position: [x, 0.0, 0.0],
            color: [1.0; 3],
            size: 1.0,
        }
    }

    fn edge(s: u32, t: u32) -> EdgeInput {
        EdgeInput {
            source: s,
            target: t,
            color: [1.0; 3],
            intensity: 0.25,
        }
    }

    fn sorted(it: impl Iterator<Item = u32>) -> Vec<u32> {
        let mut v: Vec<u32> = it.collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn adjacency_is_undirected() {
        let scene = Scene::new(SceneInput {
            nodes: vec![node(0.0), node(1.0), node(2.0), node(3.0)],
            edges: vec![edge(0, 1), edge(2, 0)],
        })
        .expect("valid");
        assert_eq!(sorted(scene.neighbors(0)), vec![1, 2]);
        assert_eq!(sorted(scene.neighbors(1)), vec![0]);
        assert_eq!(scene.neighbors(3).count(), 0);
        assert_eq!(scene.neighbors(99).count(), 0);
    }

    #[test]
    fn bad_edge_index_is_an_error() {
        let err = Scene::new(SceneInput {
            nodes: vec![node(0.0)],
            edges: vec![edge(0, 1)],
        })
        .expect_err("out of range");
        assert!(err.to_string().contains("edge 0"), "{err}");
    }

    #[test]
    fn non_finite_or_zero_size_is_an_error() {
        let mut bad = node(f32::NAN);
        assert!(
            Scene::new(SceneInput {
                nodes: vec![bad],
                edges: vec![]
            })
            .is_err()
        );
        bad = node(0.0);
        bad.size = 0.0;
        assert!(
            Scene::new(SceneInput {
                nodes: vec![bad],
                edges: vec![]
            })
            .is_err()
        );
    }

    #[test]
    fn removal_frees_the_slot_and_its_edges_and_insert_reuses_it() {
        let mut scene = Scene::new(SceneInput {
            nodes: vec![node(0.0), node(1.0), node(2.0)],
            edges: vec![edge(0, 1), edge(1, 2)],
        })
        .expect("valid");
        let removed = scene.remove_node(1).expect("remove");
        assert_eq!(sorted(removed.into_iter()), vec![0, 1]);
        assert!(!scene.is_alive(1));
        assert_eq!(scene.live_node_count(), 2);
        assert_eq!(scene.live_edge_count(), 0);
        assert_eq!(scene.neighbors(0).count(), 0);
        assert!(scene.insert_edge(edge(0, 1)).is_err(), "dead endpoint");

        let slot = scene
            .insert_node(node(9.0), Population::Agent)
            .expect("insert");
        assert_eq!(slot, 1, "the free slot is reused");
        assert_eq!(scene.population(1), Population::Agent);
        let e = scene.insert_edge(edge(1, 2)).expect("edge");
        assert!(e < 2, "a free edge slot is reused");
        assert_eq!(sorted(scene.neighbors(2)), vec![1]);
        assert!(scene.remove_node(7).is_err());
    }
}
