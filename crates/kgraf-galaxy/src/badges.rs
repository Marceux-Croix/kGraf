//! Count pills, the failed item's mark, fold handles and actor marks on the galaxy
//! (decision 0068), per `docs/STYLE_GUIDE.md` 1.4 and decision 0070. Painted with egui
//! over the rendered frame, as the labels are, so they cost no scene state and no GPU
//! upload. Every color comes from the [`Palette`].
//!
//! - **Count pill** (STG-DRAW-8): yellow, at the top right of its carrier, with the count
//!   of what needs the user. A failure needs the user, so it counts here too (a failure
//!   needs input, so it falls under yellow). A notice on a hidden node
//!   adds to the pill on its nearest drawn ancestor, or on the CPU-budget group that
//!   stands for it.
//! - **The failed item itself** (STG-DRAW-11): a 1.5 px `stage_danger` ring and a small
//!   orange pill at its top left with a dark error glyph. Orange appears nowhere else.
//! - **Tooltips and accessible names** name the kinds and the items: "1 failed gate, 2
//!   approvals: run/gate/g, run/c1" (COL-9, CMP-BDG-4).
//! - **Fold handles** (STG-DRAW-5): "+N" at the top left of a folded parent, in white at
//!   70%; a "-" on the selected expanded parent folds it again. Both are click targets.
//! - **Actors** (STG-DRAW-7): a letter on the disk and a state dot at its lower right:
//!   orange failed, yellow needs input, gray working.
//! - **Selection** (STG-DRAW-4): a 2 px cyan ring on the selected node, 1.5 px on
//!   search matches.
//! - Under a highlight, pills outside it draw at 40% of normal and handles at 15%
//!   (STG-DRAW-3).
//!
//! There is no bell here: the shell's masthead bell is the only one, with its own
//! count over every open project; [`crate::ProjectView::stage_tally`] is what these
//! pills sum to. Not built: opening the
//! matching cards from a pill (STG-DRAW-8; the activity card is the shell's): a click on
//! a pill selects its carrier; a blinking dot for working (STG-PERF-2). egui's default
//! font has one weight and proportional digits, so weight 500 and tabular numerals
//! (TYP-1, TYP-2) are not drawn.

use egui::{Color32, FontId, Pos2, Rect, Stroke, vec2};
use glam::Vec3;

use crate::camera::CameraFrame;
use crate::fold::{Badge, Carrier, Fold, NoticeKind, Toggle};
use crate::galaxy::Galaxy;
use crate::palette::Palette;
use crate::pick::core_px;

/// Pill height in points; the radius is half of it (STG-DRAW-8).
pub const PILL_HEIGHT: f32 = 18.0;
/// The failed item's pill (STG-DRAW-11): 18 px, as the count pill.
pub const DANGER_PILL_HEIGHT: f32 = 18.0;
/// The pill's outline: 3 px black.
pub const PILL_OUTLINE: f32 = 3.0;
/// The count on a pill: 11 px.
pub const PILL_FONT_SIZE: f32 = 11.0;
/// The error glyph on the failed item's pill: 12 px (STG-DRAW-11).
pub const ERROR_ICON_SIZE: f32 = 12.0;
/// A failed item's ring (STG-DRAW-11).
pub const FAILED_RING_WIDTH: f32 = 1.5;
/// Pills outside an active highlight: 40% of normal (STG-DRAW-3).
pub const PILL_DIM: f32 = 0.40;
/// The fold handle: 10 px (STG-DRAW-5).
pub const FOLD_MARK_SIZE: f32 = 10.0;
/// Handles outside an active highlight: 15% of normal (STG-DRAW-3).
pub const FOLD_MARK_DIM: f32 = 0.15;
/// Most "+N" handles drawn at once, those hiding the most first (the selected and the
/// hovered ones are always drawn). Chosen: past this, handles crowd the picture they
/// are meant to ease.
pub const MAX_FOLD_MARKS: usize = 200;
/// A folded node gets its "+N" without being selected or hovered only when its drawn
/// core is at least this radius, in points: zoomed in far enough that the handle reads
/// as belonging to it. Chosen.
pub const FOLD_MARK_MIN_RADIUS: f32 = 10.0;
/// Selection ring and match ring widths (STG-DRAW-4).
pub const SELECTION_RING_WIDTH: f32 = 2.0;
pub const MATCH_RING_WIDTH: f32 = 1.5;
/// Most match rings drawn at once.
pub const MAX_MATCH_RINGS: usize = 200;
/// Actor letter size and state dot (STG-DRAW-7, CMP-AV-4).
pub const ACTOR_LETTER_SIZE: f32 = 12.0;
pub const ACTOR_DOT_FRACTION: f32 = 0.34;
pub const ACTOR_DOT_MIN: f32 = 8.0;

/// What a click on a drawn mark does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkAction {
    /// A fold handle: expand or fold its node.
    Toggle(Toggle),
    /// A pill: select what carries it (the matching cards are the shell's).
    Select(u32),
}

/// A click target drawn this frame, in points, and the node it belongs to. A click on
/// it goes to the target only when no other node is picked there
/// ([`crate::view::click_at`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mark {
    pub rect: Rect,
    pub action: MarkAction,
    pub owner: u32,
}

/// An actor's state for its dot (CMP-AV-4, STG-DRAW-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorState {
    Working,
    NeedsInput,
    Failed,
    /// A session the orchestrator's pacer is delaying (decision 0067): a state, not a fault or a
    /// question, so COL-5 gray (never COL-3 yellow, which is reserved for "needs your input" and
    /// feeds the project dot, hamburger and bell aggregates). Drawn as a ring, not a filled dot, so
    /// it still reads as distinct from ordinary `Working`.
    Paced,
}

/// Where a carrier is drawn: its center and drawn core radius, in points.
fn place(
    galaxy: &Galaxy,
    frame: &CameraFrame,
    rect: Rect,
    ppp: f32,
    at: Carrier,
) -> Option<(Pos2, f32)> {
    let (world, size) = match at {
        Carrier::Node(s) => {
            if !galaxy.is_drawn(s) {
                return None;
            }
            (galaxy.shown_position(s)?, galaxy.drawn_size(s)?)
        }
        Carrier::Group(g) => {
            let grp = galaxy.visibility().groups().get(g as usize)?;
            (grp.position, grp.size * crate::selection::NODE_SCALE)
        }
    };
    let ([x, y], depth) = frame.project(Vec3::from(world))?;
    let r = core_px(
        size,
        depth,
        frame.pixel_world_size_per_depth,
        galaxy.budget().max_core_px(),
    ) / ppp;
    let c = Pos2::new(rect.min.x + x / ppp, rect.min.y + y / ppp);
    rect.contains(c).then_some((c, r))
}

fn frame_of(ui: &egui::Ui, rect: Rect, galaxy: &Galaxy) -> (CameraFrame, f32) {
    let ppp = ui.ctx().pixels_per_point();
    (
        galaxy
            .camera()
            .frame(rect.width() * ppp, rect.height() * ppp),
        ppp,
    )
}

/// Whether a carrier is outside an active highlight.
fn dimmed(galaxy: &Galaxy, at: Carrier) -> bool {
    match (galaxy.selection(), at) {
        (Some(sel), Carrier::Node(s)) => !sel.is_highlighted(s),
        (Some(_), Carrier::Group(_)) => true,
        (None, _) => false,
    }
}

/// A tooltip naming the kinds and the items: "1 failed gate, 2 approvals: g, c1".
pub fn words(badge: &Badge, name_of: &dyn Fn(u32) -> String) -> String {
    let kinds = badge.tally.words();
    let mut names: Vec<String> = badge
        .sources
        .iter()
        .filter(|x| x.kind.needs_attention())
        .map(|x| name_of(x.slot))
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        kinds
    } else {
        format!("{kinds}: {}", names.join(", "))
    }
}

/// How a pill looks.
struct PillStyle {
    height: f32,
    fill: Color32,
    font: FontId,
    alpha: f32,
}

/// A pill with its outline, centered at `center`. Returns its rect.
fn pill(
    painter: &egui::Painter,
    palette: &Palette,
    center: Pos2,
    text: &str,
    style: PillStyle,
) -> Rect {
    let PillStyle {
        height,
        fill,
        font,
        alpha,
    } = style;
    let galley = painter.layout_no_wrap(text.to_owned(), font, palette.pill_text);
    let w = (galley.size().x + 8.0).max(height);
    let r = Rect::from_center_size(center, vec2(w, height));
    painter.rect_filled(
        r.expand(PILL_OUTLINE),
        height / 2.0 + PILL_OUTLINE,
        Color32::BLACK.gamma_multiply(alpha),
    );
    painter.rect_filled(r, height / 2.0, fill.gamma_multiply(alpha));
    painter.galley(
        r.center() - galley.size() / 2.0,
        galley,
        palette.pill_text.gamma_multiply(alpha),
    );
    r
}

/// Gives a pill a tooltip and an accessible name.
fn name_it(ui: &egui::Ui, r: Rect, id: (&'static str, usize), tip: &str) {
    let resp = ui.interact(r, ui.id().with(id), egui::Sense::hover());
    let named = tip.to_owned();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, named.clone()));
    resp.on_hover_text(tip);
}

/// Paints the count pills of `badges`, and the ring and small pill of each failed item
/// that is drawn. Returns the pills as click targets. `name_of` names a notice's node.
pub fn draw(
    ui: &egui::Ui,
    rect: Rect,
    galaxy: &Galaxy,
    badges: &[Badge],
    palette: &Palette,
    name_of: &dyn Fn(u32) -> String,
) -> Vec<Mark> {
    let (frame, ppp) = frame_of(ui, rect, galaxy);
    let painter = ui.painter_at(rect);
    let mut marks = Vec::new();
    for (i, b) in badges.iter().enumerate() {
        let count = b.tally.attention();
        if count == 0 {
            continue;
        }
        let Some((c, r)) = place(galaxy, &frame, rect, ppp, b.at) else {
            continue;
        };
        let alpha = if dimmed(galaxy, b.at) { PILL_DIM } else { 1.0 };
        // STG-DRAW-11: the failed item's own ring and small orange pill.
        let failed_here = b
            .sources
            .iter()
            .find(|x| x.kind == NoticeKind::Failed && Carrier::Node(x.slot) == b.at);
        if let Some(f) = failed_here {
            painter.circle_stroke(
                c,
                r + FAILED_RING_WIDTH,
                Stroke::new(FAILED_RING_WIDTH, palette.danger.gamma_multiply(alpha)),
            );
            let dr = pill(
                &painter,
                palette,
                c + vec2(-r, -r),
                "!",
                PillStyle {
                    height: DANGER_PILL_HEIGHT,
                    fill: palette.danger,
                    font: FontId::proportional(ERROR_ICON_SIZE),
                    alpha,
                },
            );
            name_it(
                ui,
                dr,
                ("failed", i),
                &format!("failed: {}", name_of(f.slot)),
            );
            marks.push(Mark {
                rect: dr,
                action: MarkAction::Select(f.slot),
                owner: f.slot,
            });
        }
        let pr = pill(
            &painter,
            palette,
            c + vec2(r, -r),
            &count_text(count),
            PillStyle {
                height: PILL_HEIGHT,
                fill: palette.pill,
                font: FontId::proportional(PILL_FONT_SIZE),
                alpha,
            },
        );
        name_it(ui, pr, ("pill", i), &words(b, name_of));
        if let Carrier::Node(s) = b.at {
            marks.push(Mark {
                rect: pr,
                action: MarkAction::Select(s),
                owner: s,
            });
        }
    }
    marks
}

/// The count text, capped so a pill stays small.
pub fn count_text(total: u32) -> String {
    if total > 99 {
        "99+".to_owned()
    } else {
        total.to_string()
    }
}

/// Text with the stage's dark halo (TYP-4).
fn halo_text(
    painter: &egui::Painter,
    at: Pos2,
    galley: std::sync::Arc<egui::Galley>,
    color: Color32,
    halo: Color32,
) {
    for d in [[-1.0, 0.0], [1.0, 0.0], [0.0, -1.0], [0.0, 1.0]] {
        painter.galley(at + vec2(d[0], d[1]), galley.clone(), halo);
    }
    painter.galley(at, galley, color);
}

/// Paints the fold handles: "+N" on the selected and the hovered folded node, and on
/// folded nodes drawn at least [`FOLD_MARK_MIN_RADIUS`] (those hiding the most first, at
/// most [`MAX_FOLD_MARKS`]); "-" on the selected expanded node. A handle of the last
/// kind is skipped where it would cover another drawn node's center, so no handle sits
/// on a node the user may want to click (and a click there picks the node anyway).
/// Returns the handles as click targets.
pub fn draw_fold_marks(
    ui: &egui::Ui,
    rect: Rect,
    galaxy: &Galaxy,
    fold: &Fold,
    hovered: Option<u32>,
    palette: &Palette,
) -> Vec<Mark> {
    let (frame, ppp) = frame_of(ui, rect, galaxy);
    let painter = ui.painter_at(rect);
    let font = FontId::proportional(FOLD_MARK_SIZE);
    let selected = galaxy.selection().map(|s| s.selected());
    // (slot, text, action, always): `always` for the selected and hovered handles.
    let mut todo: Vec<(u32, String, Toggle, bool)> = Vec::new();
    for s in [selected, hovered].into_iter().flatten() {
        if todo.iter().any(|t| t.0 == s) || !galaxy.is_drawn(s) {
            continue;
        }
        if fold.hidden_below(s) > 0 {
            todo.push((
                s,
                format!("+{}", fold.hidden_below(s)),
                Toggle::Expand(s),
                true,
            ));
        } else if Some(s) == selected && fold.is_expanded(s) {
            todo.push((s, "-".to_owned(), Toggle::Collapse(s), true));
        }
    }
    let mut folded: Vec<(u32, u32, Pos2, f32)> = (0..fold.len() as u32)
        .filter(|&s| {
            fold.hidden_below(s) > 0 && galaxy.is_drawn(s) && !todo.iter().any(|t| t.0 == s)
        })
        .filter_map(|s| {
            let (c, r) = place(galaxy, &frame, rect, ppp, Carrier::Node(s))?;
            (r >= FOLD_MARK_MIN_RADIUS).then_some((fold.hidden_below(s), s, c, r))
        })
        .collect();
    folded.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    folded.truncate(MAX_FOLD_MARKS);
    // Centers of the drawn nodes, only when some handle might cover one.
    let centers: Vec<(u32, Pos2)> = if folded.is_empty() {
        Vec::new()
    } else {
        (0..galaxy.scene().nodes().len() as u32)
            .filter(|&s| galaxy.is_drawn(s))
            .filter_map(|s| {
                let (c, _) = place(galaxy, &frame, rect, ppp, Carrier::Node(s))?;
                Some((s, c))
            })
            .collect()
    };
    todo.extend(
        folded
            .into_iter()
            .map(|(n, s, _, _)| (s, format!("+{n}"), Toggle::Expand(s), false)),
    );
    let mut marks = Vec::new();
    for (s, text, action, always) in todo {
        let Some((c, r)) = place(galaxy, &frame, rect, ppp, Carrier::Node(s)) else {
            continue;
        };
        let at = c + vec2(-r, -r);
        let galley = painter.layout_no_wrap(text, font.clone(), palette.fold_mark);
        let hit = Rect::from_min_size(at - galley.size(), galley.size()).expand(2.0);
        if !always && centers.iter().any(|&(o, p)| o != s && hit.contains(p)) {
            continue;
        }
        let alpha = if dimmed(galaxy, Carrier::Node(s)) {
            FOLD_MARK_DIM
        } else {
            1.0
        };
        halo_text(
            &painter,
            hit.min + vec2(2.0, 2.0),
            galley,
            palette.fold_mark.gamma_multiply(alpha),
            palette.text_halo.gamma_multiply(alpha),
        );
        marks.push(Mark {
            rect: hit,
            action: MarkAction::Toggle(action),
            owner: s,
        });
    }
    marks
}

/// Paints the selection ring on the selected node and match rings on a search's other
/// matches (STG-DRAW-4).
pub fn draw_selection(ui: &egui::Ui, rect: Rect, galaxy: &Galaxy, palette: &Palette) {
    let Some(sel) = galaxy.selection() else {
        return;
    };
    let (frame, ppp) = frame_of(ui, rect, galaxy);
    let painter = ui.painter_at(rect);
    if let Some((c, r)) = place(galaxy, &frame, rect, ppp, Carrier::Node(sel.selected())) {
        painter.circle_stroke(
            c,
            r + SELECTION_RING_WIDTH + 1.0,
            Stroke::new(SELECTION_RING_WIDTH, palette.selection_ring),
        );
    }
    if sel.is_set() {
        for &m in sel.members().iter().skip(1).take(MAX_MATCH_RINGS) {
            if let Some((c, r)) = place(galaxy, &frame, rect, ppp, Carrier::Node(m)) {
                painter.circle_stroke(
                    c,
                    r + MATCH_RING_WIDTH + 1.0,
                    Stroke::new(MATCH_RING_WIDTH, palette.selection_ring),
                );
            }
        }
    }
}

/// Paints each actor's letter and state dot (STG-DRAW-7, CMP-AV-4). `resolver`
/// (style guide CMP-AV-6) draws the state mark as a rounded
/// square instead of a circle: a shape difference, never a new color, so a
/// resolver is never mistaken for an ordinary actor.
pub fn draw_actors(
    ui: &egui::Ui,
    rect: Rect,
    galaxy: &Galaxy,
    actors: &[(u32, char, ActorState, bool)],
    palette: &Palette,
) {
    let (frame, ppp) = frame_of(ui, rect, galaxy);
    let painter = ui.painter_at(rect);
    let font = FontId::proportional(ACTOR_LETTER_SIZE);
    for &(slot, letter, state, resolver) in actors {
        let Some((c, r)) = place(galaxy, &frame, rect, ppp, Carrier::Node(slot)) else {
            continue;
        };
        let alpha = if dimmed(galaxy, Carrier::Node(slot)) {
            FOLD_MARK_DIM
        } else {
            1.0
        };
        let galley = painter.layout_no_wrap(letter.to_string(), font.clone(), palette.text);
        halo_text(
            &painter,
            c - galley.size() / 2.0,
            galley,
            palette.text.gamma_multiply(alpha),
            palette.text_halo.gamma_multiply(alpha),
        );
        let dot = match state {
            ActorState::Failed => palette.danger,
            ActorState::NeedsInput => palette.pill,
            ActorState::Working | ActorState::Paced => {
                crate::palette::to_color32(palette.state_neutral)
            }
        };
        let d = (r * 2.0 * ACTOR_DOT_FRACTION).max(ACTOR_DOT_MIN);
        let at = c + vec2(r * 0.72, r * 0.72);
        if resolver {
            let half = d / 2.0 + 2.0;
            let outer = Rect::from_center_size(at, vec2(half * 2.0, half * 2.0));
            painter.rect_filled(outer, half * 0.3, Color32::BLACK.gamma_multiply(alpha));
            let inner = Rect::from_center_size(at, vec2(d, d));
            if state == ActorState::Paced {
                painter.rect_stroke(
                    inner,
                    d * 0.15,
                    Stroke::new(1.5, dot.gamma_multiply(alpha)),
                    egui::StrokeKind::Middle,
                );
            } else {
                painter.rect_filled(inner, d * 0.15, dot.gamma_multiply(alpha));
            }
            continue;
        }
        painter.circle_filled(at, d / 2.0 + 2.0, Color32::BLACK.gamma_multiply(alpha));
        if state == ActorState::Paced {
            // A ring, not a fill (COL-5: a state, not a fault or a needs-input item),
            // so pacing still reads as distinct from ordinary `Working`.
            painter.circle_stroke(at, d / 2.0, Stroke::new(1.5, dot.gamma_multiply(alpha)));
        } else {
            painter.circle_filled(at, d / 2.0, dot.gamma_multiply(alpha));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{Detail, Notice, Tally};

    #[test]
    fn words_name_the_kinds_and_the_items_and_skip_working() {
        let n = |slot, kind, detail| Notice { slot, kind, detail };
        let sources = vec![
            n(1, NoticeKind::Failed, Detail::FailedGate),
            n(2, NoticeKind::NeedsInput, Detail::Approval),
            n(3, NoticeKind::NeedsInput, Detail::Approval),
            n(4, NoticeKind::Working, Detail::Working),
        ];
        let tally = Tally {
            counts: [1, 2, 1, 0],
            details: [1, 0, 0, 2, 1],
        };
        let badge = Badge {
            at: Carrier::Node(1),
            tally,
            sources,
        };
        let name = |s: u32| format!("w{s}");
        assert_eq!(
            words(&badge, &name),
            "1 failed gate, 2 approvals: w1, w2, w3"
        );
        assert_eq!(
            badge.tally.attention(),
            3,
            "failures count in the yellow pill"
        );
        assert_eq!(count_text(7), "7");
        assert_eq!(count_text(250), "99+");
    }

    #[test]
    fn the_sizes_are_the_style_guide_values() {
        assert_eq!(
            (PILL_HEIGHT, PILL_OUTLINE, PILL_FONT_SIZE),
            (18.0, 3.0, 11.0)
        );
        assert_eq!((PILL_DIM, FOLD_MARK_DIM), (0.40, 0.15));
        assert_eq!((DANGER_PILL_HEIGHT, ERROR_ICON_SIZE), (18.0, 12.0));
        assert_eq!(
            (SELECTION_RING_WIDTH, MATCH_RING_WIDTH, FAILED_RING_WIDTH),
            (2.0, 1.5, 1.5)
        );
    }
}
