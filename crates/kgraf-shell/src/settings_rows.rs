//! The settings dialog's building blocks. Every tab is drawn from these, so
//! each section, row, switch and note looks and reads the same way:
//!
//! - a section heading, 500 16/22 (type scale 4.2, TYP-1: weight 500, never
//!   700, below the dialog title);
//! - a setting row: the label in 500 14/20, then its description in 400 13/18
//!   `--text-2`, 4 px below (CMP-SET-1);
//! - a 1 px `--outline` line between rows, with 16 px of padding (CMP-SET-1);
//! - a note in an `--additive` card for a fixed behavior (CMP-SET-6);
//! - helper and error text in 400 12/18, error text `--danger` (CMP-FLD-4).
//!
//! Colors come only from the palette (THM-4).

use egui::{RichText, Ui};

use crate::theme::{self, Palette};

/// Space above and below a row's divider: CMP-SET-1's 16 px row padding,
/// less egui's own item spacing.
const ROW_PAD: f32 = 12.0;

/// A section heading inside a tab (500 16/22).
pub fn heading(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).font(theme::medium(16.0)).color(p.text));
    ui.add_space(2.0);
}

/// A setting row's label (CMP-SET-1: 500 14/20).
pub fn label(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(theme::medium(14.0)).color(p.text));
}

/// A setting row's description (CMP-SET-1: 400 13/18 `--text-2`).
pub fn description(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).size(13.0).color(p.text_2));
}

/// Helper text under a control (CMP-FLD-4: 400 12/18 `--text-2`).
pub fn helper(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(p.text_2));
}

/// Error text under a control (CMP-FLD-4: 400 12/18 `--danger`).
pub fn error(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(p.danger));
}

/// The 1 px `--outline` line between two rows (CMP-SET-1).
pub fn divider(ui: &mut Ui, p: &Palette) {
    ui.add_space(ROW_PAD);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, p.outline),
    );
    ui.add_space(ROW_PAD);
}

/// An on or off setting as one row: the switch carries the label, so the
/// label is also the control's accessible name, and the description sits
/// below it (CMP-SET-1). CMP-SET-4 names a switch; egui's checkbox is the
/// toggle this shell uses for every on or off setting. Returns whether it
/// changed this frame.
pub fn toggle(ui: &mut Ui, p: &Palette, label: &str, description: &str, on: &mut bool) -> bool {
    let changed = ui
        .checkbox(
            on,
            RichText::new(label).font(theme::medium(14.0)).color(p.text),
        )
        .changed();
    if !description.is_empty() {
        indented(ui, |ui| self::description(ui, p, description));
    }
    changed
}

/// A note stating a fixed behavior (CMP-SET-6: `--additive` fill, radius 12,
/// padding 12 16, 400 13/18 `--text-2`).
pub fn note(ui: &mut Ui, p: &Palette, text: &str) {
    egui::Frame::new()
        .fill(p.additive)
        .corner_radius(theme::radius::CARD)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            description(ui, p, text);
        });
}

/// `add`, indented to the text of a checkbox above it, so a switch's
/// description lines up with its label.
pub fn indented<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let indent = ui.spacing().icon_width + ui.spacing().icon_spacing;
    ui.horizontal_top(|ui| {
        ui.add_space(indent);
        ui.vertical(add).inner
    })
    .inner
}
