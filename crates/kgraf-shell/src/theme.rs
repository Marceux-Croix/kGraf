//! The shell's one theme module: every token of the Stage style, as named values,
//! for the dark and the light theme. No other module names a chrome color
//! (THM-4).
//!
//! **Tokens follow `docs/STYLE_GUIDE.md` section 17** (the token block), with the
//! same names in Rust case: `--primary` is [`Palette::primary`], `--accent` is
//! [`ACCENT`], `--indicator` is [`INDICATOR`], `--progress` is [`PROGRESS`],
//! `--stage-bg` is [`STAGE_BG`], `--stage-popup` is [`stage_popup`],
//! `--stage-danger` is the stage [`kgraf_core::palette::Semantic::danger`],
//! `--nav-mini` and `--nav-full` are [`size::NAV_MINI`] and [`size::NAV_FULL`],
//! `--secondary-w` is [`size::secondary_w`], and the easings are in [`motion`].
//! Values are the "Nocturne" palette of style guide 1.8. Text is Instrument Sans
//! 400, 500 and 700 and code is Fragment Mono 400 (style guide 1.9, table 4.1; both
//! OFL 1.1, real family names kept, AST-4), icons are Material Symbols (Apache
//! 2.0), all embedded from `assets/` (section 15; `assets/SOURCES.md`). The
//! Instrument Sans files have tabular numerals baked in (TYP-2, see
//! `assets/fonts/build_fonts.py`). All font loading is in [`fonts`] and
//! [`family`], so a new typeface changes this module only.
//!
//! **Indicators (COL-1 to COL-5, binding).** Five colors carry
//! meaning and nothing else does: [`Indicator::Progress`] violet (the accent),
//! [`Indicator::Danger`] orange (broken, failed, dangerous, degraded; waiting is
//! gray with the reason in words), [`Indicator::NeedsInput`] yellow (needs input,
//! a failure that needs the user included), [`Indicator::Selection`] cyan, and
//! [`Indicator::Neutral`] gray for everything else. On the stage, cyan marks a
//! selection or a match; on the page, cyan also marks the primary action and "new
//! since you last looked". A provider is never encoded in color (COL-8).
//!
//! **One semantic palette.** The five values come from
//! [`kgraf_core::palette::Semantic`], chosen at run time from the surface (dark
//! page, light page, stage) and a vision mode, so a color-blind mode is a new
//! table in `kgraf-core` and no reader changes. The galaxy depends on
//! `kgraf-core` too and reads the same struct once its public API takes it (after
//! the chunking stream merges; today its colors are its own).

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, TextStyle};

use crate::settings::Appearance;

/// `#RRGGBB` as a color.
const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// `rgba(r, g, b, a)` with `a` in 0..=1, as the style guide writes it.
fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, (a * 255.0).round() as u8)
}

/// WCAG 2.2 relative luminance (Rec. 709 coefficients on linear light), the
/// same formula `kgraf_core::palette`'s test module and
/// the style guide's contrast script use.
fn relative_luminance(c: Color32) -> f32 {
    fn lin(v: u8) -> f32 {
        let v = f32::from(v) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG 2.2 contrast ratio between two **opaque** colors (decision 0070).
/// `Color32` stores premultiplied alpha (ecolor's own convention:
/// `.r()`/`.g()`/`.b()` are "component multiplied by alpha"), so
/// a translucent color's raw components are not the color it displays as; a
/// caller must composite over the real background first (`Color32::blend`,
/// egui's own premultiplied-over blend) and pass the opaque result. Debug
/// builds catch a caller that forgets: an uncomposited translucent fill
/// (such as `danger_bg`) would otherwise pass this check silently, so the
/// assertions below panic on it.
pub fn contrast_ratio(fg: Color32, bg: Color32) -> f32 {
    debug_assert_eq!(
        fg.a(),
        255,
        "contrast_ratio takes opaque colors only: {fg:?}"
    );
    debug_assert_eq!(
        bg.a(),
        255,
        "contrast_ratio takes opaque colors only: {bg:?}"
    );
    let (a, b) = (relative_luminance(fg), relative_luminance(bg));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `fg`, an **opaque** straight color, blended at `alpha` over the opaque
/// `bg`, in sRGB. This is a synthetic blend for a color that is not itself
/// translucent (`safe_tint`'s use: a solid semantic color tinted into a
/// background at a chosen weight); it is not for compositing an actual
/// translucent `Color32` fill (a real `rgba(...)` token) over its
/// background, which is `Color32::blend` (premultiplied-over, correct for a
/// value `Color32` already stores premultiplied) below in
/// `vision_modes_extended_wcag_pairs`.
fn blend_over(fg: Color32, alpha: f32, bg: Color32) -> Color32 {
    debug_assert_eq!(
        fg.a(),
        255,
        "blend_over's fg must be a straight opaque color: {fg:?}"
    );
    let mix = |f: u8, b: u8| -> u8 {
        (f32::from(f) * alpha + f32::from(b) * (1.0 - alpha)).round() as u8
    };
    Color32::from_rgb(
        mix(fg.r(), bg.r()),
        mix(fg.g(), bg.g()),
        mix(fg.b(), bg.b()),
    )
}

/// A badge fill tinted with `meaning` (the vision-remapped color it will hold
/// text of), at the largest alpha up to `max_alpha` that still keeps `need`:1
/// contrast between `meaning` and the tinted fill (bisection: 24 halvings is
/// far past sRGB's 8-bit precision). Decision 0070: a fixed alpha
/// (`--danger-bg`'s style guide value, `.16`) was only ever measured against
/// the one danger orange the style guide ships; a badge tinted with whatever
/// color a color-blind mode remapped danger to needs its own room checked,
/// not assumed. `Vision::Standard` never reaches this: its badges keep the
/// style guide's literal tokens, unrounded by any solver.
fn safe_tint(meaning: Color32, bg: Color32, max_alpha: f32, need: f32) -> Color32 {
    let mut lo = 0.0_f32;
    let mut hi = max_alpha;
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if contrast_ratio(meaning, blend_over(meaning, mid, bg)) >= need {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    blend_over(meaning, lo, bg)
}

/// `--accent` (violet): progress and live only (COL-1): the live dot, the
/// scrubber, the one accent node of the kGraf mark.
pub const ACCENT: Color32 = hex(0x8B5CF6);
/// `--indicator` (a deeper violet): the count badge and the live status badge
/// (COL-2), because white text on the accent is under 4.5:1.
pub const INDICATOR: Color32 = hex(0x7C3AED);

/// The progress gradient (`--progress`): `from` up to `stop` of the length,
/// then toward `to`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gradient {
    pub from: Color32,
    pub to: Color32,
    pub stop: f32,
}

/// `progress`: #8B5CF6 to 70%, then toward #C084FC.
pub const PROGRESS: Gradient = Gradient {
    from: hex(0x8B5CF6),
    to: hex(0xC084FC),
    stop: 0.7,
};
// `owner` and `owner_text` are retired (style guide 1.6, CMP-AV-3): the user's
// avatar and name chip are the inverted fill with `text_inv`.
/// The stage is near-black in both themes, `stage_bg` (section 3.4). From
/// `kgraf_core::palette::STAGE`, which the galaxy reads too.
pub const STAGE_BG: Color32 = bytes(kgraf_core::palette::STAGE.background);

/// sRGB bytes as a color.
const fn bytes(c: kgraf_core::palette::Rgb) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Stage popups and tooltips, `stage_popup`: rgba(22,25,38,.92).
pub fn stage_popup() -> Color32 {
    rgba(22, 25, 38, 0.92)
}

/// Stage literals of section 3.4 and the STG rules, as egui colors. The values live
/// once, in `kgraf_core::palette::STAGE`, which the galaxy maps into its palette too.
pub mod stage {
    use super::{Color32, bytes};
    use kgraf_core::palette::STAGE;
    /// A task card's fill (STG-DRAW-5).
    pub const CARD: Color32 = bytes(STAGE.card);
    /// An actor disk's fill (STG-DRAW-7).
    pub const ACTOR: Color32 = bytes(STAGE.actor);
    /// An actor dot on the minimap and the "off" switch thumb (STG-8, CMP-SET-4).
    pub const LIGHT: Color32 = bytes(STAGE.light);
    /// A node at rest: rgba(160,166,186,.58).
    pub fn node() -> Color32 {
        let [r, g, b] = STAGE.node.rgb;
        super::rgba(r, g, b, STAGE.node.alpha)
    }
    /// The relation arrow in the legend.
    pub const RELATION: Color32 = bytes(STAGE.relation);
    /// The dark icon on a stage danger pill (STG-DRAW-11).
    pub const PILL_ICON: Color32 = bytes(STAGE.pill_icon);
}

/// The galaxy's palette for the stage: the stage semantic colors under `vision`, plus
/// the stage literals. One source: the shell's chrome reads the same two tables.
pub fn galaxy_palette(vision: kgraf_core::palette::Vision) -> kgraf_galaxy::Palette {
    kgraf_galaxy::Palette::from_semantic(&stage_semantic_for(vision), &kgraf_core::palette::STAGE)
}

/// The five indicator meanings (decision 0070). Nothing else in
/// the chrome is colored for meaning. Their values come from one run-time
/// [`kgraf_core::palette::Semantic`], which the galaxy can read too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    /// Violet (the accent): progress and live.
    Progress,
    /// Orange: broken, failed, dangerous or degraded. Waiting is not orange.
    Danger,
    /// Yellow: needs your input (a failure that needs you included), and nothing else.
    NeedsInput,
    /// Cyan: your selection or a match (on the page also "new since you looked").
    Selection,
    /// Gray: everything else, including working.
    Neutral,
}

/// One theme's surface, text, action and state colors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    // Surfaces.
    pub base: Color32,
    pub raised: Color32,
    pub menu: Color32,
    pub inverted: Color32,
    pub inverted_hover: Color32,
    pub additive: Color32,
    pub additive_soft: Color32,
    pub chip_hover: Color32,
    pub outline: Color32,
    pub outline_opaque: Color32,
    // Text.
    pub text: Color32,
    pub text_2: Color32,
    pub text_3: Color32,
    pub text_dis: Color32,
    pub text_inv: Color32,
    // Actions.
    pub primary: Color32,
    pub primary_hover: Color32,
    pub primary_inv: Color32,
    pub suggested: Color32,
    // States. The retired `error`, `error_bg`, `warn_bg`, `warn_text` and
    // `icon_warn` are one orange family (style guide 1.4; 1.8 values).
    /// Orange text and icons: broken, failed, dangerous, degraded.
    pub danger: Color32,
    /// Orange badge fill.
    pub danger_bg: Color32,
    /// Yellow: needs your input.
    pub needs_input: Color32,
    pub green: Color32,
    /// The five semantic colors this palette was built from.
    pub semantic: kgraf_core::palette::Semantic,
    // Search.
    pub search_bg: Color32,
    pub search_border: Color32,
    pub search_btn: Color32,
    pub search_btn_border: Color32,
    pub search_focus: Color32,
    // Other.
    pub tooltip: Color32,
    pub scrim: Color32,
    pub scrollbar: Color32,
    /// The menu shadow's color (`0 4px 32px`).
    pub shadow_menu: Color32,
}

impl Palette {
    /// `html[data-theme="dark"]`, standard color vision. Most callers that do
    /// not have an `egui::Context` at hand (avatars, tests, a headless
    /// harness) do not need a color-blind reader either; [`Palette::current`]
    /// is the one that reads the live setting.
    pub fn dark() -> Self {
        Self::dark_with_vision(kgraf_core::palette::Vision::default())
    }

    /// `html[data-theme="dark"]` under `vision` (decision 0070).
    pub fn dark_with_vision(vision: kgraf_core::palette::Vision) -> Self {
        let dark_sem = kgraf_core::palette::Semantic::for_surface(
            kgraf_core::palette::Surface::PageDark,
            vision,
        );
        Self {
            dark: true,
            base: hex(0x0D0F17),
            raised: hex(0x171A26),
            menu: hex(0x1D2130),
            inverted: hex(0xEDEFF7),
            inverted_hover: hex(0xD3D7E4),
            additive: rgba(255, 255, 255, 0.1),
            additive_soft: rgba(255, 255, 255, 0.05),
            chip_hover: rgba(255, 255, 255, 0.2),
            outline: rgba(255, 255, 255, 0.2),
            outline_opaque: hex(0x2E3345),
            text: hex(0xEDEFF7),
            text_2: hex(0x9AA0B4),
            text_3: rgba(255, 255, 255, 0.54),
            text_dis: hex(0x6B7186),
            text_inv: hex(0x0D0F17),
            primary: hex(0x22D3EE),
            primary_hover: hex(0x67E8F9),
            primary_inv: hex(0x0E7490),
            suggested: hex(0x10303B),
            danger: rgb(dark_sem.danger),
            danger_bg: if vision == kgraf_core::palette::Vision::Standard {
                // Style guide 3.3, `--danger-bg` dark: the exact literal token.
                rgba(251, 146, 60, 0.16)
            } else {
                safe_tint(rgb(dark_sem.danger), hex(0x0D0F17), 0.16, 4.5)
            },
            needs_input: rgb(dark_sem.needs_input),
            green: hex(0x34D399),
            semantic: dark_sem,
            search_bg: hex(0x0A0C13),
            search_border: hex(0x2A2F40),
            search_btn: rgba(255, 255, 255, 0.06),
            search_btn_border: hex(0x2A2F40),
            search_focus: hex(0x0891B2),
            tooltip: rgba(46, 51, 69, 0.94),
            scrim: rgba(5, 6, 12, 0.6),
            scrollbar: hex(0x4A5068),
            shadow_menu: rgba(0, 0, 0, 0.35),
        }
    }

    /// `html[data-theme="light"]`, standard color vision. See [`Palette::dark`].
    pub fn light() -> Self {
        Self::light_with_vision(kgraf_core::palette::Vision::default())
    }

    /// `html[data-theme="light"]` under `vision` (decision 0070).
    pub fn light_with_vision(vision: kgraf_core::palette::Vision) -> Self {
        let light_sem = kgraf_core::palette::Semantic::for_surface(
            kgraf_core::palette::Surface::PageLight,
            vision,
        );
        Self {
            dark: false,
            base: hex(0xF7F8FC),
            raised: hex(0xFFFFFF),
            menu: hex(0xFFFFFF),
            inverted: hex(0x10131C),
            inverted_hover: hex(0x262A38),
            additive: rgba(16, 19, 28, 0.05),
            additive_soft: rgba(16, 19, 28, 0.03),
            chip_hover: rgba(16, 19, 28, 0.1),
            outline: rgba(16, 19, 28, 0.12),
            outline_opaque: hex(0xE1E4EE),
            text: hex(0x10131C),
            text_2: hex(0x5A6072),
            text_3: rgba(16, 19, 28, 0.6),
            text_dis: hex(0x8A90A2),
            text_inv: hex(0xFFFFFF),
            primary: hex(0x0E7490),
            primary_hover: hex(0x155E75),
            primary_inv: hex(0x22D3EE),
            suggested: hex(0xDDF6FB),
            danger: rgb(light_sem.danger),
            danger_bg: if vision == kgraf_core::palette::Vision::Standard {
                // Style guide 3.2, `--danger-bg` light: the exact literal token.
                hex(0xFFEDE3)
            } else {
                safe_tint(rgb(light_sem.danger), hex(0xF7F8FC), 0.16, 4.5)
            },
            needs_input: rgb(light_sem.needs_input),
            green: hex(0x047857),
            semantic: light_sem,
            search_bg: hex(0xFFFFFF),
            search_border: hex(0xC9CEDC),
            search_btn: hex(0xEEF0F6),
            search_btn_border: hex(0xC9CEDC),
            search_focus: hex(0x0E7490),
            tooltip: rgba(46, 51, 69, 0.94),
            scrim: rgba(16, 19, 28, 0.4),
            scrollbar: hex(0xA3A9BA),
            shadow_menu: rgba(16, 19, 28, 0.12),
        }
    }

    /// The palette of the theme egui resolved this frame, standard color
    /// vision. See [`Palette::dark`].
    pub fn of(theme: egui::Theme) -> Self {
        Self::of_with_vision(theme, kgraf_core::palette::Vision::default())
    }

    /// The palette of the theme egui resolved this frame, under `vision`.
    pub fn of_with_vision(theme: egui::Theme, vision: kgraf_core::palette::Vision) -> Self {
        match theme {
            egui::Theme::Dark => Self::dark_with_vision(vision),
            egui::Theme::Light => Self::light_with_vision(vision),
        }
    }

    /// The palette for this frame's context, with the color vision mode
    /// [`set_vision`] last wrote to `ctx` (standard if it never was, e.g. a
    /// headless harness that never calls [`install`] or [`apply_appearance`]).
    /// Reading it fresh here, every call, every frame, is how a change to the
    /// setting takes effect with no restart (decision 0070):
    /// there is nothing to invalidate.
    pub fn current(ctx: &egui::Context) -> Self {
        Self::of_with_vision(ctx.theme(), current_vision(ctx))
    }

    /// The color of an indicator meaning in this theme, from the semantic palette.
    pub fn indicator(&self, i: Indicator) -> Color32 {
        use kgraf_core::palette::Meaning;
        rgb(self.semantic.color(match i {
            Indicator::Progress => Meaning::Progress,
            Indicator::Danger => Meaning::Danger,
            Indicator::NeedsInput => Meaning::NeedsInput,
            Indicator::Selection => Meaning::Selection,
            Indicator::Neutral => Meaning::Neutral,
        }))
    }
}

/// The egui memory key [`set_vision`] and [`current_vision`] share. Not
/// persisted to disk (that is `Settings::color_vision`'s job, in
/// `settings.rs`); this is only the live value the current frame's widgets
/// read, so a color-blind mode applies with no restart.
fn vision_id() -> egui::Id {
    egui::Id::new("kgraf_color_vision")
}

/// Records `vision` as the mode every [`Palette::current`], [`stage_semantic`]
/// and [`stage_indicator`] call reads until the next call. [`install`] and
/// [`apply_appearance`] call this; nothing else needs to.
pub fn set_vision(ctx: &egui::Context, vision: kgraf_core::palette::Vision) {
    ctx.data_mut(|d| d.insert_temp(vision_id(), vision));
}

/// The vision mode last written by [`set_vision`], or [`Default::default`]
/// (standard) if it never was.
pub fn current_vision(ctx: &egui::Context) -> kgraf_core::palette::Vision {
    ctx.data(|d| d.get_temp(vision_id())).unwrap_or_default()
}

/// The semantic colors on the stage, which is near-black in both themes
/// (`stage_danger` and the rest).
pub fn stage_semantic() -> kgraf_core::palette::Semantic {
    stage_semantic_for(kgraf_core::palette::Vision::default())
}

/// The semantic colors on the stage under a color-vision mode.
pub fn stage_semantic_for(vision: kgraf_core::palette::Vision) -> kgraf_core::palette::Semantic {
    kgraf_core::palette::Semantic::for_surface(kgraf_core::palette::Surface::Stage, vision)
}

/// The semantic colors on the stage under `ctx`'s current color vision mode
/// (the live setting; see [`current_vision`]).
pub fn stage_semantic_live(ctx: &egui::Context) -> kgraf_core::palette::Semantic {
    stage_semantic_for(current_vision(ctx))
}

/// An indicator's color on the stage, under `ctx`'s current color vision mode.
pub fn stage_indicator(ctx: &egui::Context, i: Indicator) -> Color32 {
    let mut p = Palette::dark();
    p.semantic = stage_semantic_live(ctx);
    p.indicator(i)
}

/// A semantic color as an egui color.
pub fn rgb(c: kgraf_core::palette::Rgb) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Layout sizes, in points (CSS pixels).
pub mod size {
    /// `masthead_h`.
    pub const MASTHEAD_H: f32 = 56.0;
    /// `nav_mini`: the collapsed rail.
    pub const NAV_MINI: f32 = 72.0;
    /// `nav_full`: the expanded sidebar.
    pub const NAV_FULL: f32 = 240.0;
    /// `secondary_w`: `clamp(360, 28vw, 402)`.
    pub fn secondary_w(window_w: f32) -> f32 {
        (window_w * 0.28).clamp(360.0, 402.0)
    }
    pub const CHIP_H: f32 = 32.0;
    pub const CHIPBAR_H: f32 = 52.0;
    pub const SEARCH_H: f32 = 40.0;
    pub const SEARCH_BTN_W: f32 = 64.0;
    /// The input part; with the 64 point button the field is at most 732 (LAY-4).
    pub const SEARCH_MAX_W: f32 = 668.0;
    /// Button size M (the CSS builds 36, not the 40 its header comment says).
    pub const BUTTON_M: f32 = 36.0;
    pub const BUTTON_S: f32 = 32.0;
    pub const ICON_BUTTON: f32 = 40.0;
    pub const STAGE_CHROME_H: f32 = 60.0;
    /// The stage's selection title (top left): its top and height below the stage's
    /// top edge. A 40 point avatar and a 44 point text block, so it spans 12 to 56.
    pub const SELECTION_TITLE_TOP: f32 = 12.0;
    pub const SELECTION_TITLE_H: f32 = 44.0;
    pub const COLUMN_GAP: f32 = 24.0;
    pub const CARD_HEAD_H: f32 = 48.0;
}

/// Corner radii.
pub mod radius {
    use egui::CornerRadius;
    pub const STAGE: CornerRadius = CornerRadius::same(12);
    pub const CARD: CornerRadius = CornerRadius::same(12);
    pub const CHIP: CornerRadius = CornerRadius::same(8);
    pub const THUMB: CornerRadius = CornerRadius::same(8);
    pub const NAV_ENTRY: CornerRadius = CornerRadius::same(10);
    pub const MENU: CornerRadius = CornerRadius::same(12);
    pub const TOOLTIP: CornerRadius = CornerRadius::same(4);
}

/// Motion: the cubic beziers by name, and the indicator animations.
pub mod motion {
    use std::time::Duration;
    /// `ease_in`: cubic-bezier(0, 0, .2, 1).
    pub const EASE_IN: [f32; 4] = [0.0, 0.0, 0.2, 1.0];
    /// `ease_out`: cubic-bezier(.4, 0, 1, 1).
    pub const EASE_OUT: [f32; 4] = [0.4, 0.0, 1.0, 1.0];
    /// `ease_std`: cubic-bezier(.2, 0, 0, 1) (Material standard).
    pub const EASE_STD: [f32; 4] = [0.2, 0.0, 0.0, 1.0];
    /// `ease_search`: cubic-bezier(.2, 0, .6, 1).
    pub const EASE_SEARCH: [f32; 4] = [0.2, 0.0, 0.6, 1.0];
    /// `ease_spring`: cubic-bezier(.34, 1.56, .64, 1), the hamburger's wiggle.
    pub const EASE_SPRING: [f32; 4] = [0.34, 1.56, 0.64, 1.0];
    /// The working dot's blink: opacity 1 to 0.3 and back over 1.6 s.
    pub const BLINK_PERIOD_S: f32 = 1.6;
    /// Redraw interval while something blinks: about 15 a second (STG-PERF-2),
    /// instead of every display frame.
    pub const BLINK_FRAME: Duration = Duration::from_millis(66);
    /// The hamburger's needs-input wiggle: a 600 ms swing every 4.6 s.
    pub const WIGGLE_PERIOD_S: f32 = 4.6;
    pub const WIGGLE_S: f32 = 0.6;
    /// Its largest swing, in degrees.
    pub const WIGGLE_DEGREES: f32 = 8.0;
}

/// Font family names registered by [`fonts`].
pub mod family {
    /// `font`: Instrument Sans 400, the proportional default.
    pub const FONT: &str = "instrument-sans";
    /// Instrument Sans 500, for names, labels, chips and buttons (TYP-1).
    pub const MEDIUM: &str = "instrument-sans-medium";
    /// Instrument Sans 700, for page and dialog titles and the wordmark (TYP-1).
    pub const BOLD: &str = "instrument-sans-bold";
    /// `mono`: Fragment Mono 400, the monospace default.
    pub const MONO: &str = "fragment-mono";
    /// The Material Symbols glyphs of `kgraf-icons.ttf`.
    pub const ICONS: &str = "kgraf-icons";
}

const SANS: &[u8] = include_bytes!("../assets/fonts/InstrumentSans-Regular.ttf");
const SANS_MEDIUM: &[u8] = include_bytes!("../assets/fonts/InstrumentSans-Medium.ttf");
const SANS_BOLD: &[u8] = include_bytes!("../assets/fonts/InstrumentSans-Bold.ttf");
const MONO: &[u8] = include_bytes!("../assets/fonts/FragmentMono-Regular.ttf");
const ICON_FONT: &[u8] = include_bytes!("../assets/fonts/kgraf-icons.ttf");

/// Font definitions: Instrument Sans for text, Fragment Mono for code, the icon glyphs, with
/// egui's default fonts kept behind them for scripts outside the latin subset.
pub fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(FontData::from_static(bytes)),
        );
    };
    add(family::FONT, SANS);
    add(family::MEDIUM, SANS_MEDIUM);
    add(family::BOLD, SANS_BOLD);
    add(family::MONO, MONO);
    add(family::ICONS, ICON_FONT);
    let defaults_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let defaults_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let chain = |first: &[&str], rest: &[String]| {
        let mut v: Vec<String> = first.iter().map(|s| (*s).to_owned()).collect();
        v.push(family::ICONS.to_owned());
        v.extend(rest.iter().cloned());
        v
    };
    fonts.families.insert(
        FontFamily::Proportional,
        chain(&[family::FONT], &defaults_prop),
    );
    fonts.families.insert(
        FontFamily::Monospace,
        chain(&[family::MONO], &defaults_mono),
    );
    fonts.families.insert(
        FontFamily::Name(family::MEDIUM.into()),
        chain(&[family::MEDIUM, family::FONT], &defaults_prop),
    );
    fonts.families.insert(
        FontFamily::Name(family::BOLD.into()),
        chain(&[family::BOLD, family::FONT], &defaults_prop),
    );
    fonts.families.insert(
        FontFamily::Name(family::ICONS.into()),
        vec![family::ICONS.to_owned(), family::FONT.to_owned()],
    );
    fonts
}

/// Instrument Sans 700 at `size`: page and dialog titles, the wordmark (TYP-1).
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(family::BOLD.into()))
}

/// Instrument Sans 500 at `size`.
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(family::MEDIUM.into()))
}

/// Instrument Sans 400 at `size`.
pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

/// The icon glyphs at `size`.
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(family::ICONS.into()))
}

/// egui visuals from a palette: surfaces, widget states, selection, tooltips.
pub fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.base;
    v.window_fill = p.menu;
    v.window_stroke = egui::Stroke::NONE;
    v.window_corner_radius = radius::MENU;
    v.menu_corner_radius = radius::MENU;
    v.window_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 32,
        spread: 0,
        color: p.shadow_menu,
    };
    v.popup_shadow = v.window_shadow;
    v.extreme_bg_color = p.search_bg;
    v.faint_bg_color = p.additive_soft;
    v.code_bg_color = p.additive;
    v.hyperlink_color = p.primary;
    v.warn_fg_color = p.danger;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.suggested;
    // Text selection is the `Selection` meaning (COL-4: "the current
    // selection"), so it follows the vision-remapped semantic color, not the
    // fixed `primary` action color (audit note, decision 0070: `primary`
    // is reserved for the primary-action role from here on).
    v.selection.stroke = egui::Stroke::new(1.0, p.indicator(Indicator::Selection));
    let r = CornerRadius::same(8);
    let set = |w: &mut egui::style::WidgetVisuals, bg: Color32, fg: Color32| {
        w.bg_fill = bg;
        w.weak_bg_fill = bg;
        w.bg_stroke = egui::Stroke::NONE;
        w.fg_stroke = egui::Stroke::new(1.0, fg);
        w.corner_radius = r;
        w.expansion = 0.0;
    };
    set(&mut v.widgets.noninteractive, p.base, p.text);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.outline);
    set(&mut v.widgets.inactive, p.additive, p.text);
    set(&mut v.widgets.hovered, p.chip_hover, p.text);
    set(&mut v.widgets.active, p.outline, p.text);
    set(&mut v.widgets.open, p.additive, p.text);
    v.text_cursor.stroke = egui::Stroke::new(2.0, p.text);
    v
}

/// Text sizes: 20/28 dialog titles as `bold(20)` (TYP-1);
/// 16/22 names; 14/20 body; 12/18 meta; 13 code.
pub fn style(p: &Palette) -> egui::Style {
    let mut s = egui::Style {
        visuals: visuals(p),
        ..egui::Style::default()
    };
    s.text_styles = [
        (TextStyle::Heading, bold(20.0)),
        (TextStyle::Body, regular(14.0)),
        (TextStyle::Button, medium(14.0)),
        (TextStyle::Small, regular(12.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    s.spacing.item_spacing = egui::vec2(8.0, 6.0);
    s.spacing.button_padding = egui::vec2(12.0, 6.0);
    // CMP-TIP-4: tooltips show after 400 ms of hover.
    s.interaction.tooltip_delay = 0.4;
    s
}

/// Installs the fonts and both themes' styles, and applies the appearance and
/// color vision settings. Call once at startup.
pub fn install(ctx: &egui::Context, appearance: Appearance, vision: kgraf_core::palette::Vision) {
    ctx.set_fonts(fonts());
    apply_appearance(ctx, appearance, vision);
}

/// Whether this context has the theme's fonts; installs them when not. The
/// named families ([`medium`], [`icon_font`]) must be bound before any text uses
/// them, and new fonts apply from the next pass, so a caller that gets `false`
/// draws nothing this pass (a headless harness's first frame; the window installs
/// the fonts before its first frame).
pub fn ensure(ctx: &egui::Context) -> bool {
    let ready = ctx.fonts(|f| {
        f.definitions()
            .families
            .contains_key(&FontFamily::Name(family::MEDIUM.into()))
    });
    if !ready {
        install(
            ctx,
            Appearance::Dark,
            kgraf_core::palette::Vision::default(),
        );
        ctx.request_repaint();
    }
    ready
}

/// Sets both themes' styles, the theme preference and the live color vision
/// mode (again whenever either setting changes; `app.rs` compares both
/// against what it last applied and calls this again when either moved, so a
/// color-blind mode takes effect the next frame with no restart
/// (decision 0070).
pub fn apply_appearance(
    ctx: &egui::Context,
    appearance: Appearance,
    vision: kgraf_core::palette::Vision,
) {
    ctx.set_style_of(egui::Theme::Dark, style(&Palette::dark_with_vision(vision)));
    ctx.set_style_of(
        egui::Theme::Light,
        style(&Palette::light_with_vision(vision)),
    );
    ctx.set_theme(match appearance {
        Appearance::Device => egui::ThemePreference::System,
        Appearance::Dark => egui::ThemePreference::Dark,
        Appearance::Light => egui::ThemePreference::Light,
    });
    set_vision(ctx, vision);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stage chrome and the galaxy draw the same stage colors (one source): the
    /// background, a card, an actor disk, the relation arrow, the pill icon, and the
    /// stage's needs-input, danger and selection meanings.
    #[test]
    fn the_chrome_and_the_galaxy_share_the_stage_colors() {
        let ctx = egui::Context::default();
        let g = galaxy_palette(kgraf_core::palette::Vision::default());
        let c = kgraf_galaxy::palette::to_color32;
        assert_eq!(c(g.background), STAGE_BG);
        assert_eq!(c(g.card), stage::CARD);
        assert_eq!(c(g.agent), stage::ACTOR);
        assert_eq!(c(g.relation_arrow), stage::RELATION);
        assert_eq!(g.pill_text, stage::PILL_ICON);
        assert_eq!(g.pill, stage_indicator(&ctx, Indicator::NeedsInput));
        assert_eq!(g.danger, stage_indicator(&ctx, Indicator::Danger));
        assert_eq!(
            g.selection_ring,
            stage_indicator(&ctx, Indicator::Selection)
        );
    }

    /// The five indicator meanings are five distinct colors in each theme, and
    /// progress is the accent in both (style guide 1.8 values).
    #[test]
    fn five_indicator_colors_per_theme() {
        for p in [Palette::dark(), Palette::light()] {
            let all = [
                Indicator::Progress,
                Indicator::Danger,
                Indicator::NeedsInput,
                Indicator::Selection,
                Indicator::Neutral,
            ]
            .map(|i| p.indicator(i));
            for (i, a) in all.iter().enumerate() {
                for b in &all[i + 1..] {
                    assert_ne!(a, b, "{p:?}");
                }
            }
            assert_eq!(p.indicator(Indicator::Progress), ACCENT);
            assert_eq!(p.indicator(Indicator::Selection), p.primary);
            assert_eq!(p.indicator(Indicator::Neutral), p.text_2);
            assert_eq!(p.indicator(Indicator::NeedsInput), p.needs_input);
            assert_eq!(p.indicator(Indicator::Danger), p.danger);
        }
        assert_eq!(
            Palette::dark().indicator(Indicator::NeedsInput),
            hex(0xFACC15)
        );
        assert_eq!(Palette::light().needs_input, hex(0xA16207));
        assert_eq!(Palette::dark().danger, hex(0xFB923C));
        assert_eq!(Palette::light().danger, hex(0xB83C0B));
        assert_eq!(
            Palette::light().indicator(Indicator::Selection),
            hex(0x0E7490)
        );
        let ctx = egui::Context::default();
        assert_eq!(stage_indicator(&ctx, Indicator::Danger), hex(0xFF7A33));
        assert_eq!(stage_indicator(&ctx, Indicator::Selection), hex(0x22D3EE));
        assert_eq!(stage_indicator(&ctx, Indicator::NeedsInput), hex(0xFACC15));
        assert_eq!(stage_indicator(&ctx, Indicator::Neutral), hex(0x9AA0B4));
    }

    /// Every style guide 14.1 pair that involves a color-blind-remapped
    /// meaning still meets its AA floor, on every vision mode: the badge fill
    /// (derived from the remapped danger, not the fixed standard orange),
    /// the confirm prompt on a card (danger on `base` + `additive`), degraded
    /// state text on a menu (danger on `menu`), and the selection ring on the
    /// page (`semantic.selection`, which was decoupled from the fixed
    /// `primary` action color). `kgraf-core`'s own `vision_modes_meet_wcag_aa`
    /// covers the plain-page and plain-stage pairs; this one covers the
    /// composited surfaces that only exist here, in the shell's tokens.
    #[test]
    fn vision_modes_extended_wcag_pairs() {
        for vision in kgraf_core::palette::Vision::ALL {
            for p in [
                Palette::dark_with_vision(vision),
                Palette::light_with_vision(vision),
            ] {
                // `danger_bg` (Standard's dark palette keeps the style guide's
                // literal, translucent `rgba(...,.16)` token) and `additive`
                // are themselves translucent; composite each over `base`
                // (`Color32::blend`, egui's own premultiplied-over compositor,
                // as egui draws a filled frame) before measuring text on it.
                // Reading a translucent fill's raw components directly is
                // exactly the bug (`contrast_ratio`'s doc comment).
                let danger_badge = p.base.blend(p.danger_bg);
                assert!(
                    contrast_ratio(p.danger, danger_badge) >= 4.5,
                    "{vision:?} {}: danger badge text {:.2}",
                    if p.dark { "dark" } else { "light" },
                    contrast_ratio(p.danger, danger_badge)
                );
                let card = p.base.blend(p.additive);
                assert!(
                    contrast_ratio(p.danger, card) >= 4.5,
                    "{vision:?} {}: confirm prompt on card {:.2}",
                    if p.dark { "dark" } else { "light" },
                    contrast_ratio(p.danger, card)
                );
                assert!(
                    contrast_ratio(p.danger, p.menu) >= 4.5,
                    "{vision:?} {}: degraded state text on menu {:.2}",
                    if p.dark { "dark" } else { "light" },
                    contrast_ratio(p.danger, p.menu)
                );
                let selection = rgb(p.semantic.selection);
                assert!(
                    contrast_ratio(selection, p.base) >= 3.0,
                    "{vision:?} {}: selection ring on page {:.2}",
                    if p.dark { "dark" } else { "light" },
                    contrast_ratio(selection, p.base)
                );
            }
        }
    }

    /// `safe_tint` is necessary in general, even
    /// though every color this branch currently ships happens to carry
    /// enough contrast margin that a fixed `.16` tint of the *standard*
    /// orange still passes `vision_modes_extended_wcag_pairs` today (a
    /// proposed mutation was re-run against the fixed colors
    /// above and did not reproduce a failure, precisely because this
    /// session's card-contrast fix, done in the same pass, widened every
    /// dark danger's margin; that is reported honestly rather than a false
    /// "confirmed failing" claim). This test instead uses a synthetic,
    /// adversarial `danger` (contrast 4.50 against `--base`, the floor
    /// exactly) that a *fixed* `.16` badge tint of the standard orange
    /// cannot hold 4.5:1 against (measured 3.51), to show the mechanism
    /// itself, independent of which colors this branch ships today.
    #[test]
    fn safe_tint_holds_where_a_fixed_alpha_would_not() {
        let dark_bg = hex(0x0D0F17);
        let standard_orange = hex(0xFB923C);
        let adversarial_danger = Color32::from_rgb(230, 54, 54);
        assert!(
            (contrast_ratio(adversarial_danger, dark_bg) - 4.5).abs() < 0.05,
            "the fixture must sit right at the floor"
        );
        let fixed = blend_over(standard_orange, 0.16, dark_bg);
        assert!(
            contrast_ratio(adversarial_danger, fixed) < 4.5,
            "the fixture: a fixed-alpha tint of a color unrelated to the \
             foreground must be able to fail, or this test proves nothing"
        );
        let derived = safe_tint(adversarial_danger, dark_bg, 0.16, 4.5);
        assert!(
            contrast_ratio(adversarial_danger, derived) >= 4.5,
            "safe_tint must find a badge that holds where a fixed one does not"
        );
    }

    /// A color vision mode chosen in settings applies live: [`Palette::current`]
    /// and [`stage_indicator`] read whatever [`set_vision`] last wrote to the
    /// context, with no restart and no other state to invalidate (decision
    /// 0070).
    #[test]
    fn color_vision_applies_live_with_no_restart() {
        let ctx = egui::Context::default();
        assert_eq!(current_vision(&ctx), kgraf_core::palette::Vision::Standard);
        assert_eq!(
            Palette::current(&ctx).indicator(Indicator::Danger),
            Palette::dark().danger
        );

        apply_appearance(
            &ctx,
            Appearance::Dark,
            kgraf_core::palette::Vision::Protanopia,
        );
        assert_eq!(
            current_vision(&ctx),
            kgraf_core::palette::Vision::Protanopia
        );
        let protan_danger = Palette::current(&ctx).indicator(Indicator::Danger);
        assert_ne!(protan_danger, Palette::dark().danger);
        assert_eq!(
            protan_danger,
            rgb(kgraf_core::palette::Semantic::for_surface(
                kgraf_core::palette::Surface::PageDark,
                kgraf_core::palette::Vision::Protanopia,
            )
            .danger)
        );
        assert_eq!(
            stage_indicator(&ctx, Indicator::Danger),
            rgb(kgraf_core::palette::Semantic::for_surface(
                kgraf_core::palette::Surface::Stage,
                kgraf_core::palette::Vision::Protanopia,
            )
            .danger)
        );

        // Switching back to standard, mid-session, is just another call: no
        // leftover state to reset.
        apply_appearance(
            &ctx,
            Appearance::Dark,
            kgraf_core::palette::Vision::default(),
        );
        assert_eq!(
            Palette::current(&ctx).indicator(Indicator::Danger),
            Palette::dark().danger
        );
    }

    /// TYP-2: every digit has one width in each text weight, because the build
    /// bakes the font's `tnum` glyphs into the character map. With the
    /// proportional digits, "1" is 391 units and "0" 666 in the 400 weight. Only
    /// the digits are baked: `+` and `%` keep their prose widths (531 to 551 and
    /// 786 units against the tabular digits' 600).
    #[test]
    fn digits_are_tabular_in_every_text_weight() {
        let ctx = egui::Context::default();
        ctx.set_fonts(fonts());
        let mut widths = Vec::new();
        let mut signs = Vec::new();
        for _ in 0..2 {
            widths.clear();
            signs.clear();
            let mut out = ctx.run_ui(Default::default(), |ui| {
                for font in [regular(14.0), medium(14.0), bold(14.0)] {
                    let row: Vec<f32> = "0123456789"
                        .chars()
                        .map(|d| {
                            ui.painter()
                                .layout_no_wrap(
                                    d.to_string().repeat(8),
                                    font.clone(),
                                    Color32::WHITE,
                                )
                                .size()
                                .x
                        })
                        .collect();
                    widths.push(row);
                    let w = |t: &str| {
                        ui.painter()
                            .layout_no_wrap(t.repeat(8), font.clone(), Color32::WHITE)
                            .size()
                            .x
                    };
                    signs.push((w("0"), w("+"), w("%")));
                }
            });
            out.textures_delta.clear();
        }
        for row in &widths {
            let first = row[0];
            assert!(first > 0.0);
            for w in row {
                assert!((w - first).abs() < 0.01, "digit widths differ: {row:?}");
            }
        }
        for (digit, plus, percent) in &signs {
            assert!(
                (plus - digit).abs() > 0.5 && (percent - digit).abs() > 0.5,
                "a sign took the tabular width: {signs:?}"
            );
        }
    }

    #[test]
    fn the_secondary_column_clamps_between_360_and_402() {
        assert_eq!(size::secondary_w(1000.0), 360.0);
        assert_eq!(size::secondary_w(1400.0), 392.0);
        assert_eq!(size::secondary_w(2000.0), 402.0);
    }

    /// Each embedded font is the one that draws latin text: placed before egui's
    /// defaults, it changes the width of a line. A font egui could not read would
    /// leave the defaults drawing it, in silence.
    #[test]
    fn the_embedded_fonts_draw_the_text() {
        let width = |first: Option<&str>| {
            let mut defs = fonts();
            let defaults = FontDefinitions::default();
            let mut chain = defaults
                .families
                .get(&FontFamily::Proportional)
                .cloned()
                .unwrap_or_default();
            if let Some(name) = first {
                chain.insert(0, name.to_owned());
            }
            defs.families.insert(FontFamily::Proportional, chain);
            let ctx = egui::Context::default();
            ctx.set_fonts(defs);
            let mut w = 0.0;
            // The definitions apply from the next pass.
            for _ in 0..2 {
                let mut out = ctx.run_ui(Default::default(), |ui| {
                    w = ui
                        .painter()
                        .layout_no_wrap(
                            "The quick brown fox jumps over the lazy dog, 0123456789 WMmi".into(),
                            regular(14.0),
                            Color32::WHITE,
                        )
                        .size()
                        .x;
                });
                out.textures_delta.clear();
            }
            w
        };
        let default = width(None);
        let widths: Vec<(&str, f32)> = [family::FONT, family::MEDIUM, family::BOLD, family::MONO]
            .into_iter()
            .map(|name| (name, width(Some(name))))
            .collect();
        for (name, w) in &widths {
            assert!(
                (w - default).abs() > 0.5,
                "{name}: {w} against the default {default}; all: {widths:?}"
            );
        }
    }
}
