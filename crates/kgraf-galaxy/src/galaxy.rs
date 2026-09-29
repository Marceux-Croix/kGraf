//! The galaxy's state, independent of egui: scene, camera, selection, GPU buffers,
//! live changes with their animation, and the redraw decision.
//!
//! Redraw on change (0054, and the shell review's idle-repaint finding): a frame is
//! rendered only when the camera moved, the scene or selection changed, an animation
//! is running, or the target was resized. Everything else reuses the last frame in the
//! target texture.
//!
//! Live changes (0015, 0058). The graph changes while the user watches: nodes appear,
//! disappear, and move when their call depth or their cluster's size bucket changes.
//! Each change is applied to its own slot. With nothing selected and no CPU grouping,
//! GPU buffers are in slot order, so a change batch rewrites only the bytes of the
//! slots it touched ("partial upload"). A selection, or CPU-budget grouping, orders
//! the buffers differently, and then a change re-uploads them whole; both are
//! bounded (a selection is a user action, grouping applies above 10,000 nodes on a
//! software rasterizer).
//!
//! Animation state machine, per slot:
//! ```text
//!   Still --insert--> Appearing (scale 0 -> 1) --done--> Still
//!   Still --move----> Moving (lerp to the new position) --done--> Still
//!   Still --remove--> Vanishing (scale -> 0; edges gone at once) --done--> slot freed
//! ```
//! A move during a move restarts from where the node is drawn now, so nothing jumps.
//! On the CPU budget with grouping active nothing animates: a change is applied at
//! once, because every animated frame there would regroup the whole scene.
//!
//! Folding (decision 0068, [`crate::fold`]). The owner of the scene gives a fold with
//! [`Galaxy::set_fold`]; a folded node is neither drawn nor picked, its edges are not
//! drawn, and its drawn ancestor grows with what it hides. The fold's own transitions:
//! ```text
//!   Drawn --fold-----> Folding (to the carrier's position, scale -> 0) --done--> Hidden
//!   Hidden --unfold--> Unfolding (from the carrier's position, scale 0 -> 1) --> Drawn
//! ```
//! Both are ordinary tweens; a folding node stops being pickable at once, as a
//! vanishing one does. A selection's explicit nodes (the selected node, or every match
//! of a search) are drawn even when folded. Selecting a proxy lights its hierarchy
//! children as well as its edges (it has none), so an expanded directory's contents
//! are not dimmed. On the CPU budget a fold also compacts the upload to what is drawn
//! (as grouping does): a software rasterizer pays per instance, hidden or not.

use std::collections::HashMap;

use glam::Vec3;

use crate::budget::{CPU_GROUP_GRID, RenderBudget};
use crate::camera::{CameraFrame, OrbitCamera};
use crate::error::GalaxyError;
use crate::fold::{Fold, Hierarchy};
use crate::gpu::{GalaxyRenderer, RenderTarget, TARGET_FORMAT};
use crate::lod::Visibility;
use crate::pick::{pick_detail, within_reach};
use crate::scene::{EdgeInput, NodeInput, Population, Scene};
use crate::selection::{
    DIM_COLOR, DIM_SCALE, EdgeVertex, NODE_SCALE, NodeInstance, Selection, build_edges_at,
    code_edge_count, edge_intensity_scale, instance_for, selected_edge, srgb_to_linear,
    unselected_edge,
};

/// How long an appearance, removal or move takes. Chosen, not measured: long enough
/// to read as motion (0015: an un-animated change looks like a rendering fault), short
/// enough that a burst of reindex changes settles within a second.
pub const TWEEN_SECONDS: f32 = 0.6;

/// Dirty slots closer than this are written as one run: one larger write beats many
/// tiny ones, and the bytes in between are current anyway.
const RUN_GAP: u32 = 16;

/// The start view: the live nodes' bounding sphere, framed.
fn home_camera(scene: &Scene) -> OrbitCamera {
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut any = false;
    for (i, n) in scene.nodes().iter().enumerate() {
        if !scene.is_alive(i as u32) {
            continue;
        }
        any = true;
        let p = Vec3::from(n.position);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    if !any {
        return OrbitCamera::default();
    }
    let center = (lo + hi) * 0.5;
    OrbitCamera::framing(center, (hi - lo).length() * 0.5)
}

/// The visibility for `budget`: everything on the GPU budget; on the CPU budget the
/// overlay and the highest-degree nodes up to the cap, the rest grouped. Folded nodes
/// are left to their drawn ancestor in both.
fn visibility_for(scene: &Scene, budget: RenderBudget, folded: &[bool]) -> Visibility {
    match budget.node_cap() {
        Some(cap) => Visibility::capped(scene, cap, CPU_GROUP_GRID, |i| {
            folded.get(i as usize).copied().unwrap_or(false)
        }),
        None => Visibility::all(scene),
    }
}

/// Sprites to upload outside slot order: drawn nodes (highlighted last, so their halos
/// blend over dimmed ones; cores are ordered by the depth buffer), then group sprites.
/// The CPU copy stays in slot order because picking returns slots. Also returns where
/// each slot landed in the list ([`NOT_UPLOADED`] if nowhere), so a moving node can
/// be rewritten in place.
fn upload_list(
    instances: &[NodeInstance],
    drawn: impl Fn(u32) -> bool,
    vis: &Visibility,
    selection: Option<&Selection>,
) -> (Vec<NodeInstance>, Vec<u32>) {
    let mut out = Vec::with_capacity(instances.len().min(vis.shown_count() + 64));
    let mut index = vec![NOT_UPLOADED; instances.len()];
    let highlighted = |i: u32| selection.is_some_and(|s| s.is_highlighted(i));
    for (i, inst) in instances.iter().enumerate() {
        let slot = i as u32;
        if !inst.is_hidden() && drawn(slot) && !highlighted(slot) {
            index[i] = out.len() as u32;
            out.push(*inst);
        }
    }
    if let Some(sel) = selection {
        for &m in sel.members() {
            let inst = instances[m as usize];
            if !inst.is_hidden() {
                index[m as usize] = out.len() as u32;
                out.push(inst);
            }
        }
    }
    let (c, k) = if selection.is_some() {
        (DIM_COLOR, DIM_SCALE)
    } else {
        (1.0, NODE_SCALE)
    };
    out.extend(vis.groups().iter().map(|g| {
        let [r, gg, b] = g.color.map(srgb_to_linear);
        NodeInstance {
            pos_size: [g.position[0], g.position[1], g.position[2], g.size * k],
            color: [r * c, gg * c, b * c, Population::Code.shape()],
        }
    }));
    (out, index)
}

/// A slot or edge with no place in the uploaded list.
const NOT_UPLOADED: u32 = u32::MAX;

/// Sorted, deduplicated dirty slots grouped into `[first, last]` runs.
fn runs(mut slots: Vec<u32>) -> Vec<(u32, u32)> {
    slots.sort_unstable();
    slots.dedup();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for s in slots {
        match out.last_mut() {
            Some((_, last)) if s <= last.saturating_add(RUN_GAP) => *last = s,
            _ => out.push((s, s)),
        }
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct Tween {
    from: [f32; 3],
    to: [f32; 3],
    from_scale: f32,
    to_scale: f32,
    elapsed: f32,
    /// Free the slot when done (a removal).
    then_remove: bool,
    /// Folding into a parent: drawn until done, then hidden by the fold.
    then_fold: bool,
}

/// What changed since the last upload.
#[derive(Debug, Default)]
struct Dirty {
    /// Re-upload every buffer.
    full: bool,
    /// Slots were added or freed, or edges changed: visibility, selection and the
    /// density scale are recomputed before the next upload.
    structure: bool,
    /// The fold changed: the CPU visibility is recomputed, and reordered lists (a
    /// selection or grouping) are uploaded whole.
    fold: bool,
    nodes: Vec<u32>,
    edges: Vec<u32>,
}

impl Dirty {
    fn any(&self) -> bool {
        self.full || self.structure || self.fold || !self.nodes.is_empty() || !self.edges.is_empty()
    }
}

/// Upload counters, for tests and the benchmark: they show whether a change was
/// applied in place or forced a whole-scene upload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadStats {
    /// Uploads that rewrote every instance and edge.
    pub full: u64,
    /// Uploads that rewrote only changed slots.
    pub partial: u64,
    /// Bytes written by the last upload.
    pub last_bytes: u64,
}

/// The galaxy view's state.
#[derive(Debug)]
pub struct Galaxy {
    scene: Scene,
    budget: RenderBudget,
    camera: OrbitCamera,
    selection: Option<Selection>,
    /// Which nodes the budget draws individually (all, on the GPU budget).
    visibility: Visibility,
    /// Where each slot is drawn now (differs from the scene while animating).
    shown_pos: Vec<[f32; 3]>,
    /// Per slot: hidden in a folded parent ([`Self::set_fold`]).
    folded: Vec<bool>,
    /// Per slot: the fold's size multiplier (a folded parent grows).
    fold_scale: Vec<f32>,
    /// Per slot: the drawn ancestor a folded slot hides in, for the unfold animation.
    fold_carrier: Vec<u32>,
    folded_count: usize,
    /// Hierarchy children of each proxy (they have no edges): lit with its selection.
    proxy_children: HashMap<u32, Vec<u32>>,
    /// The camera distance at "Fit", for the fit scale (style guide STG-CAM-1).
    fit_distance: f32,
    /// The selection's relations and search matches (the palette's selection color).
    selection_color: [f32; 3],
    /// A code node lit as the selected node's neighbor (the palette's neighbor color).
    neighbor_color: [f32; 3],
    /// Size multiplier per slot (0 to 1 while appearing or vanishing).
    shown_scale: Vec<f32>,
    tweens: HashMap<u32, Tween>,
    /// CPU copy of every slot's sprite, for picking at drawn sizes and partial writes.
    instances: Vec<NodeInstance>,
    /// The density scale for unselected code edges (frame uniform).
    edge_density: f32,
    renderer: GalaxyRenderer,
    dirty: Dirty,
    /// The camera frame of the last render; a different frame means redraw.
    last_rendered: Option<CameraFrame>,
    frames_rendered: u64,
    uploads: UploadStats,
    /// With a selection: where each slot's sprite and each edge's first vertex sit in
    /// the uploaded (reordered, compacted) lists. Empty in slot order.
    sprite_at: Vec<u32>,
    edge_at: Vec<u32>,
}

impl Galaxy {
    pub fn new(device: &wgpu::Device, scene: Scene, budget: RenderBudget) -> Self {
        let camera = home_camera(&scene);
        let visibility = visibility_for(&scene, budget, &[]);
        let fit_distance = camera.distance();
        let mut g = Self {
            scene,
            budget,
            camera,
            selection: None,
            visibility,
            shown_pos: Vec::new(),
            folded: Vec::new(),
            fold_scale: Vec::new(),
            fold_carrier: Vec::new(),
            folded_count: 0,
            proxy_children: HashMap::new(),
            fit_distance,
            selection_color: crate::palette::Palette::default().selection,
            neighbor_color: crate::palette::Palette::default().neighbor,
            shown_scale: Vec::new(),
            tweens: HashMap::new(),
            instances: Vec::new(),
            edge_density: 1.0,
            renderer: GalaxyRenderer::new(device, TARGET_FORMAT),
            dirty: Dirty::default(),
            last_rendered: None,
            frames_rendered: 0,
            uploads: UploadStats::default(),
            sprite_at: Vec::new(),
            edge_at: Vec::new(),
        };
        g.reset_display();
        g
    }

    /// Display state from the scene as it is: no animation, no fold, everything
    /// re-uploaded.
    fn reset_display(&mut self) {
        self.tweens.clear();
        let n = self.scene.nodes().len();
        self.shown_pos = self.scene.nodes().iter().map(|n| n.position).collect();
        self.shown_scale = vec![1.0; n];
        self.folded = vec![false; n];
        self.fold_scale = vec![1.0; n];
        self.fold_carrier = vec![u32::MAX; n];
        self.folded_count = 0;
        self.visibility = visibility_for(&self.scene, self.budget, &self.folded);
        self.edge_density = edge_intensity_scale(code_edge_count(&self.scene));
        self.rebuild_instances();
        self.dirty = Dirty {
            full: true,
            ..Dirty::default()
        };
    }

    fn rebuild_instances(&mut self) {
        self.instances = (0..self.scene.nodes().len() as u32)
            .map(|i| self.instance_at(i))
            .collect();
    }

    /// Whether the fold hides `i` from drawing now: folded, not named by the
    /// selection, and not still animating into its parent.
    fn fold_hides(&self, i: u32) -> bool {
        self.is_folded(i)
            && !self.selection.as_ref().is_some_and(|s| s.is_explicit(i))
            && !self.tweens.get(&i).is_some_and(|t| t.then_fold)
    }

    /// Whether the fold hides `i` (folded into a parent; animation aside).
    pub fn is_folded(&self, i: u32) -> bool {
        self.folded.get(i as usize).copied().unwrap_or(false)
    }

    /// Slot `i`'s sprite as drawn now: its animated position and scale, the fold's
    /// growth, and nothing at all when the fold hides it.
    fn instance_at(&self, i: u32) -> NodeInstance {
        if self.fold_hides(i) {
            return NodeInstance::HIDDEN;
        }
        let k = self.fold_scale.get(i as usize).copied().unwrap_or(1.0);
        instance_for(
            &self.scene,
            i,
            self.selection.as_ref(),
            self.shown_pos[i as usize],
            self.shown_scale[i as usize] * k,
            crate::selection::Tints {
                selection: self.selection_color,
                neighbor: self.neighbor_color,
            },
        )
    }

    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    pub fn budget(&self) -> RenderBudget {
        self.budget
    }

    /// Replaces the scene (a reload). The selection is cleared: its slots refer to the
    /// old scene. The camera stays where it is; call [`Self::frame_all`] to reframe.
    pub fn set_scene(&mut self, scene: Scene) {
        self.scene = scene;
        self.selection = None;
        self.reset_display();
    }

    /// Moves the camera to frame every live node ("Fit").
    pub fn frame_all(&mut self) {
        self.camera = home_camera(&self.scene);
        self.fit_distance = self.camera.distance();
    }

    /// The camera scale over the fit scale (STG-CAM-1): 1 at "Fit", 2 when zoomed in
    /// twice as close. The camera scale is inversely proportional to the distance.
    pub fn zoom_over_fit(&self) -> f32 {
        let d = self.camera.distance();
        if d > 0.0 && self.fit_distance > 0.0 {
            self.fit_distance / d
        } else {
            1.0
        }
    }

    /// The stage background and a work card's fill (the palette's stage colors).
    pub fn set_stage(&mut self, background: [f32; 3], card: [f32; 3]) {
        self.renderer.set_stage(background, card);
        // A frame drawn with the old colors must be redrawn.
        self.dirty.full = true;
    }

    /// The color of the selection's relations and of search matches, and of a code
    /// node lit as the selected node's neighbor.
    pub fn set_selection_colors(&mut self, selection: [f32; 3], neighbor: [f32; 3]) {
        if (selection, neighbor) != (self.selection_color, self.neighbor_color) {
            self.selection_color = selection;
            self.neighbor_color = neighbor;
            self.rebuild_instances();
            self.dirty.full = true;
        }
    }

    /// One-hop neighbors, plus a proxy's hierarchy children.
    fn select_around(&self, n: u32) -> Option<Selection> {
        let extra = self
            .proxy_children
            .get(&n)
            .map_or(&[][..], |v| v.as_slice());
        Selection::one_hop_and(&self.scene, n, extra)
    }

    pub fn camera(&self) -> &OrbitCamera {
        &self.camera
    }

    /// Input goes through here; `update` reports whether it produced motion.
    pub fn camera_mut(&mut self) -> &mut OrbitCamera {
        &mut self.camera
    }

    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// Selects `node` and its one-hop neighborhood and flies the camera to its
    /// centroid, or clears the selection with `None`. Unchanged selection is a no-op;
    /// a node that is vanishing cannot be selected.
    pub fn select(&mut self, node: Option<u32>) {
        if self.selection.as_ref().map(Selection::selected) == node {
            return;
        }
        let node = node.filter(|&n| !self.is_vanishing(n));
        self.selection = node.and_then(|n| self.select_around(n));
        if let Some(sel) = &self.selection {
            let (centroid, spread) = sel.centroid_and_spread(&self.scene);
            self.camera.fly_to(centroid, spread);
        }
        self.rebuild_instances();
        self.dirty.full = true;
    }

    /// Selects a set of nodes (a search result) and flies the camera to their
    /// centroid; an empty set clears the selection. The first live node of
    /// `nodes` is the primary. Vanishing nodes are left out.
    pub fn select_set(&mut self, nodes: &[u32]) {
        let live: Vec<u32> = nodes
            .iter()
            .copied()
            .filter(|&n| !self.is_vanishing(n))
            .collect();
        let sel = live
            .first()
            .and_then(|&p| Selection::of_set(&self.scene, p, &live));
        if sel == self.selection {
            return;
        }
        self.selection = sel;
        if let Some(sel) = &self.selection {
            let (centroid, spread) = sel.centroid_and_spread(&self.scene);
            self.camera.fly_to(centroid, spread);
        }
        self.rebuild_instances();
        self.dirty.full = true;
    }

    /// Whether `slot` is animating out (still drawn, no longer part of the graph).
    pub fn is_vanishing(&self, slot: u32) -> bool {
        self.tweens.get(&slot).is_some_and(|t| t.then_remove)
    }

    /// Whether any node is animating.
    pub fn is_animating(&self) -> bool {
        !self.tweens.is_empty()
    }

    /// Whether changes animate now (not while the CPU budget groups nodes).
    fn animates(&self) -> bool {
        self.visibility.groups().is_empty()
    }

    fn grow_display(&mut self, slot: u32) {
        let n = slot as usize + 1;
        if self.shown_pos.len() < n {
            self.shown_pos.resize(n, [0.0; 3]);
            self.shown_scale.resize(n, 1.0);
            self.instances.resize(n, NodeInstance::HIDDEN);
            self.folded.resize(n, false);
            self.fold_scale.resize(n, 1.0);
            self.fold_carrier.resize(n, u32::MAX);
        }
    }

    fn touch_node(&mut self, slot: u32, with_edges: bool) {
        self.instances[slot as usize] = self.instance_at(slot);
        self.dirty.nodes.push(slot);
        if with_edges {
            self.dirty
                .edges
                .extend_from_slice(self.scene.incident_edges(slot));
        }
    }

    /// An animated slot's sprite, and only the edges it draws: an edge with an end that
    /// is not drawn is empty, and a position change leaves it empty (whatever made it
    /// empty wrote it then). In a folded graph most edges of a moving node run into
    /// folded code, so this is most of the animation's upload.
    fn touch_moved(&mut self, slot: u32) {
        self.touch_node(slot, false);
        if !self.is_drawn(slot) {
            return;
        }
        let edges: Vec<u32> = self
            .scene
            .incident_edges(slot)
            .iter()
            .copied()
            .filter(|&e| {
                let edge = self.scene.edges()[e as usize];
                let other = if edge.source == slot {
                    edge.target
                } else {
                    edge.source
                };
                self.is_drawn(other)
            })
            .collect();
        self.dirty.edges.extend(edges);
    }

    fn require_settled(&self, slot: u32) -> Result<(), GalaxyError> {
        if self.is_vanishing(slot) || !self.scene.is_alive(slot) {
            return Err(GalaxyError::InvalidScene(format!(
                "node slot {slot} is not live"
            )));
        }
        Ok(())
    }

    /// Adds a node; it grows in from nothing at its position. Returns its slot.
    pub fn insert_node(
        &mut self,
        node: NodeInput,
        population: Population,
    ) -> Result<u32, GalaxyError> {
        let slot = self.scene.insert_node(node, population)?;
        self.grow_display(slot);
        self.shown_pos[slot as usize] = node.position;
        // A reused slot starts unfolded; the owner's next fold places it.
        if std::mem::take(&mut self.folded[slot as usize]) {
            self.folded_count -= 1;
        }
        self.fold_scale[slot as usize] = 1.0;
        self.fold_carrier[slot as usize] = u32::MAX;
        if self.animates() {
            self.shown_scale[slot as usize] = 0.0;
            self.tweens.insert(
                slot,
                Tween {
                    from: node.position,
                    to: node.position,
                    from_scale: 0.0,
                    to_scale: 1.0,
                    elapsed: 0.0,
                    then_remove: false,
                    then_fold: false,
                },
            );
        } else {
            self.shown_scale[slot as usize] = 1.0;
        }
        self.touch_node(slot, false);
        self.dirty.structure = true;
        Ok(slot)
    }

    /// Removes a node: its edges go at once, the node shrinks away, then its slot is
    /// freed.
    pub fn remove_node(&mut self, slot: u32) -> Result<(), GalaxyError> {
        self.require_settled(slot)?;
        let edges = self.scene.incident_edges(slot).to_vec();
        for e in edges {
            self.scene.remove_edge(e)?;
            self.dirty.edges.push(e);
        }
        if self.animates() {
            let at = self.shown_pos[slot as usize];
            self.tweens.insert(
                slot,
                Tween {
                    from: at,
                    to: at,
                    from_scale: self.shown_scale[slot as usize],
                    to_scale: 0.0,
                    elapsed: 0.0,
                    then_remove: true,
                    then_fold: false,
                },
            );
        } else {
            self.scene.remove_node(slot)?;
            self.tweens.remove(&slot);
        }
        self.touch_node(slot, false);
        self.dirty.structure = true;
        Ok(())
    }

    /// Moves a node to `position`, animated from where it is drawn now.
    pub fn move_node(&mut self, slot: u32, position: [f32; 3]) -> Result<(), GalaxyError> {
        self.require_settled(slot)?;
        let mut node = self.scene.nodes()[slot as usize];
        node.position = position;
        self.scene.set_node(slot, node)?;
        if self.animates() {
            let scale = self.shown_scale[slot as usize];
            let to_scale = self.tweens.get(&slot).map_or(scale, |t| t.to_scale);
            self.tweens.insert(
                slot,
                Tween {
                    from: self.shown_pos[slot as usize],
                    to: position,
                    from_scale: scale,
                    to_scale,
                    elapsed: 0.0,
                    then_remove: false,
                    then_fold: false,
                },
            );
        } else {
            self.shown_pos[slot as usize] = position;
        }
        // The node's sprite and its incident edges are rewritten in place, with a
        // selection too (their places in the reordered lists are recorded).
        self.touch_node(slot, true);
        Ok(())
    }

    /// Changes a node's color and size (a degree change), in place.
    pub fn restyle_node(
        &mut self,
        slot: u32,
        color: [f32; 3],
        size: f32,
    ) -> Result<(), GalaxyError> {
        self.require_settled(slot)?;
        let mut node = self.scene.nodes()[slot as usize];
        node.color = color;
        node.size = size;
        self.scene.set_node(slot, node)?;
        self.touch_node(slot, false);
        Ok(())
    }

    /// Adds an edge between two settled nodes. Returns its slot.
    pub fn insert_edge(&mut self, edge: EdgeInput) -> Result<u32, GalaxyError> {
        self.require_settled(edge.source)?;
        self.require_settled(edge.target)?;
        let slot = self.scene.insert_edge(edge)?;
        self.dirty.edges.push(slot);
        self.dirty.structure = true;
        Ok(slot)
    }

    /// Removes an edge.
    pub fn remove_edge(&mut self, slot: u32) -> Result<(), GalaxyError> {
        self.scene.remove_edge(slot)?;
        self.dirty.edges.push(slot);
        self.dirty.structure = true;
        Ok(())
    }

    /// Changes an edge's color and intensity.
    pub fn set_edge_look(
        &mut self,
        slot: u32,
        color: [f32; 3],
        intensity: f32,
    ) -> Result<(), GalaxyError> {
        self.scene.set_edge_look(slot, color, intensity)?;
        // A look change never changes whether an edge is drawn, so it is written in
        // place in either upload order.
        self.dirty.edges.push(slot);
        Ok(())
    }

    /// The node under `cursor` in a `width` by `height` pixel view: the depth pick of
    /// [`crate::pick`]. The selected node wins only over a hit that is merely within
    /// reach (no core under the cursor), never over another node's drawn core: an
    /// earlier rule that let it win anywhere in its disc swallowed 3 to 7.5% of clicks
    /// on its own children (2026-09-27). The fold handles ("+N", "-") are the
    /// sure way to fold a parent hidden behind its children.
    pub fn pick(&self, cursor: [f32; 2], width: u32, height: u32) -> Option<u32> {
        let frame = self.camera.frame(width as f32, height as f32);
        let pickable = |i: u32| self.is_drawn(i) && !self.is_vanishing(i);
        let max = self.budget.max_core_px();
        let best = pick_detail(&self.instances, pickable, &frame, cursor, max);
        if let Some((_, false)) = best
            && let Some(sel) = self.selection.as_ref().map(Selection::selected)
            && pickable(sel)
            && self
                .instances
                .get(sel as usize)
                .is_some_and(|inst| within_reach(inst, &frame, cursor, max))
        {
            return Some(sel);
        }
        best.map(|(s, _)| s)
    }

    /// Whether node `i` is drawn: live, not folded away, and shown by the budget or
    /// highlighted by the selection (a selected node or neighbor is always drawn
    /// individually). A selection's explicit nodes are drawn even when folded.
    pub fn is_drawn(&self, i: u32) -> bool {
        let sel = self.selection.as_ref();
        self.scene.is_alive(i)
            && (sel.is_some_and(|s| s.is_explicit(i))
                || (!self.is_folded(i)
                    && (self.visibility.is_shown(i) || sel.is_some_and(|s| s.is_highlighted(i)))))
    }

    /// Slots the fold hides now.
    pub fn folded_count(&self) -> usize {
        self.folded_count
    }

    /// Applies a fold ([`crate::fold::resolve`]): slots it hides stop being drawn,
    /// picked and labeled, and a drawn proxy grows with what it hides (a code dot does
    /// not: its radius is its connection count's, STG-DRAW-2). With `animate` (and
    /// nothing grouped), a newly folded slot shrinks into its carrier and a newly
    /// unfolded one grows out of it; without, the change is immediate (a load). Slots
    /// beyond the fold's length, and non-members, are never folded. `hierarchy` gives
    /// the proxies' children, which a proxy's selection lights.
    pub fn set_fold(&mut self, fold: &Fold, hierarchy: &Hierarchy, animate: bool) {
        let n = self.scene.nodes().len();
        // Proxies and work nodes have no edges to their children: their selection
        // lights them from the hierarchy.
        self.proxy_children = (0..n as u32)
            .filter(|&i| {
                self.scene.is_alive(i)
                    && matches!(
                        self.scene.population(i),
                        Population::Cluster | Population::Work
                    )
                    && !hierarchy.children(i).is_empty()
            })
            .map(|i| (i, hierarchy.children(i).to_vec()))
            .collect();
        if let Some(sel) = self.selection.as_ref().map(Selection::selected)
            && matches!(
                self.scene.population(sel),
                Population::Cluster | Population::Work
            )
        {
            // Its children may have changed: light the current ones.
            self.selection = self.select_around(sel);
            self.rebuild_instances();
            self.dirty.full = true;
        }
        let animate = animate && self.animates();
        let mut changed = 0usize;
        // Slots whose fold state flipped, ascending.
        let mut flipped: Vec<u32> = Vec::new();
        for i in 0..n as u32 {
            let s = i as usize;
            let hide = fold.is_hidden(i) && self.scene.is_alive(i);
            let scale = if self.scene.population(i) == Population::Cluster {
                fold.scale(i)
            } else {
                1.0
            };
            let carrier = fold.carrier(i).unwrap_or(u32::MAX);
            let was = self.folded[s];
            if was == hide && self.fold_scale[s] == scale {
                self.fold_carrier[s] = carrier;
                continue;
            }
            changed += 1;
            if was != hide {
                flipped.push(i);
                self.folded[s] = hide;
                if hide {
                    self.folded_count += 1;
                } else {
                    self.folded_count -= 1;
                }
                if self.is_vanishing(i) {
                    // A removal is already under way; it ends the slot either way.
                } else if animate {
                    let from = if hide { carrier } else { self.fold_carrier[s] };
                    self.fold_tween(i, hide, from);
                } else {
                    self.tweens.remove(&i);
                    self.shown_pos[s] = self.scene.nodes()[s].position;
                    self.shown_scale[s] = 1.0;
                }
            }
            self.fold_scale[s] = scale;
            self.fold_carrier[s] = carrier;
            self.touch_node(i, false);
        }
        // An edge changes only where an endpoint flipped and the other end is drawn
        // now or flipped too: an edge to a node that stays folded is empty before and
        // after. Most edges of a folding node run into folded code, and rewriting them
        // cost more than the fold itself (700 scattered edges, 1.3 ms, 2026-09-27). A
        // size change moves no edge vertex: edges run between centers.
        let mut edges = Vec::new();
        for &i in &flipped {
            for &e in self.scene.incident_edges(i) {
                let edge = self.scene.edges()[e as usize];
                let j = if edge.source == i {
                    edge.target
                } else {
                    edge.source
                };
                if self.is_drawn(j) || flipped.binary_search(&j).is_ok() {
                    edges.push(e);
                }
            }
        }
        self.dirty.edges.extend(edges);
        if changed > 0 {
            self.dirty.fold = true;
            if changed > n / 4 {
                // A load or a large unfold: one whole upload beats thousands of runs.
                self.dirty.full = true;
            }
        }
    }

    /// Starts slot `i` folding into (`hide`) or unfolding out of `carrier`.
    fn fold_tween(&mut self, i: u32, hide: bool, carrier: u32) {
        let s = i as usize;
        let own = self.scene.nodes()[s].position;
        let at = |c: u32| {
            self.shown_pos
                .get(c as usize)
                .copied()
                .filter(|_| self.scene.is_alive(c))
                .unwrap_or(own)
        };
        let tween = if hide {
            Tween {
                from: self.shown_pos[s],
                to: at(carrier),
                from_scale: self.shown_scale[s],
                to_scale: 0.0,
                elapsed: 0.0,
                then_remove: false,
                then_fold: true,
            }
        } else {
            // From its carrier, or from where it is drawn now if it was still folding
            // in (an unfold during a fold restarts from the drawn state).
            let folding = self.tweens.get(&i).is_some_and(|t| t.then_fold);
            let (from, from_scale) = if folding {
                (self.shown_pos[s], self.shown_scale[s])
            } else {
                (at(carrier), 0.0)
            };
            self.shown_pos[s] = from;
            self.shown_scale[s] = from_scale;
            Tween {
                from,
                to: own,
                from_scale,
                to_scale: 1.0,
                elapsed: 0.0,
                then_remove: false,
                then_fold: false,
            }
        };
        self.tweens.insert(i, tween);
    }

    /// The CPU-budget group sprite that stands for `slot`, if the render cap grouped it.
    pub fn group_of(&self, slot: u32) -> Option<u32> {
        self.visibility.group_of(slot)
    }

    /// Whether `slot` is shrinking into its parent (drawn, not pickable).
    fn is_folding(&self, slot: u32) -> bool {
        self.tweens.get(&slot).is_some_and(|t| t.then_fold)
    }

    /// Which nodes are drawn individually before selection overrides.
    pub fn visibility(&self) -> &Visibility {
        &self.visibility
    }

    /// Advances camera motion and node animation. `true` while either is moving.
    pub fn update(&mut self, dt: f32) -> bool {
        let camera = self.camera.update(dt);
        let animating = self.advance_tweens(dt);
        camera || animating
    }

    fn advance_tweens(&mut self, dt: f32) -> bool {
        if self.tweens.is_empty() {
            return false;
        }
        let dt = dt.max(0.0);
        let mut finished = Vec::new();
        let slots: Vec<u32> = self.tweens.keys().copied().collect();
        for slot in slots {
            let Some(tw) = self.tweens.get_mut(&slot) else {
                continue;
            };
            tw.elapsed += dt;
            let t = (tw.elapsed / TWEEN_SECONDS).clamp(0.0, 1.0);
            // Smoothstep: starts and ends at rest.
            let e = t * t * (3.0 - 2.0 * t);
            let lerp = |a: f32, b: f32| a + (b - a) * e;
            let tw = *tw;
            self.shown_pos[slot as usize] = [
                lerp(tw.from[0], tw.to[0]),
                lerp(tw.from[1], tw.to[1]),
                lerp(tw.from[2], tw.to[2]),
            ];
            self.shown_scale[slot as usize] = lerp(tw.from_scale, tw.to_scale);
            if t >= 1.0 {
                // Land exactly: `a + (b - a) * 1` can miss `b` by an ulp, and a node
                // at rest must be where the scene says it is.
                self.shown_pos[slot as usize] = tw.to;
                self.shown_scale[slot as usize] = tw.to_scale;
                finished.push((slot, tw.then_remove));
            }
            self.touch_moved(slot);
        }
        for (slot, then_remove) in finished {
            if self.tweens.remove(&slot).is_some_and(|t| t.then_fold) {
                // Folded in: hidden from now on, and drawn at home when it unfolds.
                let s = slot as usize;
                if self.scene.is_alive(slot) {
                    self.shown_pos[s] = self.scene.nodes()[s].position;
                }
                self.shown_scale[s] = 1.0;
                self.touch_node(slot, false);
                continue;
            }
            if then_remove && self.scene.is_alive(slot) {
                // Edges were removed when the removal began; this only frees the slot.
                if self.scene.remove_node(slot).is_ok() {
                    self.shown_scale[slot as usize] = 1.0;
                    self.touch_node(slot, false);
                    self.dirty.structure = true;
                }
            }
        }
        true
    }

    /// Whether a render into a `width` by `height` target would differ from the last.
    pub fn needs_render(&self, width: u32, height: u32) -> bool {
        self.dirty.any()
            || self.last_rendered != Some(self.camera.frame(width as f32, height as f32))
    }

    /// Frames rendered so far (for tests and the idle check).
    pub fn frames_rendered(&self) -> u64 {
        self.frames_rendered
    }

    /// How the last uploads went.
    pub fn upload_stats(&self) -> UploadStats {
        self.uploads
    }

    /// Recomputes what a structural change invalidates.
    fn settle(&mut self) {
        if std::mem::take(&mut self.dirty.fold) {
            let grouped_before = !self.visibility.groups().is_empty();
            self.visibility = visibility_for(&self.scene, self.budget, &self.folded);
            // Which sprites and edges are drawn changed: reordered lists go whole.
            if grouped_before || !self.slot_mode() {
                self.dirty.full = true;
            }
        }
        if !self.dirty.structure {
            return;
        }
        self.dirty.structure = false;
        let grouped_before = !self.visibility.groups().is_empty();
        self.visibility = visibility_for(&self.scene, self.budget, &self.folded);
        self.edge_density = edge_intensity_scale(code_edge_count(&self.scene));
        if grouped_before || !self.visibility.groups().is_empty() {
            self.dirty.full = true;
        }
        if let Some(sel) = &self.selection {
            let selected = sel.selected();
            self.selection = if self.is_vanishing(selected) {
                None
            } else {
                self.select_around(selected)
            };
            self.rebuild_instances();
            self.dirty.full = true;
        }
    }

    /// Slot order on the GPU: nothing selected, nothing grouped, and on the CPU budget
    /// nothing folded (there a compact list of what is drawn is uploaded instead).
    fn slot_mode(&self) -> bool {
        self.selection.is_none()
            && self.visibility.groups().is_empty()
            && !(self.budget == RenderBudget::Cpu && self.folded_count > 0)
    }

    fn upload_full(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), GalaxyError> {
        let sel = self.selection.as_ref();
        let (sprites, edges) = if self.slot_mode() {
            self.sprite_at.clear();
            self.edge_at.clear();
            let edges = build_edges_at(
                &self.scene,
                sel,
                self.budget,
                |i| self.is_drawn(i),
                |i| self.shown_pos[i as usize],
                self.selection_color,
            );
            (self.instances.clone(), edges)
        } else {
            let (sprites, sprite_at) = upload_list(
                &self.instances,
                |i| self.is_drawn(i) || self.is_folding(i),
                &self.visibility,
                sel,
            );
            // The selection's compact edge list, recording where each edge lands.
            let mut edges = Vec::new();
            let mut edge_at = vec![NOT_UPLOADED; self.scene.edges().len()];
            if let Some(s) = sel {
                // `edge_density` is current: `settle` recomputes it on any edge change.
                let density = self.edge_density;
                for e in 0..self.scene.edges().len() as u32 {
                    if let Some(pair) = selected_edge(
                        &self.scene,
                        s,
                        e,
                        density,
                        |i| self.is_drawn(i),
                        |i| self.shown_pos[i as usize],
                        self.selection_color,
                    ) {
                        edge_at[e as usize] = edges.len() as u32;
                        edges.extend(pair);
                    }
                }
            } else {
                edges = build_edges_at(
                    &self.scene,
                    None,
                    self.budget,
                    |i| self.is_drawn(i),
                    |i| self.shown_pos[i as usize],
                    self.selection_color,
                );
            }
            self.sprite_at = sprite_at;
            self.edge_at = edge_at;
            (sprites, edges)
        };
        self.renderer.set_instances(device, queue, &sprites)?;
        self.renderer.set_edges(device, queue, &edges)?;
        self.uploads.full += 1;
        self.uploads.last_bytes = (std::mem::size_of_val(sprites.as_slice())
            + std::mem::size_of_val(edges.as_slice())) as u64;
        Ok(())
    }

    /// Writes only the dirty slots at their places in the reordered lists of a
    /// selection or of a folded CPU-budget scene (moves and restyles; anything that
    /// changes which sprites or edges are drawn arrives as a structural or fold change
    /// and a full upload instead). Without a selection there are no edges (the CPU
    /// budget draws edges only for a selection).
    fn upload_partial_selected(&mut self, queue: &wgpu::Queue) -> Result<bool, GalaxyError> {
        let mut nodes = std::mem::take(&mut self.dirty.nodes);
        nodes.sort_unstable();
        nodes.dedup();
        let sprites: Vec<(u32, NodeInstance)> = nodes
            .iter()
            .filter_map(|&s| {
                let at = *self.sprite_at.get(s as usize)?;
                (at != NOT_UPLOADED).then(|| (at, self.instances[s as usize]))
            })
            .collect();
        let slices: Vec<(u32, &[NodeInstance])> = sprites
            .iter()
            .map(|(at, inst)| (*at, std::slice::from_ref(inst)))
            .collect();
        let mut bytes = (slices.len() * std::mem::size_of::<NodeInstance>()) as u64;
        let count = self.renderer.instance_count() as usize;
        if !self.renderer.patch_instances(queue, count, &slices)? {
            return Ok(false);
        }
        let mut edges = std::mem::take(&mut self.dirty.edges);
        let Some(sel) = self.selection.as_ref() else {
            self.uploads.partial += 1;
            self.uploads.last_bytes = bytes;
            return Ok(true);
        };
        edges.sort_unstable();
        edges.dedup();
        let density = self.edge_density;
        let pairs: Vec<(u32, [EdgeVertex; 2])> = edges
            .iter()
            .filter_map(|&e| {
                let at = *self.edge_at.get(e as usize)?;
                if at == NOT_UPLOADED {
                    return None;
                }
                let pair = selected_edge(
                    &self.scene,
                    sel,
                    e,
                    density,
                    |i| self.is_drawn(i),
                    |i| self.shown_pos[i as usize],
                    self.selection_color,
                )?;
                Some((at, pair))
            })
            .collect();
        let slices: Vec<(u32, &[EdgeVertex])> =
            pairs.iter().map(|(at, p)| (*at, p.as_slice())).collect();
        bytes += (slices.len() * 2 * std::mem::size_of::<EdgeVertex>()) as u64;
        let count = self.renderer.edge_vertex_count() as usize;
        if !self.renderer.patch_edges(queue, count, &slices)? {
            return Ok(false);
        }
        self.uploads.partial += 1;
        self.uploads.last_bytes = bytes;
        Ok(true)
    }

    /// Writes only the dirty slots. `Ok(false)` when a buffer must grow first.
    fn upload_partial(&mut self, queue: &wgpu::Queue) -> Result<bool, GalaxyError> {
        let mut bytes = 0u64;
        let node_runs = runs(std::mem::take(&mut self.dirty.nodes));
        let slices: Vec<(u32, &[NodeInstance])> = node_runs
            .iter()
            .map(|&(a, b)| (a, &self.instances[a as usize..=b as usize]))
            .collect();
        bytes += slices
            .iter()
            .map(|(_, s)| std::mem::size_of_val(*s) as u64)
            .sum::<u64>();
        if !self
            .renderer
            .patch_instances(queue, self.instances.len(), &slices)?
        {
            return Ok(false);
        }
        if self.budget.edges_without_selection() {
            let edge_runs = runs(std::mem::take(&mut self.dirty.edges));
            let vertices: Vec<(u32, Vec<EdgeVertex>)> = edge_runs
                .iter()
                .map(|&(a, b)| {
                    let v = (a..=b)
                        .flat_map(|e| {
                            unselected_edge(
                                &self.scene,
                                e,
                                |i| self.is_drawn(i),
                                |i| self.shown_pos[i as usize],
                            )
                        })
                        .collect();
                    (a * 2, v)
                })
                .collect();
            let slices: Vec<(u32, &[EdgeVertex])> =
                vertices.iter().map(|(a, v)| (*a, v.as_slice())).collect();
            bytes += slices
                .iter()
                .map(|(_, s)| std::mem::size_of_val(*s) as u64)
                .sum::<u64>();
            if !self
                .renderer
                .patch_edges(queue, self.scene.edges().len() * 2, &slices)?
            {
                return Ok(false);
            }
        }
        self.uploads.partial += 1;
        self.uploads.last_bytes = bytes;
        Ok(true)
    }

    /// Uploads changed buffers and encodes one frame into `target`. A scene beyond the
    /// device's buffer limits is an error, and nothing is drawn.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &RenderTarget,
    ) -> Result<(), GalaxyError> {
        self.render_timed(device, queue, encoder, target, None)
    }

    /// Brings the GPU buffers up to date without rendering (the benchmark times this).
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), GalaxyError> {
        self.settle();
        if self.dirty.any() {
            // In place without a pending full upload: in slot order, or at the
            // recorded places of a selection's lists (not with CPU grouping). A
            // buffer that must grow falls back to a full upload.
            let in_place = !self.dirty.full
                && if self.slot_mode() {
                    self.upload_partial(queue)?
                } else if self.visibility.groups().is_empty()
                    && (self.selection.is_some() || !self.budget.edges_without_selection())
                {
                    self.upload_partial_selected(queue)?
                } else {
                    false
                };
            if !in_place {
                self.upload_full(device, queue)?;
            }
        }
        self.dirty = Dirty::default();
        Ok(())
    }

    /// [`Self::render`] with GPU timestamps written at the start and end of the pass.
    pub fn render_timed(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &RenderTarget,
        timestamps: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> Result<(), GalaxyError> {
        self.upload(device, queue)?;
        let frame = self.camera.frame(target.width as f32, target.height as f32);
        self.renderer.render(
            queue,
            encoder,
            target,
            &frame,
            self.budget,
            self.edge_density,
            timestamps,
        );
        self.last_rendered = Some(frame);
        self.frames_rendered += 1;
        Ok(())
    }

    /// Sprites currently on the GPU (slots in slot order, or drawn nodes plus group
    /// sprites otherwise).
    pub fn drawn_sprites(&self) -> u32 {
        self.renderer.instance_count()
    }

    /// GPU bytes held for instances and edges.
    pub fn buffer_capacity(&self) -> (u64, u64) {
        self.renderer.buffer_capacity()
    }

    /// Edge vertices currently on the GPU (two per edge slot, or per drawn edge).
    pub fn drawn_edge_vertices(&self) -> u32 {
        self.renderer.edge_vertex_count()
    }

    /// The world radius `slot` is drawn at now (selection scale, animation and a
    /// proxy's fold growth included); `None` when nothing is drawn there.
    pub fn drawn_size(&self, slot: u32) -> Option<f32> {
        self.instances
            .get(slot as usize)
            .filter(|i| !i.is_hidden())
            .map(|i| i.pos_size[3])
    }

    /// Where `slot` is drawn now (its animated position).
    pub fn shown_position(&self, slot: u32) -> Option<[f32; 3]> {
        self.shown_pos.get(slot as usize).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::runs;

    #[test]
    fn dirty_slots_coalesce_into_runs() {
        assert_eq!(runs(vec![]), vec![]);
        assert_eq!(runs(vec![5, 3, 4, 5]), vec![(3, 5)]);
        assert_eq!(runs(vec![0, 10, 100]), vec![(0, 10), (100, 100)]);
    }
}
