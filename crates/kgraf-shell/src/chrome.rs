//! Reusable pieces of the shell chrome in the Stage style (`docs/STYLE_GUIDE.md`
//! section 9), drawn from [`crate::theme`]'s tokens: buttons, chips, avatars, dots
//! and the kGraf mark.
//!
//! The components are drawn with egui's painter from the guide's values; no
//! component code is vendored (decision 0055: UI code lives in this tree so
//! it can be reshaped).

use egui::{Color32, Pos2, Rect, Response, RichText, Sense, Stroke, Ui, Vec2, pos2, vec2};

use crate::theme::{self, Palette};

/// A neutral avatar: a circle in the opaque outline color with one letter, and an
/// optional state dot at its lower right. The letter names the provider or the
/// worker; color never does (the four-color rule).
pub fn avatar(
    painter: &egui::Painter,
    rect: Rect,
    letter: char,
    p: &Palette,
    dot: Option<Color32>,
) {
    let r = rect.width().min(rect.height()) * 0.5;
    let c = rect.center();
    painter.circle_filled(c, r, p.outline_opaque);
    painter.text(
        c,
        egui::Align2::CENTER_CENTER,
        letter.to_uppercase().collect::<String>(),
        theme::medium((r * 0.95).max(8.0)),
        p.text,
    );
    if let Some(color) = dot {
        let d = (r * 0.68).max(4.0);
        let at = c + vec2(r * 0.72, r * 0.72);
        painter.circle_filled(at, d * 0.5 + 1.5, p.base);
        painter.circle_filled(at, d * 0.5, color);
    }
}

/// The kGraf mark (AST-2): a "k" drawn as graph edges with one accent node,
/// on a 28 by 28 unit grid scaled into `rect`. An original mark.
pub fn kgraf_mark(painter: &egui::Painter, rect: Rect, color: Color32) {
    let s = rect.width().min(rect.height()) / 28.0;
    let at = |x: f32, y: f32| rect.min + vec2(x * s, y * s);
    let stroke = Stroke::new(2.2 * s, color);
    painter.line_segment([at(8.0, 5.0), at(8.0, 23.0)], stroke);
    painter.line_segment([at(8.0, 15.0), at(17.0, 7.0)], stroke);
    painter.line_segment([at(8.0, 15.0), at(18.0, 23.0)], stroke);
    for (x, y) in [(8.0, 5.0), (8.0, 23.0), (18.0, 6.2), (19.0, 23.0)] {
        painter.circle_filled(at(x, y), 2.6 * s, color);
    }
    painter.circle_filled(at(8.0, 15.0), 3.4 * s, theme::ACCENT);
}

/// A 40 point round icon button (CMP-IBTN-1), with a tooltip. `color`
/// overrides the icon color; `angle` rotates the glyph (the hamburger's wiggle).
pub fn icon_button_ex(
    ui: &mut Ui,
    p: &Palette,
    icon: char,
    tooltip: &str,
    color: Option<Color32>,
    angle: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::click());
    let resp = resp.on_hover_text(tooltip);
    if resp.is_pointer_button_down_on() {
        ui.painter()
            .circle_filled(rect.center(), 20.0, p.chip_hover); // CMP-IBTN-1: pressed
    } else if resp.hovered() {
        ui.painter().circle_filled(rect.center(), 20.0, p.additive);
    }
    if resp.has_focus() {
        ui.painter()
            .circle_stroke(rect.center(), 19.0, Stroke::new(2.0, p.text));
    }
    glyph(
        ui.painter(),
        rect.center(),
        icon,
        24.0,
        color.unwrap_or(p.text),
        angle,
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
    resp
}

/// [`icon_button_ex`] with the text color and no rotation.
pub fn icon_button(ui: &mut Ui, p: &Palette, icon: char, tooltip: &str) -> Response {
    icon_button_ex(ui, p, icon, tooltip, None, 0.0)
}

/// Paints an icon glyph centered at `center`, rotated by `angle` radians about
/// its center.
pub fn glyph(
    painter: &egui::Painter,
    center: Pos2,
    icon: char,
    size: f32,
    color: Color32,
    angle: f32,
) {
    let galley = painter.layout_no_wrap(icon.to_string(), theme::icon_font(size), color);
    let half = galley.size() * 0.5;
    // The text shape rotates about its top-left corner; offset so the glyph
    // turns about its center.
    let rot = egui::emath::Rot2::from_angle(angle);
    let pos = center - rot * half;
    painter.add(egui::epaint::TextShape::new(pos, galley, color).with_angle(angle));
}

/// A pill button's look.
struct PillStyle {
    font: egui::FontId,
    fill: Color32,
    hover: Color32,
    fg: Color32,
    height: f32,
}

fn pill(ui: &mut Ui, text: &str, s: PillStyle, enabled: bool) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), s.font, s.fg);
    let size = vec2(galley.size().x + 32.0, s.height);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let bg = if enabled && resp.hovered() {
        s.hover
    } else {
        s.fill
    };
    ui.painter().rect_filled(rect, s.height * 0.5, bg);
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect,
            s.height * 0.5,
            Stroke::new(2.0, s.fg),
            egui::StrokeKind::Inside,
        );
    }
    let color = if enabled {
        s.fg
    } else {
        s.fg.gamma_multiply(0.45)
    };
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, color);
    let label = text.to_owned();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &label));
    resp
}

/// The tonal button (additive fill), size S.
pub fn tonal_button(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    let s = PillStyle {
        font: theme::medium(12.0),
        fill: p.additive,
        hover: p.chip_hover,
        fg: p.text,
        height: theme::size::BUTTON_S,
    };
    pill(ui, text, s, true)
}

/// The filled button (inverted fill), size S: the recommended choice among peers
/// (CMP-BTN-2).
pub fn filled_button(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    let s = PillStyle {
        font: theme::medium(12.0),
        fill: p.inverted,
        hover: p.inverted_hover,
        fg: p.text_inv,
        height: theme::size::BUTTON_S,
    };
    pill(ui, text, s, true)
}

/// A text button in `color`, size S.
pub fn text_button(ui: &mut Ui, color: Color32, text: &str) -> Response {
    let s = PillStyle {
        font: theme::medium(12.0),
        fill: Color32::TRANSPARENT,
        hover: color.gamma_multiply(0.15),
        fg: color,
        height: theme::size::BUTTON_S,
    };
    pill(ui, text, s, true)
}

/// The filled primary button, size M (36); plain when disabled.
pub fn cta_button(ui: &mut Ui, p: &Palette, text: &str, enabled: bool) -> Response {
    let (fill, fg) = if enabled {
        (p.primary, p.text_inv)
    } else {
        (p.additive, p.text_dis)
    };
    let s = PillStyle {
        font: theme::medium(14.0),
        fill,
        hover: if enabled { p.primary_hover } else { fill },
        fg,
        height: theme::size::BUTTON_M,
    };
    pill(ui, text, s, enabled)
}

/// A chip (32 high, radius 8, 14/500): additive when off, inverted when on.
pub fn chip(
    ui: &mut Ui,
    p: &Palette,
    text: &str,
    selected: bool,
    dot: Option<Color32>,
) -> Response {
    let font = theme::medium(14.0);
    let (fill, fg) = if selected {
        (p.inverted, p.text_inv)
    } else {
        (p.additive, p.text)
    };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let dot_w = if dot.is_some() { 14.0 } else { 0.0 };
    let size = vec2(galley.size().x + 24.0 + dot_w, theme::size::CHIP_H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let bg = if !selected && resp.hovered() {
        p.chip_hover
    } else {
        fill
    };
    ui.painter().rect_filled(rect, theme::radius::CHIP, bg);
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.shrink(2.0),
            theme::radius::CHIP,
            Stroke::new(2.0, if selected { p.base } else { p.text }),
            egui::StrokeKind::Inside,
        );
    }
    let mut x = rect.left() + 12.0;
    if let Some(c) = dot {
        ui.painter()
            .circle_filled(pos2(x + 4.0, rect.center().y), 4.0, c);
        x += dot_w;
    }
    ui.painter()
        .galley(pos2(x, rect.center().y - galley.size().y * 0.5), galley, fg);
    let label = text.to_owned();
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, &label)
    });
    resp
}

/// A status dot of `radius`, blinking when `blink` (opacity per
/// [`crate::projects::blink_alpha`] at `t` seconds).
pub fn dot(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    color: Color32,
    blink: bool,
    t: f32,
) {
    let alpha = if blink {
        crate::projects::blink_alpha(t)
    } else {
        1.0
    };
    painter.circle_filled(center, radius, color.gamma_multiply(alpha));
}

/// Paints `color` over the four corners outside a rounded rect of `radius`, so
/// content drawn as a square (the galaxy's texture) reads as the rounded stage.
pub fn mask_corners(painter: &egui::Painter, rect: Rect, radius: f32, color: Color32) {
    let steps = 8;
    let corners = [
        (
            rect.left_top(),
            pos2(rect.left() + radius, rect.top() + radius),
            std::f32::consts::PI,
        ),
        (
            rect.right_top(),
            pos2(rect.right() - radius, rect.top() + radius),
            1.5 * std::f32::consts::PI,
        ),
        (
            rect.right_bottom(),
            pos2(rect.right() - radius, rect.bottom() - radius),
            0.0,
        ),
        (
            rect.left_bottom(),
            pos2(rect.left() + radius, rect.bottom() - radius),
            0.5 * std::f32::consts::PI,
        ),
    ];
    for (corner, center, start) in corners {
        // A fan from the corner along the arc: convex pieces only.
        let arc: Vec<Pos2> = (0..=steps)
            .map(|i| {
                let a = start + (i as f32 / steps as f32) * std::f32::consts::FRAC_PI_2;
                center + vec2(a.cos(), a.sin()) * radius
            })
            .collect();
        for w in arc.windows(2) {
            painter.add(egui::Shape::convex_polygon(
                vec![corner, w[0], w[1]],
                color,
                Stroke::NONE,
            ));
        }
    }
}

/// A section title in the guide (16/22 500).
pub fn guide_title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).font(theme::medium(16.0)).color(p.text));
    ui.add_space(4.0);
}

/// One full-guide entry (40 high, radius 10, icon 24 then text). Returns the
/// response.
pub fn guide_entry(ui: &mut Ui, p: &Palette, icon: char, text: &str, current: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    let bg = if current || resp.hovered() {
        p.additive
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, theme::radius::NAV_ENTRY, bg);
    glyph(
        ui.painter(),
        pos2(rect.left() + 24.0, rect.center().y),
        icon,
        24.0,
        p.text,
        0.0,
    );
    let font = if current {
        theme::medium(14.0)
    } else {
        theme::regular(14.0)
    };
    ui.painter().text(
        pos2(rect.left() + 60.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        font,
        p.text,
    );
    let label = text.to_owned();
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, current, &label)
    });
    resp
}

/// One mini-guide entry (64 wide, icon over a 10/14 label).
pub fn mini_entry(ui: &mut Ui, p: &Palette, icon: char, text: &str, current: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(64.0, 74.0), Sense::click());
    // CMP-NAV-3 (1.2): collapsed, the current entry has no fill; it shows its
    // filled icon (the caller passes it).
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, theme::radius::NAV_ENTRY, p.additive);
    }
    glyph(
        ui.painter(),
        pos2(rect.center().x, rect.top() + 28.0),
        icon,
        24.0,
        p.text,
        0.0,
    );
    ui.painter().text(
        pos2(rect.center().x, rect.top() + 52.0),
        egui::Align2::CENTER_CENTER,
        text,
        theme::regular(10.0),
        p.text,
    );
    let label = text.to_owned();
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, current, &label)
    });
    resp
}

/// The count badge on the bell (`--indicator`, COL-2).
pub fn badge(painter: &egui::Painter, anchor: Pos2, count: usize, p: &Palette) {
    let text = if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    };
    let galley = painter.layout_no_wrap(text, theme::regular(12.0), Color32::WHITE);
    let w = (galley.size().x + 10.0).max(20.0);
    let rect = Rect::from_min_size(anchor, vec2(w, 20.0));
    painter.rect_filled(rect.expand(2.0), 12.0, p.base);
    painter.rect_filled(rect, 10.0, theme::INDICATOR);
    painter.galley(rect.center() - galley.size() * 0.5, galley, Color32::WHITE);
}

/// A size in points as a `Vec2`, for readability at call sites.
pub fn sz(w: f32, h: f32) -> Vec2 {
    vec2(w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INT-7 (1.2): every target is at least 24 by 24 points, and buttons, chips
    /// and icon buttons are 32 or taller. Measured from the responses egui
    /// returns for the real widgets.
    #[test]
    fn every_control_meets_the_minimum_target_size() {
        type Sizes = std::rc::Rc<std::cell::RefCell<Vec<(&'static str, Vec2)>>>;
        let sizes: Sizes = Default::default();
        let seen = sizes.clone();
        let mut h = egui_kittest::Harness::new_ui(move |ui| {
            if !crate::theme::ensure(ui.ctx()) {
                return;
            }
            let p = Palette::dark();
            let icons = crate::icons::MENU;
            let rows: [(&'static str, Response); 8] = [
                ("icon button", icon_button(ui, &p, icons, "Guide")),
                ("chip", chip(ui, &p, "All", true, None)),
                ("tonal", tonal_button(ui, &p, "Allow always")),
                ("filled", filled_button(ui, &p, "Allow once")),
                ("text", text_button(ui, p.text, "Cancel")),
                ("primary", cta_button(ui, &p, "Send", true)),
                (
                    "rail entry",
                    mini_entry(ui, &p, crate::icons::GRAPH, "Graph", true),
                ),
                (
                    "nav entry",
                    guide_entry(ui, &p, crate::icons::SEARCH, "Search", false),
                ),
            ];
            *seen.borrow_mut() = rows.iter().map(|(n, r)| (*n, r.rect.size())).collect();
        });
        h.run();
        let got = sizes.borrow();
        assert_eq!(got.len(), 8, "every control drew");
        for (name, s) in got.iter() {
            assert!(s.x >= 24.0 && s.y >= 24.0, "{name}: {s:?}");
            assert!(
                s.y >= 32.0,
                "{name} is a button, chip or icon button: {s:?}"
            );
        }
    }
}
