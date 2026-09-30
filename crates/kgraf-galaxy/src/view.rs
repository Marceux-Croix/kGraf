//! The egui widget: input to camera and selection, offscreen render, texture display.
//!
//! Controls: left drag orbits, right or middle drag (or shift + left drag) pans, wheel
//! or pinch zooms, click selects (empty space clears), Escape clears. The click is also
//! kept for the view's owner ([`GalaxyView::take_click`]): a project view expands a
//! folded node on click and folds the selected node on a second click (decision 0068).

use std::sync::Arc;

use egui::mutex::RwLock;
use egui::{Color32, Rect, Sense, pos2};

use crate::budget::RenderBudget;
use crate::galaxy::Galaxy;
use crate::gpu::RenderTarget;
use crate::palette::Palette;
use crate::scene::Scene;

/// Longest step the camera takes in one frame. After an idle gap egui's dt can be
/// seconds; clamping stops a queued motion from jumping to its end in one frame.
const MAX_DT: f32 = 0.1;

/// A click on the galaxy: what was hit and whether that node was already the selected
/// one (the second click of the fold rule), or which hot zone it landed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Click {
    pub hit: Option<u32>,
    pub was_selected: bool,
    /// The index of the hot zone ([`GalaxyView::set_hot_zones`]) the click landed in.
    /// Such a click neither picks nor changes the selection.
    pub zone: Option<usize>,
}

/// What a click at `pos` does, given the node `hit` under it and the `selected` node:
/// the last zone containing `pos` wins when `hit` is nothing or the zone's own node;
/// otherwise the click is on `hit`.
pub fn click_at(
    zones: &[(Rect, u32)],
    pos: egui::Pos2,
    hit: Option<u32>,
    selected: Option<u32>,
) -> Click {
    let zone = zones
        .iter()
        .rposition(|(r, owner)| r.contains(pos) && hit.is_none_or(|h| h == *owner));
    match zone {
        Some(z) => Click {
            zone: Some(z),
            ..Click::default()
        },
        None => Click {
            hit,
            was_selected: hit.is_some() && selected == hit,
            zone: None,
        },
    }
}

/// A galaxy drawn into an egui panel.
pub struct GalaxyView {
    galaxy: Galaxy,
    target: Option<RenderTarget>,
    texture: Option<egui::TextureId>,
    /// egui's renderer, held so the registered texture is freed on drop.
    egui_renderer: Arc<RwLock<egui_wgpu::Renderer>>,
    /// A render failure (a scene beyond the device limits), shown in the panel until
    /// the scene is replaced; retrying the same scene would fail the same way.
    error: Option<String>,
    /// Where the galaxy was last drawn, for painting labels over it.
    last_rect: Option<Rect>,
    /// The last click, until the owner takes it.
    click: Option<Click>,
    /// Rectangles (in points) drawn over the galaxy that take clicks themselves (the
    /// fold handles and pills), each with the node it belongs to.
    hot_zones: Vec<(Rect, u32)>,
    /// The node under the pointer, when the pointer is over the galaxy.
    hovered: Option<u32>,
    /// The pointer position the hover was picked at (a pick runs only when it moves).
    hover_at: Option<egui::Pos2>,
    /// Whether the scene counts are drawn (the stage HUD).
    hud: bool,
    /// HUD and error colors (from the palette).
    hud_color: Color32,
    error_color: Color32,
}

impl Drop for GalaxyView {
    /// Frees the native texture registered with egui; without this every view created
    /// and dropped would leave its last frame allocated in egui's texture table.
    fn drop(&mut self) {
        if let Some(id) = self.texture.take() {
            self.egui_renderer.write().free_texture(&id);
        }
    }
}

impl std::fmt::Debug for GalaxyView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GalaxyView")
            .field("galaxy", &self.galaxy)
            .field("texture", &self.texture)
            .finish_non_exhaustive()
    }
}

impl GalaxyView {
    pub fn new(render_state: &egui_wgpu::RenderState, scene: Scene, budget: RenderBudget) -> Self {
        Self {
            galaxy: Galaxy::new(&render_state.device, scene, budget),
            target: None,
            texture: None,
            egui_renderer: Arc::clone(&render_state.renderer),
            error: None,
            last_rect: None,
            click: None,
            hot_zones: Vec::new(),
            hovered: None,
            hover_at: None,
            hud: true,
            hud_color: Palette::default().hud,
            error_color: Palette::default().error,
        }
    }

    /// Shows or hides the scene counts drawn at the bottom left.
    pub fn set_hud(&mut self, on: bool) {
        self.hud = on;
    }

    /// Takes the HUD and error colors, and the selection's color for the scene, from
    /// `palette`.
    pub fn set_palette(&mut self, palette: &Palette) {
        self.hud_color = palette.hud;
        self.error_color = palette.error;
        self.galaxy
            .set_selection_colors(palette.selection, palette.neighbor);
        self.galaxy.set_stage(palette.background, palette.card);
    }

    /// Sets the click targets drawn over the galaxy (in points), each with the node it
    /// belongs to. A click picks first: a zone takes it only when the pick hits nothing
    /// or hits the zone's own node, so a zone never covers another node's click (336 node centers were
    /// unclickable under "+N" zones once). A zone's click is
    /// reported with its index and does not change the selection.
    pub fn set_hot_zones(&mut self, zones: Vec<(Rect, u32)>) {
        self.hot_zones = zones;
    }

    /// The node under the pointer, if the pointer is over the galaxy.
    pub fn hovered(&self) -> Option<u32> {
        self.hovered
    }

    /// The click of the last frame, once.
    pub fn take_click(&mut self) -> Option<Click> {
        self.click.take()
    }

    pub fn galaxy(&self) -> &Galaxy {
        &self.galaxy
    }

    pub fn galaxy_mut(&mut self) -> &mut Galaxy {
        &mut self.galaxy
    }

    /// The texture id registered with egui, once the view has been shown.
    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture
    }

    /// Replaces the scene and clears a render failure the old scene caused.
    pub fn set_scene(&mut self, scene: Scene) {
        self.galaxy.set_scene(scene);
        self.error = None;
    }

    /// Where the galaxy was last drawn, in points.
    pub fn last_rect(&self) -> Option<Rect> {
        self.last_rect
    }

    /// Why the last render failed, if it did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The offscreen target the last frame was drawn into, once the view has been shown.
    pub fn target(&self) -> Option<&RenderTarget> {
        self.target.as_ref()
    }

    /// Fills the available space. Requests another frame only while the camera moves
    /// or a drag is in progress; an idle galaxy costs no frames.
    pub fn show(&mut self, ui: &mut egui::Ui, render_state: &egui_wgpu::RenderState) {
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        self.last_rect = Some(rect);
        let ppp = ui.ctx().pixels_per_point();
        let width = ((rect.width() * ppp).round() as u32).max(1);
        let height = ((rect.height() * ppp).round() as u32).max(1);
        let h = height as f32;

        let shift = ui.input(|i| i.modifiers.shift);
        let delta = resp.drag_delta() * ppp;
        let camera = self.galaxy.camera_mut();
        if resp.dragged_by(egui::PointerButton::Primary) && !shift {
            camera.rotate(delta.x, delta.y, h);
        } else if resp.dragged_by(egui::PointerButton::Secondary)
            || resp.dragged_by(egui::PointerButton::Middle)
            || (resp.dragged_by(egui::PointerButton::Primary) && shift)
        {
            camera.pan(delta.x, delta.y, h);
        }
        if resp.hovered() {
            let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            if scroll != 0.0 {
                camera.zoom_scroll(scroll);
            }
            if zoom != 1.0 {
                camera.zoom_factor(1.0 / zoom);
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.galaxy.select(None);
            }
        }
        // The hovered node: picked again only when the pointer moves.
        match resp.hover_pos() {
            Some(pos) if self.hover_at != Some(pos) => {
                let cursor = [(pos.x - rect.min.x) * ppp, (pos.y - rect.min.y) * ppp];
                self.hovered = self.galaxy.pick(cursor, width, height);
                self.hover_at = Some(pos);
            }
            Some(_) => {}
            None => {
                self.hovered = None;
                self.hover_at = None;
            }
        }
        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let cursor = [(pos.x - rect.min.x) * ppp, (pos.y - rect.min.y) * ppp];
            let hit = self.galaxy.pick(cursor, width, height);
            self.click = Some(click_at(
                &self.hot_zones,
                pos,
                hit,
                self.galaxy.selection().map(|s| s.selected()),
            ));
            if self.click.is_some_and(|c| c.zone.is_none()) {
                self.galaxy.select(hit);
            }
        }

        let dt = ui.input(|i| i.stable_dt).min(MAX_DT);
        let moving = self.galaxy.update(dt);

        // A target failure (a panel larger than the device allows) is re-checked every
        // frame, so shrinking the window recovers.
        let target_error = self.ensure_target(render_state, width, height).err();
        if target_error.is_none()
            && self.error.is_none()
            && self.galaxy.needs_render(width, height)
            && let Some(target) = &self.target
        {
            let device = &render_state.device;
            let queue = &render_state.queue;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("galaxy frame"),
            });
            match self.galaxy.render(device, queue, &mut encoder, target) {
                Ok(()) => {
                    queue.submit([encoder.finish()]);
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        if let Some(err) = target_error
            .map(|e| e.to_string())
            .or_else(|| self.error.clone())
        {
            // Fail loud: the panel says why there is no galaxy.
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("galaxy unavailable: {err}"),
                egui::FontId::proportional(14.0),
                self.error_color,
            );
            return;
        }
        if let Some(tex) = self.texture {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            ui.painter().image(tex, rect, uv, Color32::WHITE);
        }
        if self.hud {
            self.overlay(ui, rect);
        }
        if moving || resp.dragged() {
            ui.ctx().request_repaint();
        }
    }

    fn ensure_target(
        &mut self,
        render_state: &egui_wgpu::RenderState,
        width: u32,
        height: u32,
    ) -> Result<(), crate::GalaxyError> {
        let same = self
            .target
            .as_ref()
            .is_some_and(|t| t.width == width && t.height == height);
        if same {
            return Ok(());
        }
        let target = RenderTarget::new(&render_state.device, width, height)?;
        let mut renderer = render_state.renderer.write();
        match self.texture {
            Some(id) => renderer.update_egui_texture_from_wgpu_texture(
                &render_state.device,
                &target.view,
                wgpu::FilterMode::Linear,
                id,
            ),
            None => {
                self.texture = Some(renderer.register_native_texture(
                    &render_state.device,
                    &target.view,
                    wgpu::FilterMode::Linear,
                ));
            }
        }
        self.target = Some(target);
        Ok(())
    }

    fn overlay(&self, ui: &egui::Ui, rect: Rect) {
        let g = &self.galaxy;
        let mut text = format!(
            "{} nodes, {} edges",
            g.scene().live_node_count(),
            g.scene().live_edge_count()
        );
        if g.folded_count() > 0 {
            text.push_str(&format!(" | {} folded", g.folded_count()));
        }
        if g.budget() == RenderBudget::Cpu {
            let vis = g.visibility();
            text.push_str(&format!(
                " | CPU budget: {} nodes drawn, {} groups, edges on selection only",
                vis.shown_count(),
                vis.groups().len()
            ));
        }
        if let Some(sel) = g.selection() {
            text.push_str(&format!(
                " | selected node {} (+{} neighbors)",
                sel.selected(),
                sel.members().len() - 1
            ));
        }
        ui.painter().text(
            rect.left_bottom() + egui::vec2(8.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            text,
            egui::FontId::monospace(11.0),
            self.hud_color,
        );
    }
}
