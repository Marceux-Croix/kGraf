//! Selection: a node, its one-hop neighborhood, and what that does to the drawing.
//!
//! Ported from DeusData/codebase-memory-mcp graph-ui/src/components/NodeCloud.tsx and
//! EdgeLines.tsx at 64c23fab6c3eba3de226355c0ca1ff750116edab, MIT (see
//! third_party/codebase-memory-mcp/UPSTREAM), as recorded in
//! the analysis of that UI, section 4,
//! "Selection": nodes draw at instance scale 0.5 when highlighted or when nothing is
//! selected; non-highlighted nodes get color times 0.15 and scale 0.2; edges with
//! neither endpoint highlighted are dropped; both highlighted go to 0.5; half
//! highlighted to 0.04 scaled by density. The constants are
//! `kgraf_graph::layout::density`'s, used directly (no copy).
//! Changes: no glow boost (no bloom pass yet); selection kept as a mask, not a set;
//! edges touching an agent or work item (the overlay) use the 0.85 the evidence
//! reserves for those links (recommendation 3) when both ends are highlighted, and
//! their own intensity, never density-scaled, when nothing is selected.

use bytemuck::{Pod, Zeroable};
use glam::Vec3;

use crate::budget::RenderBudget;
use crate::scene::{EdgeInput, Population, Scene};

pub use kgraf_graph::layout::density::{
    DIMMED_NODE_COLOR as DIM_COLOR, DIMMED_NODE_SCALE as DIM_SCALE,
    HALF_SELECTED_EDGE as EDGE_HALF_SELECTED, NODE_SCALE, SELECTED_EDGE as EDGE_BOTH_SELECTED,
    edge_intensity_scale,
};

/// Intensity of an overlay edge (agent or work item) with both ends highlighted: the
/// evidence's 0.85 for links outside the code field.
pub const OVERLAY_EDGE: f32 = 0.85;

/// A selected node and its highlighted set (itself plus its one-hop neighbors).
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    selected: u32,
    highlighted: Vec<bool>,
    members: Vec<u32>,
    /// A set selection (search): every member was asked for by name, so each is
    /// drawn even when a fold hides it. A one-hop selection names only its node.
    set: bool,
}

impl Selection {
    /// `None` when `node` is not a live node of the scene.
    pub fn one_hop(scene: &Scene, node: u32) -> Option<Self> {
        Self::one_hop_and(scene, node, &[])
    }

    /// [`Self::one_hop`] plus `extra` nodes lit with it: a proxy's hierarchy children
    /// (decision 0068), which it has no edges to. Dead slots in `extra` are skipped.
    pub fn one_hop_and(scene: &Scene, node: u32, extra: &[u32]) -> Option<Self> {
        if !scene.is_alive(node) {
            return None;
        }
        let mut highlighted = vec![false; scene.nodes().len()];
        let mut members = vec![node];
        highlighted[node as usize] = true;
        let lit = extra.iter().copied().filter(|&m| scene.is_alive(m));
        for m in scene.neighbors(node).chain(lit) {
            if !highlighted[m as usize] {
                highlighted[m as usize] = true;
                members.push(m);
            }
        }
        Some(Self {
            selected: node,
            highlighted,
            members,
            set: false,
        })
    }

    /// A set of nodes lit together, with no neighbors added: a search result
    /// (0032: "matching nodes light up and everything else recedes"), ported from
    /// codebase-memory-mcp `GraphTab.tsx` `handleSelectPath`, which sets the
    /// highlighted ids to the matched set and targets the camera at its centroid.
    /// `primary` is the best match; it must be one of `nodes`. Dead slots are
    /// skipped; `None` when no live node remains.
    pub fn of_set(scene: &Scene, primary: u32, nodes: &[u32]) -> Option<Self> {
        let mut highlighted = vec![false; scene.nodes().len()];
        let mut members = Vec::with_capacity(nodes.len());
        let live = |n: u32| scene.is_alive(n);
        let first = if live(primary) {
            primary
        } else {
            *nodes.iter().find(|&&n| live(n))?
        };
        for &n in std::iter::once(&first).chain(nodes) {
            if live(n) && !highlighted[n as usize] {
                highlighted[n as usize] = true;
                members.push(n);
            }
        }
        Some(Self {
            selected: first,
            highlighted,
            members,
            set: true,
        })
    }

    /// Whether this is a set selection (a search's matches), not one node's
    /// neighborhood.
    pub fn is_set(&self) -> bool {
        self.set
    }

    pub fn selected(&self) -> u32 {
        self.selected
    }

    /// The selected node first, then its distinct neighbors.
    pub fn members(&self) -> &[u32] {
        &self.members
    }

    /// Whether `node` was named by the selection itself (the selected node, or any
    /// member of a set), not merely lit as a neighbor. A fold never hides these: a
    /// search reaches what the visual layer folded away (0032).
    pub fn is_explicit(&self, node: u32) -> bool {
        if self.set {
            self.is_highlighted(node)
        } else {
            node == self.selected
        }
    }

    pub fn is_highlighted(&self, node: u32) -> bool {
        self.highlighted
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Centroid and radius (largest distance from the centroid) of the highlighted set,
    /// for the camera's fly-to.
    pub fn centroid_and_spread(&self, scene: &Scene) -> (Vec3, f32) {
        let pos = |i: u32| Vec3::from(scene.nodes()[i as usize].position);
        let sum: Vec3 = self.members.iter().map(|&i| pos(i)).sum();
        let centroid = sum / self.members.len().max(1) as f32;
        let spread = self
            .members
            .iter()
            .map(|&i| pos(i).distance(centroid))
            .fold(0.0, f32::max);
        (centroid, spread)
    }
}

/// One node sprite as the GPU reads it (instance-rate vertex buffer).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct NodeInstance {
    /// xyz world position, w core radius.
    pub pos_size: [f32; 4],
    /// Linear RGB; a is the sprite shape ([`Population::shape`]), 0 for a free slot,
    /// which draws nothing.
    pub color: [f32; 4],
}

impl NodeInstance {
    /// A free slot: nothing is drawn and nothing is picked.
    pub const HIDDEN: Self = Self {
        pos_size: [0.0; 4],
        color: [0.0; 4],
    };

    pub fn is_hidden(&self) -> bool {
        self.color[3] == 0.0
    }
}

/// One edge endpoint (line-list vertex buffer).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct EdgeVertex {
    pub position: [f32; 3],
    /// Linear RGB already multiplied by intensity (edges blend additively).
    pub color: [f32; 3],
    /// 1 when the frame's density scale still applies (a code edge with nothing
    /// selected), 0 when the intensity is final. Keeping density in the frame uniform
    /// means an edge insert that changes the edge count does not rewrite every edge.
    pub density: f32,
}

impl EdgeVertex {
    /// An edge slot that draws nothing (black under additive blending).
    pub const NONE: Self = Self {
        position: [0.0; 3],
        color: [0.0; 3],
        density: 0.0,
    };
}

/// sRGB transfer to linear. The target texture is sRGB, so blending happens in linear.
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear3(c: [f32; 3]) -> [f32; 3] {
    c.map(srgb_to_linear)
}

/// The selection's direct relations: `selection` cyan at 55% (style guide STG-DRAW-4).
pub const RELATION_EDGE: f32 = 0.55;

/// The colors a selection gives nodes (from the palette).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tints {
    /// A search match (STG-DRAW-2, COL-4) and the selection's relations (STG-DRAW-4).
    pub selection: [f32; 3],
    /// A code node lit as a neighbor of the selected node (section 3.4).
    pub neighbor: [f32; 3],
}

impl Default for Tints {
    fn default() -> Self {
        let p = crate::palette::Palette::default();
        Self {
            selection: p.selection,
            neighbor: p.neighbor,
        }
    }
}

/// The sprite for slot `i` drawn at `position` with its size times `scale` (an
/// appearance or removal animation), under `selection`. A search match (a member of a
/// set selection) is drawn in the selection tint (STG-DRAW-2, COL-4); a code node lit
/// as a neighbor of the selected node in the neighbor tint (section 3.4).
pub fn instance_for(
    scene: &Scene,
    i: u32,
    selection: Option<&Selection>,
    position: [f32; 3],
    scale: f32,
    tints: Tints,
) -> NodeInstance {
    if !scene.is_alive(i) {
        return NodeInstance::HIDDEN;
    }
    let n = &scene.nodes()[i as usize];
    let lit = selection.is_none_or(|s| s.is_highlighted(i));
    let (c, s) = if lit {
        (1.0, NODE_SCALE)
    } else {
        (DIM_COLOR, DIM_SCALE)
    };
    let matched = selection.is_some_and(|s| s.is_set() && s.is_highlighted(i));
    let neighbor = selection.is_some_and(|s| {
        !s.is_set()
            && s.selected() != i
            && s.is_highlighted(i)
            && scene.population(i) == Population::Code
    });
    let base = if matched {
        tints.selection
    } else if neighbor {
        tints.neighbor
    } else {
        n.color
    };
    let [r, g, b] = linear3(base);
    NodeInstance {
        pos_size: [position[0], position[1], position[2], n.size * s * scale],
        color: [r * c, g * c, b * c, scene.population(i).shape()],
    }
}

/// Node instances for the scene under `selection`, in slot order, at the scene's
/// positions.
pub fn build_instances(scene: &Scene, selection: Option<&Selection>) -> Vec<NodeInstance> {
    scene
        .nodes()
        .iter()
        .enumerate()
        .map(|(i, n)| {
            instance_for(
                scene,
                i as u32,
                selection,
                n.position,
                1.0,
                Tints::default(),
            )
        })
        .collect()
}

/// Whether an edge touches an agent or a work item.
pub fn is_overlay(scene: &Scene, e: &EdgeInput) -> bool {
    scene.population(e.source) != Population::Code || scene.population(e.target) != Population::Code
}

/// The two vertices of edge slot `e` with nothing selected, positioned by `pos`, or
/// [`EdgeVertex::NONE`] when the slot is free or an endpoint is not `drawn`. Code
/// edges carry their base intensity and defer density to the frame uniform; overlay
/// edges carry their final intensity.
pub fn unselected_edge(
    scene: &Scene,
    e: u32,
    drawn: impl Fn(u32) -> bool,
    pos: impl Fn(u32) -> [f32; 3],
) -> [EdgeVertex; 2] {
    if !scene.is_edge_alive(e) {
        return [EdgeVertex::NONE; 2];
    }
    let edge = &scene.edges()[e as usize];
    if !drawn(edge.source) || !drawn(edge.target) {
        return [EdgeVertex::NONE; 2];
    }
    let density = if is_overlay(scene, edge) { 0.0 } else { 1.0 };
    let color = linear3(edge.color).map(|c| c * edge.intensity);
    [
        EdgeVertex {
            position: pos(edge.source),
            color,
            density,
        },
        EdgeVertex {
            position: pos(edge.target),
            color,
            density,
        },
    ]
}

/// Edge vertices for the scene under `selection` and `budget`, at the scene's
/// positions. See [`build_edges_at`].
pub fn build_edges(
    scene: &Scene,
    selection: Option<&Selection>,
    budget: RenderBudget,
    drawn: impl Fn(u32) -> bool,
) -> Vec<EdgeVertex> {
    build_edges_at(
        scene,
        selection,
        budget,
        drawn,
        |i| scene.nodes()[i as usize].position,
        crate::palette::Palette::default().selection,
    )
}

/// Edge vertices for the scene under `selection` and `budget`, positioned by `pos`,
/// skipping edges with an endpoint `drawn` reports as not drawn.
///
/// Without a selection: two vertices per edge slot, in slot order, so one changed
/// edge is one small buffer write ([`unselected_edge`]); none at all on the CPU
/// budget. With one: a compact list under the selection rules above, on both budgets,
/// with density already applied.
pub fn build_edges_at(
    scene: &Scene,
    selection: Option<&Selection>,
    budget: RenderBudget,
    drawn: impl Fn(u32) -> bool,
    pos: impl Fn(u32) -> [f32; 3],
    selection_color: [f32; 3],
) -> Vec<EdgeVertex> {
    let edges = scene.edges();
    let Some(sel) = selection else {
        if !budget.edges_without_selection() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(edges.len() * 2);
        for e in 0..edges.len() as u32 {
            out.extend(unselected_edge(scene, e, &drawn, &pos));
        }
        return out;
    };
    let density = edge_intensity_scale(code_edge_count(scene));
    let mut out = Vec::new();
    for e in 0..edges.len() as u32 {
        if let Some(pair) = selected_edge(scene, sel, e, density, &drawn, &pos, selection_color) {
            out.extend(pair);
        }
    }
    out
}

/// Edge slot `e` under a selection, with density `density` applied, or `None` when
/// the rules drop it (free slot, undrawn endpoint, or no highlighted endpoint). An edge
/// of the selected node itself is one of its direct relations: `selection_color` at
/// [`RELATION_EDGE`] (STG-DRAW-4). Which edges are kept depends only on the selection
/// and liveness, so a moved endpoint changes an edge's vertices but never whether it is
/// drawn.
pub fn selected_edge(
    scene: &Scene,
    sel: &Selection,
    e: u32,
    density: f32,
    drawn: impl Fn(u32) -> bool,
    pos: impl Fn(u32) -> [f32; 3],
    selection_color: [f32; 3],
) -> Option<[EdgeVertex; 2]> {
    if !scene.is_edge_alive(e) {
        return None;
    }
    let edge = &scene.edges()[e as usize];
    if !drawn(edge.source) || !drawn(edge.target) {
        return None;
    }
    let relation = edge.source == sel.selected() || edge.target == sel.selected();
    let k = match (
        sel.is_highlighted(edge.source),
        sel.is_highlighted(edge.target),
    ) {
        (true, true) if relation => RELATION_EDGE,
        (true, true) if is_overlay(scene, edge) => OVERLAY_EDGE,
        (true, true) => EDGE_BOTH_SELECTED,
        (true, false) | (false, true) => EDGE_HALF_SELECTED * density,
        (false, false) => return None,
    };
    let base = if relation && k == RELATION_EDGE {
        selection_color
    } else {
        edge.color
    };
    let color = linear3(base).map(|c| c * k);
    Some([
        EdgeVertex {
            position: pos(edge.source),
            color,
            density: 0.0,
        },
        EdgeVertex {
            position: pos(edge.target),
            color,
            density: 0.0,
        },
    ])
}

/// Live edges between two code nodes: the count the density rule scales by. Overlay
/// edges are excluded so a busy agent does not dim the code field.
pub fn code_edge_count(scene: &Scene) -> usize {
    scene
        .edges()
        .iter()
        .enumerate()
        .filter(|(i, e)| scene.is_edge_alive(*i as u32) && !is_overlay(scene, e))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{NodeInput, SceneInput};

    /// 0 - 1 - 2 - 3, plus 4 isolated.
    fn chain() -> Scene {
        let nodes = (0..5)
            .map(|i| NodeInput {
                position: [i as f32 * 10.0, 0.0, 0.0],
                color: [1.0, 1.0, 1.0],
                size: 5.0,
            })
            .collect();
        let edge = |s, t| EdgeInput {
            source: s,
            target: t,
            color: [1.0, 1.0, 1.0],
            intensity: 0.25,
        };
        Scene::new(SceneInput {
            nodes,
            edges: vec![edge(0, 1), edge(1, 2), edge(2, 3)],
        })
        .expect("valid")
    }

    fn lit(v: &[EdgeVertex]) -> usize {
        v.iter().filter(|x| x.color != [0.0; 3]).count()
    }

    #[test]
    fn density_constants_are_the_evidence_values() {
        // Stated literally so a changed constant in the graph crate fails here.
        assert_eq!(
            (DIM_COLOR, DIM_SCALE, NODE_SCALE),
            (0.15, 0.2, 0.5),
            "dimming"
        );
        assert_eq!((EDGE_BOTH_SELECTED, EDGE_HALF_SELECTED), (0.5, 0.04));
        assert_eq!(edge_intensity_scale(10_000), 0.5);
    }

    #[test]
    fn one_hop_highlights_self_and_neighbors_only() {
        let scene = chain();
        let sel = Selection::one_hop(&scene, 1).expect("in scene");
        assert_eq!(sel.members(), &[1, 0, 2]);
        assert!(!sel.is_highlighted(3) && !sel.is_highlighted(4));
        assert!(Selection::one_hop(&scene, 5).is_none());
        // Extra nodes (a proxy's children) are lit too; dead ones are skipped.
        let sel = Selection::one_hop_and(&scene, 4, &[3, 99]).expect("in scene");
        assert_eq!(sel.members(), &[4, 3]);
        assert!(sel.is_highlighted(3) && !sel.is_explicit(3));
    }

    #[test]
    fn non_highlighted_nodes_are_dimmed_and_shrunk() {
        let scene = chain();
        let sel = Selection::one_hop(&scene, 1).expect("in scene");
        let inst = build_instances(&scene, Some(&sel));
        assert_eq!(inst[1].color[0], 1.0);
        // Evidence values, stated literally so a changed constant fails here: highlighted
        // scale 0.5, dimmed scale 0.2 with color 0.15.
        assert_eq!(inst[1].pos_size[3], 2.5);
        assert!((inst[3].color[0] - 0.15).abs() < 1e-6);
        assert!((inst[3].pos_size[3] - 1.0).abs() < 1e-6);
        let none = build_instances(&scene, None);
        assert!(
            none.iter()
                .all(|i| i.color[0] == 1.0 && i.pos_size[3] == 2.5 && i.color[3] == 1.0)
        );
    }

    /// Section 3.4: a code node lit as the selected node's neighbor takes the neighbor
    /// tint; the selected node and unlit nodes keep their own color.
    #[test]
    fn a_lit_neighbor_takes_the_neighbor_tint() {
        let scene = chain();
        let sel = Selection::one_hop(&scene, 1).expect("in scene");
        let tints = Tints {
            selection: [0.0, 1.0, 0.0],
            neighbor: [0.5, 0.25, 1.0],
        };
        let at = |i: u32| instance_for(&scene, i, Some(&sel), [0.0; 3], 1.0, tints).color;
        let neighbor = linear3(tints.neighbor);
        for i in [0, 2] {
            assert_eq!(at(i)[..3], neighbor, "neighbor {i}");
        }
        assert_eq!(at(1)[..3], [1.0; 3], "the selected node keeps its color");
        let dim = at(3);
        assert!(
            (dim[0] - DIM_COLOR).abs() < 1e-6 && dim[0] == dim[2],
            "unlit"
        );
    }

    #[test]
    fn free_slots_are_hidden_and_populations_carry_their_shape() {
        let mut scene = chain();
        scene.remove_node(4).expect("remove");
        let agent = scene
            .insert_node(
                NodeInput {
                    position: [0.0; 3],
                    color: [0.3, 1.0, 0.85],
                    size: 14.0,
                },
                Population::Agent,
            )
            .expect("agent");
        assert_eq!(agent, 4, "reused slot");
        scene.remove_node(3).expect("remove");
        let inst = build_instances(&scene, None);
        assert!(inst[3].is_hidden());
        assert_eq!(inst[4].color[3], Population::Agent.shape());
        assert_eq!(inst[0].color[3], Population::Code.shape());
    }

    #[test]
    fn edge_rules_under_selection() {
        let scene = chain();
        let sel = Selection::one_hop(&scene, 1).expect("in scene");
        let v = build_edges(&scene, Some(&sel), RenderBudget::Cpu, |_| true);
        // 0-1 and 1-2 are the selected node's own relations: selection cyan at 55%
        // (STG-DRAW-4); 2-3 half highlighted (0.04, gray); none dropped here.
        assert_eq!(v.len(), 6);
        let cyan = linear3(crate::palette::Palette::default().selection).map(|c| c * RELATION_EDGE);
        assert_eq!(v[0].color, cyan);
        assert_eq!(v[2].color, cyan);
        assert!((v[4].color[0] - EDGE_HALF_SELECTED).abs() < 1e-6);
        assert!(v[4].color[0] == v[4].color[2], "not a relation: stays gray");
        assert!(v.iter().all(|x| x.density == 0.0), "final intensities");

        // Selecting the isolated node drops every edge.
        let lone = Selection::one_hop(&scene, 4).expect("in scene");
        assert!(build_edges(&scene, Some(&lone), RenderBudget::Gpu, |_| true).is_empty());

        // Two highlighted neighbors joined by an edge that is not the selection's own:
        // the both-highlighted gray, not cyan.
        let mut tri = chain();
        tri.insert_edge(EdgeInput {
            source: 0,
            target: 2,
            color: [1.0; 3],
            intensity: 0.25,
        })
        .expect("edge");
        let sel = Selection::one_hop(&tri, 1).expect("in scene");
        let v = build_edges(&tri, Some(&sel), RenderBudget::Gpu, |_| true);
        let side = v
            .chunks(2)
            .find(|p| p[0].position == [0.0; 3] && p[1].position == [20.0, 0.0, 0.0])
            .expect("0-2 drawn");
        assert!((side[0].color[0] - EDGE_BOTH_SELECTED).abs() < 1e-6);
    }

    #[test]
    fn overlay_edges_use_their_own_intensity_and_skip_density() {
        let mut scene = chain();
        let agent = scene
            .insert_node(
                NodeInput {
                    position: [0.0, 0.0, 100.0],
                    color: [1.0; 3],
                    size: 14.0,
                },
                Population::Agent,
            )
            .expect("agent");
        let e = scene
            .insert_edge(EdgeInput {
                source: agent,
                target: 2,
                color: [1.0; 3],
                intensity: 0.3,
            })
            .expect("edge");
        let v = unselected_edge(&scene, e, |_| true, |i| scene.nodes()[i as usize].position);
        assert!((v[0].color[0] - 0.3).abs() < 1e-6 && v[0].density == 0.0);
        let code = unselected_edge(&scene, 0, |_| true, |i| scene.nodes()[i as usize].position);
        assert_eq!(code[0].density, 1.0);
        assert_eq!(code_edge_count(&scene), 3, "the agent edge is not counted");

        let sel = Selection::one_hop(&scene, agent).expect("agent");
        let v = build_edges(&scene, Some(&sel), RenderBudget::Gpu, |_| true);
        let agent_edge = v
            .chunks(2)
            .find(|p| p[0].position == [0.0, 0.0, 100.0])
            .expect("agent edge drawn");
        // The selected agent's own edge is one of its relations: cyan at 55%.
        let cyan = linear3(crate::palette::Palette::default().selection).map(|c| c * RELATION_EDGE);
        assert_eq!(agent_edge[0].color, cyan);
    }

    #[test]
    fn cpu_budget_draws_edges_only_for_a_selection() {
        let scene = chain();
        assert!(build_edges(&scene, None, RenderBudget::Cpu, |_| true).is_empty());
        let gpu = build_edges(&scene, None, RenderBudget::Gpu, |_| true);
        assert_eq!(gpu.len(), 6);
        assert!((gpu[0].color[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn edges_to_undrawn_nodes_are_skipped() {
        let scene = chain();
        let v = build_edges(&scene, None, RenderBudget::Gpu, |i| i != 2);
        // Slot order keeps every pair; only 0-1 is lit, 1-2 and 2-3 touch the hidden node.
        assert_eq!(v.len(), 6);
        assert_eq!(lit(&v), 2);
        assert!(v[0].color != [0.0; 3]);
    }

    #[test]
    fn a_set_selection_lights_exactly_its_members_and_skips_dead_slots() {
        let mut scene = chain();
        let sel = Selection::of_set(&scene, 3, &[0, 3, 3]).expect("live");
        assert_eq!(sel.selected(), 3);
        assert_eq!(sel.members(), &[3, 0], "no neighbors are added, no repeats");
        assert!(!sel.is_highlighted(1) && !sel.is_highlighted(2));
        scene.remove_node(3).expect("remove");
        let sel = Selection::of_set(&scene, 3, &[3, 4]).expect("4 is live");
        assert_eq!(sel.selected(), 4);
        assert!(Selection::of_set(&scene, 3, &[3]).is_none());
    }

    #[test]
    fn centroid_and_spread_of_neighborhood() {
        let scene = chain();
        let sel = Selection::one_hop(&scene, 1).expect("in scene");
        let (c, s) = sel.centroid_and_spread(&scene);
        assert!((c - Vec3::new(10.0, 0.0, 0.0)).length() < 1e-5);
        assert!((s - 10.0).abs() < 1e-5);
    }
}
