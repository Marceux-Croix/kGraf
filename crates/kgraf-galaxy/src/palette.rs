//! Every color the galaxy draws, in one replaceable struct.
//!
//! A [`Palette`] is built from one source: [`Palette::from_semantic`] maps the run-time
//! semantic colors (`kgraf_core::palette::Semantic`, which carries the color-vision
//! mode) and the stage literals (`kgraf_core::palette::STAGE`, style guide 1.8 section
//! 3.4) that the shell's stage chrome reads too. The shell hands the result to
//! [`crate::ProjectView::set_palette`]; the default is the same mapping for the standard
//! vision mode, so nothing here restates a guide color. Violet is progress only, orange
//! (`--stage-danger`) marks a failed or broken item itself, yellow is needs input (and,
//! in any count, a failure), cyan is the selection or a match, gray everything else.
//!
//! The scene draws opaque sprites, so a translucent guide color is stored as its
//! result over the stage background: "node `rgba(160,166,186,.58)`" is `#606472` here.

use egui::Color32;
use kgraf_core::palette::{Rgb, Rgba, STAGE, Semantic, StageLiterals, Surface, Vision};

/// sRGB bytes as 0..=1.
fn unit(c: Rgb) -> [f32; 3] {
    c.map(|v| f32::from(v) / 255.0)
}

/// `color` at `alpha` over `under`: the opaque result, 0..=1.
fn over(color: Rgb, alpha: f32, under: Rgb) -> [f32; 3] {
    let (c, u) = (unit(color), unit(under));
    [0, 1, 2].map(|i| c[i] * alpha + u[i] * (1.0 - alpha))
}

/// A guide `rgba(...)` over `under`.
fn over_rgba(c: Rgba, under: Rgb) -> [f32; 3] {
    over(c.rgb, c.alpha, under)
}

/// sRGB bytes as an egui color.
fn c32(c: Rgb) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// White at `alpha` (0..=1) as an egui color.
fn white(alpha: f32) -> Color32 {
    Color32::from_white_alpha((alpha * 255.0).round() as u8)
}

/// A presumed card's border (decision 0064): white at this alpha over the card, dimmer
/// than a real card's. Not a guide value: the guide has no presumed state.
const PRESUMED_BORDER_WHITE: f32 = 0.16;
/// A switched-off schedule's border: dimmest. Not a guide value.
const OFF_BORDER_WHITE: f32 = 0.08;
/// The fold handle "+N" (STG-DRAW-5): white at 70%.
const FOLD_MARK_WHITE: f32 = 0.70;

/// The galaxy's colors: scene colors in sRGB 0..=1, overlay colors for egui.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// The stage behind everything (STG-1).
    pub background: [f32; 3],
    /// Code dots (STG-DRAW-2): the neutral node color; only their size follows degree.
    pub code_dot: [f32; 3],
    /// Every edge between items, and an actor's relation lines: faint and neutral. The
    /// scene multiplies it by each edge's intensity (0.25 within a cluster), so this
    /// comes out near the guide's "white at 5%".
    pub edge: [f32; 3],
    /// A waits-on arrow between work items (STG-DRAW-6).
    pub relation_arrow: [f32; 3],
    /// The selection's direct relations and search matches (STG-DRAW-2, STG-DRAW-4).
    pub selection: [f32; 3],
    /// A highlighted neighbor of the selected node (section 3.4: `rgba(225,229,240,.9)`).
    pub neighbor: [f32; 3],
    /// Directory and group proxies.
    pub proxy: [f32; 3],
    /// An actor's disk (STG-DRAW-7).
    pub agent: [f32; 3],
    /// A work card's fill (STG-DRAW-5).
    pub card: [f32; 3],
    /// A card's border in a neutral state: white at 34% over the card (STG-DRAW-5).
    pub state_neutral: [f32; 3],
    /// A card's border when held for the user (COL-3).
    pub state_needs_input: [f32; 3],
    /// A card's border when failed or broken (COL-7, STG-DRAW-11).
    pub state_danger: [f32; 3],
    /// A presumed card's border (decision 0064): dimmer than real work.
    pub state_presumed: [f32; 3],
    /// A switched-off schedule's border: dimmest.
    pub state_off: [f32; 3],
    /// Labels on the stage: white at 88% (STG-1, section 3.4).
    pub text: Color32,
    /// Group labels (proxies): white at 55% (section 3.4, STG-DRAW-10).
    pub group_label: Color32,
    /// The dark halo under stage text (TYP-4): black at 85%.
    pub text_halo: Color32,
    /// The stage HUD line.
    pub hud: Color32,
    /// An error said on the stage.
    pub error: Color32,
    /// A waiting or missing state said on the stage (an unindexed project).
    pub notice: Color32,
    /// Count pills (STG-DRAW-8): yellow, failures included.
    pub pill: Color32,
    /// Text and icons on a pill: dark.
    pub pill_text: Color32,
    /// The failed item's own ring and small pill (STG-DRAW-11).
    pub danger: Color32,
    /// The fold handle "+N" (STG-DRAW-5): white at 70%.
    pub fold_mark: Color32,
    /// The ring around the selected node and around matches (STG-DRAW-4).
    pub selection_ring: Color32,
}

impl Palette {
    /// The stage palette from the semantic colors and the stage literals. The shell
    /// calls this with `Semantic::for_surface(Surface::Stage, vision)` and [`STAGE`].
    pub fn from_semantic(sem: &Semantic, stage: &StageLiterals) -> Self {
        let bg = stage.background;
        let card = stage.card;
        Self {
            background: unit(bg),
            code_dot: over_rgba(stage.node, bg),
            // The scene multiplies an edge by its intensity (same cluster 0.25) and the
            // renderer adds edges up, so this draws as the guide's "white at 5%".
            edge: [stage.edge_white / kgraf_graph::layout::density::SAME_CLUSTER_EDGE; 3],
            relation_arrow: unit(stage.relation),
            selection: unit(sem.selection),
            neighbor: over_rgba(stage.neighbor, bg),
            proxy: unit(sem.neutral),
            agent: unit(stage.actor),
            card: unit(card),
            state_neutral: over([0xFF; 3], stage.card_border_white, card),
            state_needs_input: unit(sem.needs_input),
            state_danger: unit(sem.danger),
            state_presumed: over([0xFF; 3], PRESUMED_BORDER_WHITE, card),
            state_off: over([0xFF; 3], OFF_BORDER_WHITE, card),
            text: white(stage.label_white),
            group_label: white(stage.group_label_white),
            text_halo: Color32::from_black_alpha((stage.halo_black * 255.0).round() as u8),
            hud: c32(sem.neutral),
            error: c32(sem.danger),
            notice: c32(sem.needs_input),
            pill: c32(sem.needs_input),
            pill_text: c32(stage.pill_icon),
            danger: c32(sem.danger),
            fold_mark: white(FOLD_MARK_WHITE),
            selection_ring: c32(sem.selection),
        }
    }
}

impl Default for Palette {
    /// The standard vision mode's stage palette.
    fn default() -> Self {
        Self::from_semantic(
            &Semantic::for_surface(Surface::Stage, Vision::Standard),
            &STAGE,
        )
    }
}

/// sRGB 0..=1 to an egui color.
pub fn to_color32(c: [f32; 3]) -> Color32 {
    Color32::from_rgb(
        (c[0] * 255.0).round() as u8,
        (c[1] * 255.0).round() as u8,
        (c[2] * 255.0).round() as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each meaning of a run-time `Semantic` (a color-vision mode, say) reaches every
    /// role that carries it, and nothing else changes.
    #[test]
    fn a_semantic_palette_reaches_every_role_of_its_meaning() {
        let base = Semantic::for_surface(Surface::Stage, Vision::Standard);
        let marked = Semantic {
            selection: [1, 2, 3],
            needs_input: [4, 5, 6],
            danger: [7, 8, 9],
            neutral: [10, 11, 12],
            ..base
        };
        let p = Palette::from_semantic(&marked, &STAGE);
        let d = Palette::from_semantic(&base, &STAGE);
        let rgb = |c: Rgb| unit(c);
        assert_eq!(
            (p.selection, p.selection_ring),
            (rgb([1, 2, 3]), c32([1, 2, 3]))
        );
        assert_eq!(p.state_needs_input, rgb([4, 5, 6]));
        assert_eq!((p.pill, p.notice), (c32([4, 5, 6]), c32([4, 5, 6])));
        assert_eq!(p.state_danger, rgb([7, 8, 9]));
        assert_eq!((p.danger, p.error), (c32([7, 8, 9]), c32([7, 8, 9])));
        assert_eq!((p.proxy, p.hud), (rgb([10, 11, 12]), c32([10, 11, 12])));
        // The literals do not move with the meanings.
        assert_eq!(
            (
                p.background,
                p.code_dot,
                p.card,
                p.agent,
                p.edge,
                p.neighbor
            ),
            (
                d.background,
                d.code_dot,
                d.card,
                d.agent,
                d.edge,
                d.neighbor
            )
        );
        assert_eq!(
            (p.relation_arrow, p.text, p.group_label, p.pill_text),
            (d.relation_arrow, d.text, d.group_label, d.pill_text)
        );
    }

    #[test]
    fn the_defaults_are_the_nocturne_stage_palette() {
        let p = Palette::default();
        let hex = |c: [f32; 3]| to_color32(c);
        assert_eq!(hex(p.background), Color32::from_rgb(0x07, 0x08, 0x0F));
        assert_eq!(
            p.background,
            unit(crate::gpu::BACKGROUND_SRGB),
            "the renderer's default background is the palette's"
        );
        // rgba(160,166,186,.58) over #07080F.
        assert_eq!(hex(p.code_dot), Color32::from_rgb(0x60, 0x64, 0x72));
        assert_eq!(hex(p.selection), Color32::from_rgb(0x22, 0xD3, 0xEE));
        assert_eq!(p.selection_ring, hex(p.selection));
        assert_eq!(hex(p.state_needs_input), p.pill, "failures count in yellow");
        assert_eq!(p.pill, Color32::from_rgb(0xFA, 0xCC, 0x15));
        assert_eq!(hex(p.state_danger), p.danger);
        assert_eq!(p.danger, Color32::from_rgb(0xFF, 0x7A, 0x33));
        assert_eq!(hex(p.agent), Color32::from_rgb(0x1E, 0x22, 0x33));
        assert_eq!(hex(p.card), Color32::from_rgb(0x17, 0x1A, 0x26));
        assert_eq!(hex(p.relation_arrow), Color32::from_rgb(0xC3, 0xC8, 0xD8));
        assert_eq!(p.pill_text, Color32::from_rgb(0x0D, 0x0F, 0x17));
        // rgba(225,229,240,.9) over #07080F.
        assert_eq!(hex(p.neighbor), Color32::from_rgb(0xCB, 0xCF, 0xDA));
        assert_eq!(
            p.group_label,
            Color32::from_white_alpha(140),
            "white at 55%"
        );
    }
}
