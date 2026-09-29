//! Labels on the galaxy: names on the largest stars, and on every work node and
//! agent its status, what blocks it, the user's order, pending approvals and its
//! intent ("see at a glance").
//!
//! Ported from DeusData/codebase-memory-mcp `graph-ui/src/components/NodeLabels.tsx`
//! at 64c23fab6c3eba3de226355c0ca1ff750116edab (MIT,
//! `third_party/codebase-memory-mcp/UPSTREAM`):
//! - at most [`MAX_LABELS`] (upstream `maxLabels = 80`), chosen by descending node
//!   size; with a highlight, only highlighted nodes are labeled;
//! - text over the maximum width is cut with a binary-search ellipsis fit
//!   (upstream `fitText`, `...` appended);
//! - a dark outline under the fill (upstream strokes at 0.9 alpha black).
//!
//! Changes: upstream draws billboarded canvas textures in the 3D scene; here labels
//! are egui text painted over the rendered frame at each node's projected position,
//! so they cost no texture uploads and no scene state. Work nodes and agents
//! (bounded by plan size, 0014) are labeled before code and are never crowded out by
//! it. The CPU budget halves the cap ([`RenderBudget::label_cap`]).

use egui::{Color32, FontId, Pos2, Rect};
use glam::Vec3;

use crate::budget::RenderBudget;
use crate::galaxy::Galaxy;

/// Most labels drawn (upstream `maxLabels`).
pub const MAX_LABELS: usize = 80;
/// Label size: 12 px (style guide STG-DRAW-9, STG-DRAW-10).
pub const LABEL_FONT_SIZE: f32 = 12.0;
/// The zoom over the fit scale from which a group shows its full name (STG-DRAW-10).
pub const FULL_NAME_ZOOM: f32 = 2.0;
/// Widest code label, in points (upstream fits 720 px at a 64 px font: about 11
/// ems; at 11 pt that is about 124 pt; widened for qualified names).
pub const CODE_LABEL_WIDTH: f32 = 180.0;
/// Widest work or agent label, in points: these carry status and intent.
pub const OVERLAY_LABEL_WIDTH: f32 = 340.0;

impl RenderBudget {
    /// Most labels drawn on this budget.
    pub fn label_cap(self) -> usize {
        match self {
            Self::Gpu => MAX_LABELS,
            Self::Cpu => MAX_LABELS / 2,
        }
    }
}

/// A label to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub slot: u32,
    pub text: String,
    pub color: Color32,
    /// Work nodes and agents: wider, and labeled before code.
    pub overlay: bool,
    /// A group (a proxy): the label sits under it (STG-DRAW-10), not above.
    pub group: bool,
}

/// Which nodes get a label: drawn and not vanishing (a node folded into its parent,
/// decision 0068, has none); with a selection, only highlighted ones. Overlay nodes
/// first (by descending size), then code nodes in `code_by_size` order (largest
/// first), at most `cap` in all.
pub fn choose(
    galaxy: &Galaxy,
    overlay: &[u32],
    code_by_size: impl IntoIterator<Item = u32>,
    cap: usize,
) -> (Vec<u32>, Vec<u32>) {
    let scene = galaxy.scene();
    let sel = galaxy.selection();
    let shown = |slot: u32| {
        scene.is_alive(slot)
            && galaxy.is_drawn(slot)
            && !galaxy.is_vanishing(slot)
            && sel.is_none_or(|s| s.is_highlighted(slot))
    };
    let mut over: Vec<u32> = overlay.iter().copied().filter(|&s| shown(s)).collect();
    let size = |s: u32| scene.nodes()[s as usize].size;
    over.sort_by(|&a, &b| size(b).total_cmp(&size(a)).then(a.cmp(&b)));
    over.truncate(cap);
    let room = cap - over.len();
    let code = code_by_size
        .into_iter()
        .filter(|&s| shown(s))
        .take(room)
        .collect();
    (over, code)
}

/// `text` cut to `max_width` with `...` (upstream `fitText`): the longest prefix
/// that fits, found by binary search over char counts.
pub fn fit(text: &str, max_width: f32, width_of: impl Fn(&str) -> f32) -> String {
    if width_of(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "...";
        if width_of(&candidate) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo.max(1)].iter().collect::<String>() + "..."
}

/// Paints the chosen labels over the galaxy drawn in `rect`.
pub fn draw(ui: &egui::Ui, rect: Rect, galaxy: &Galaxy, chosen: &[Label], halo: Color32) {
    let ppp = ui.ctx().pixels_per_point();
    let frame = galaxy
        .camera()
        .frame(rect.width() * ppp, rect.height() * ppp);
    let painter = ui.painter_at(rect);
    let font = FontId::proportional(LABEL_FONT_SIZE);
    let outline = halo;
    for c in chosen {
        let Some(world) = galaxy.shown_position(c.slot) else {
            continue;
        };
        let Some(([x, y], _depth)) = frame.project(Vec3::from(world)) else {
            continue;
        };
        // Above the node, or under it for a group (STG-DRAW-10).
        let dy = if c.group { 10.0 } else { -10.0 };
        let at = Pos2::new(rect.min.x + x / ppp, rect.min.y + y / ppp + dy);
        let anchor = if c.group {
            egui::Align2::CENTER_TOP
        } else {
            egui::Align2::CENTER_BOTTOM
        };
        if !rect.expand(40.0).contains(at) {
            continue;
        }
        let max = if c.overlay {
            OVERLAY_LABEL_WIDTH
        } else {
            CODE_LABEL_WIDTH
        };
        let text = fit(&c.text, max, |t| {
            ui.ctx().fonts_mut(|f| {
                f.layout_no_wrap(t.to_owned(), font.clone(), c.color)
                    .size()
                    .x
            })
        });
        // The outline: the text in black one point off in four directions.
        for d in [[-1.0, 0.0], [1.0, 0.0], [0.0, -1.0], [0.0, 1.0]] {
            painter.text(
                at + egui::vec2(d[0], d[1]),
                anchor,
                &text,
                font.clone(),
                outline,
            );
        }
        painter.text(at, anchor, &text, font.clone(), c.color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_what_fits_and_cuts_the_rest_with_an_ellipsis() {
        // One unit per char.
        let w = |t: &str| t.chars().count() as f32;
        assert_eq!(fit("short", 10.0, w), "short");
        assert_eq!(fit("a_long_qualified_name", 10.0, w), "a_long_...");
        assert_eq!(w(&fit("a_long_qualified_name", 10.0, w)), 10.0);
        assert_eq!(fit("abcdef", 2.0, w), "a...", "never empty");
    }
}
