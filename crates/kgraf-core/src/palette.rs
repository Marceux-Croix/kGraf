//! The five semantic colors of kGraf's interface, in one place both the desktop
//! shell and the galaxy renderer can read (`docs/STYLE_GUIDE.md` section 3, as
//! extended by decision 0070: "there should be a color blind mode in
//! the settings for the various types of color blindness. this isn't a style
//! guide problem - this should live only within kgraf").
//!
//! Five meanings carry color and nothing else does:
//!
//! - **progress** violet (the accent), also live;
//! - **danger** orange: broken, failed, dangerous, degraded;
//! - **needs input** yellow (a failure that needs the user counts here in any
//!   indicator that sums up several items);
//! - **selection** cyan: a selection or a match (on the page also the primary
//!   action and "new since you looked");
//! - **neutral** gray: everything else, including working and waiting.
//!
//! A [`Semantic`] is chosen at run time from the theme and a [`Vision`] mode.
//! [`Vision`] has one variant for the standard palette and one for each of the
//! four color vision deficiencies: protanopia,
//! deuteranopia, tritanopia (the three dichromacies a simulation can model with
//! a linear matrix) and achromatopsia (total color blindness, luminance only).
//! Values are plain sRGB bytes: this crate knows no toolkit.
//!
//! **How the four vision palettes were chosen.** Each mode's five colors are
//! picked so that, after running them through a simulation of that deficiency,
//! they stay perceptually distinct (module [`tests::cvd`] has the simulation
//! and the distance metric, `cargo test -p kgraf-core` reruns the check). The
//! simulation is the Machado, Oliveira and Fernandes (2009) linear model at
//! full severity (1.0), applied in linearized sRGB (the paper gives the
//! general, per-severity form; the matrices below are its severity-1.0 case).
//! Achromatopsia has no matrix in that model (it is not a dichromacy); it is
//! simulated as relative luminance only, using the same Rec. 709 coefficients
//! this project's WCAG contrast math already uses (`contrast.py`). Distance is
//! measured with CIEDE2000 (Sharma, Wu and Dalal, 2005) on the simulated
//! color, computed here rather than pulled from a crate: the formula is small,
//! the numbers must be exact and auditable, and a hand search of crates.io
//! (`palette`, `perceive-cvd`, `colorverse`, `cvd-palette`) found nothing that
//! exposed the Machado 2009 matrices at a pinned, cited severity rather than a
//! newer or different model; `tests::cvd::matches_the_published_reference_pairs`
//! runs this formula against all 34 of Sharma et al.'s published reference
//! pairs so it is checked against known answers, not just against itself.
//!
//! **What is and is not verified against another implementation.** An
//! A cross-check re-derived the Machado matrices from the `colour-science`
//! Python package (`colour.blindness.matrix_cvd_Machado2009`) and confirmed
//! they match the three matrices below, and ran this module's own
//! `tests::cvd::simulate` and `ciede2000` against `colour`'s outputs across
//! every color this file uses; that check is not itself part of this crate
//! (it depends on a package this workspace does not take as a dependency), so
//! it is not repeated here as a test, and it is not the same claim as "this is
//! the model Chromium, Coblis or sim-daltonism use" (a claim removed from an
//! earlier draft of this file for want of a citable source; the Machado 2009
//! model is used, and cited, on its own terms).
//!
//! Each chromatic mode (protanopia, deuteranopia, tritanopia) keeps a measured
//! simulated CIEDE2000 of at least 20 between every pair of its five meanings,
//! on every surface (`tests::vision_modes_stay_distinct_under_simulation`), and
//! at least 20 from the page's own body text and secondary text colors too
//! (`--text` and `--text-2`; `tests::vision_modes_stay_distinct_from_text`),
//! for every meaning except `neutral`. `neutral` is not checked against text:
//! COL-5 already defines `neutral` as carrying the same weight as `--text-2`
//! (in the standard palette they are the same value), so a color-blind mode
//! keeping that pairing is the rule working as designed, not a defect.
//!
//! Achromatopsia cannot reach the chromatic modes' bars: with only one channel
//! of information (luminance), a WCAG AA contrast floor to hold against three
//! different surfaces (a page's `--base`, a page's `--menu` for `danger`'s
//! "degraded state text on menu" pairing, and the stage), and `--text`/
//! `--text-2` to stay clear of too, there is very little room left. Measured
//! minimums: mutual (the five meanings against each other) 9.69 (dark and
//! stage) and 9.91 (light); against `--text`/`--text-2` (excluding `neutral`,
//! for the reason above) 2.80 (dark and stage) and 3.74 (light). Achromatopsia
//! is exactly why the style guide already requires that color never be the
//! only carrier of a meaning (COL-9): that rule stops being optional here and
//! starts being the only thing that works. Luminance still carries a
//! five-way *ranking* comparable to what a sighted user gets from hue (five
//! distinct steps, ordered by how far each stands out from its background:
//! `progress` least, since it alone only needs 3:1 and only ever draws a dot,
//! a bar or a scrubber knob; `neutral`, `danger` and `selection` next; `needs
//! input` most, since it also carries the sum of several failed items); the
//! word next to the indicator, the icon on it, and its shape (pill, dot,
//! badge) carry which of the five it is. No accessibility pass has yet gone
//! through every indicator in the shell asking "does this still read with hue
//! removed and only a word or icon left"; that pass is not part of this
//! change (decision 0070, "Open").
//!
//! Every color keeps WCAG 2.2 AA contrast against its surface's background
//! (`tests::vision_modes_meet_wcag_aa`, the same math as
//! the style guide's contrast script): 4.5:1 on a
//! page (where these colors also serve as text and icons, e.g.
//! [`Palette::danger`] in a `colored_label`) and 3:1 on the stage (where they
//! mark non-text elements: dots, rings, pills), except `progress`, which only
//! ever draws a dot, a bar or a scrubber knob and so needs 3:1 everywhere,
//! matching the style guide's own "Live dot and scrubber knob (accent)" row.
//! `kgraf-shell`'s own tests (`theme::tests::vision_modes_extended_wcag_pairs`)
//! cover the composited surfaces that only exist there: a badge fill derived
//! from a remapped `danger` (H1: a fixed-alpha badge behind a remapped
//! foreground cannot be assumed to still pass, and is now solved for, not
//! assumed, in `kgraf-shell::theme::safe_tint`), a card, and a menu.
//!
//! `progress_end` (the progress gradient's far end) is checked too
//! (`tests::vision_modes_progress_end_is_usable`): it keeps 3:1 contrast on
//! its surface (`progress` itself only ever needs 3:1) and stays clear of
//! `neutral` at the mode's own bar. It is not required to clear the other
//! three meanings: it only ever appears as the far end of the gradient that
//! starts at `progress`, immediately next to `progress`, never alone, so
//! confusing it with `danger` or `selection` at a glance is not the failure
//! mode this guards against; confusing it with `neutral`, the "nothing to see
//! here" color, is.
//!
//! A color-blind mode is one more [`Vision`] variant and one more table here;
//! no reader changes. Values are plain sRGB bytes: this crate knows no
//! toolkit.

/// An sRGB color, 0 to 255 per channel.
pub type Rgb = [u8; 3];

/// The meanings a color may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Meaning {
    Progress,
    Danger,
    NeedsInput,
    Selection,
    Neutral,
}

impl Meaning {
    pub const ALL: [Meaning; 5] = [
        Meaning::Progress,
        Meaning::Danger,
        Meaning::NeedsInput,
        Meaning::Selection,
        Meaning::Neutral,
    ];
}

/// How colors are chosen for the viewer's color vision (decision 0070). The three dichromacies and
/// achromatopsia join the standard palette; see the module docs for how each was chosen. Serializes
/// in `snake_case` so it can sit in the shell's settings file next to `Appearance` (kgraf-shell's
/// `settings.rs`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Vision {
    #[default]
    Standard,
    /// Red-blind (no L cones). Full severity.
    Protanopia,
    /// Green-blind (no M cones). Full severity.
    Deuteranopia,
    /// Blue-yellow-blind (no S cones). Full severity.
    Tritanopia,
    /// Total color blindness (no cones; rods only). Luminance only.
    Achromatopsia,
}

impl Vision {
    pub const ALL: [Vision; 5] = [
        Vision::Standard,
        Vision::Protanopia,
        Vision::Deuteranopia,
        Vision::Tritanopia,
        Vision::Achromatopsia,
    ];

    /// A short label for a settings list.
    pub fn label(self) -> &'static str {
        match self {
            Vision::Standard => "Standard",
            Vision::Protanopia => "Protanopia (red-blind)",
            Vision::Deuteranopia => "Deuteranopia (green-blind)",
            Vision::Tritanopia => "Tritanopia (blue-yellow-blind)",
            Vision::Achromatopsia => "Achromatopsia (color-blind)",
        }
    }
}

/// Where a color is drawn: the page takes the theme; the stage is black in both
/// themes, so it has its own values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    PageDark,
    PageLight,
    Stage,
}

/// The five semantic colors for one surface and vision mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Semantic {
    pub progress: Rgb,
    /// The progress gradient's far end (`--progress`: `progress` to 70%, then this).
    pub progress_end: Rgb,
    pub danger: Rgb,
    pub needs_input: Rgb,
    pub selection: Rgb,
    pub neutral: Rgb,
}

impl Semantic {
    /// The palette for `surface` under `vision`.
    ///
    /// Style guide 1.8 ("Nocturne") values, tables 3.2 to 3.4, for
    /// [`Vision::Standard`]: progress #8B5CF6 to #C084FC; `--danger` dark
    /// #FB923C, light #B83C0B, `--stage-danger` #FF7A33; `--needs-input` dark
    /// and stage #FACC15, light #A16207; `--primary` dark and stage #22D3EE,
    /// light #0E7490; gray `--text-2` dark and stage #9AA0B4 (COL-5), light
    /// #5A6072. The other four modes are this crate's own values (module
    /// docs): kGraf's color-blind modes are not part of the style guide and
    /// do not change its one palette.
    pub fn for_surface(surface: Surface, vision: Vision) -> Self {
        match (vision, surface) {
            (Vision::Standard, Surface::PageDark) => Self {
                progress: [0x8B, 0x5C, 0xF6],
                progress_end: [0xC0, 0x84, 0xFC],
                danger: [0xFB, 0x92, 0x3C],
                needs_input: [0xFA, 0xCC, 0x15],
                selection: [0x22, 0xD3, 0xEE],
                neutral: [0x9A, 0xA0, 0xB4],
            },
            (Vision::Standard, Surface::PageLight) => Self {
                progress: [0x8B, 0x5C, 0xF6],
                progress_end: [0xC0, 0x84, 0xFC],
                danger: [0xB8, 0x3C, 0x0B],
                needs_input: [0xA1, 0x62, 0x07],
                selection: [0x0E, 0x74, 0x90],
                neutral: [0x5A, 0x60, 0x72],
            },
            (Vision::Standard, Surface::Stage) => Self {
                progress: [0x8B, 0x5C, 0xF6],
                progress_end: [0xC0, 0x84, 0xFC],
                danger: [0xFF, 0x7A, 0x33],
                needs_input: [0xFA, 0xCC, 0x15],
                selection: [0x22, 0xD3, 0xEE],
                neutral: [0x9A, 0xA0, 0xB4],
            },

            // Protanopia: min simulated CIEDE2000 21.21 (dark/stage) and 23.09
            // (light) between any two meanings; at least 20 from `--text` and
            // `--text-2` too (module docs); `danger` is at least 4.5:1 against
            // `--menu` and against a card (`--base` blended with `--additive`)
            // as well as `--base` (`--danger-bg` and a
            // confirm prompt on a card both composite over `--base`, which
            // needs measuring at the true composited color, not a raw
            // translucent one; card is the tightest of the three on the dark
            // palettes, since `--additive` there lightens `--base` more than
            // `--menu` does).
            (Vision::Protanopia, Surface::PageDark) => Self {
                progress: [0xAA, 0x41, 0xF9],
                progress_end: [0xE5, 0x5A, 0xFA],
                danger: [0xF7, 0x54, 0x66],
                needs_input: [0xDC, 0xF5, 0x36],
                selection: [0x32, 0xCF, 0x9F],
                neutral: [0xAA, 0xB1, 0xBF],
            },
            (Vision::Protanopia, Surface::Stage) => Self {
                progress: [0xAA, 0x41, 0xF9],
                progress_end: [0xE5, 0x5A, 0xFA],
                danger: [0xF7, 0x54, 0x66],
                needs_input: [0xDC, 0xF5, 0x36],
                selection: [0x32, 0xCF, 0x9F],
                neutral: [0xAA, 0xB1, 0xBF],
            },
            (Vision::Protanopia, Surface::PageLight) => Self {
                progress: [0xFC, 0x03, 0xF3],
                progress_end: [0xA8, 0x03, 0xE3],
                danger: [0x85, 0x00, 0x01],
                needs_input: [0x6F, 0x73, 0x24],
                selection: [0x20, 0x0F, 0x84],
                neutral: [0x79, 0x6E, 0x78],
            },

            // Deuteranopia: min simulated CIEDE2000 20.68 (dark/stage) and
            // 22.05 (light); at least 20 from text (same exceptions and menu
            // pairing as protanopia, above).
            (Vision::Deuteranopia, Surface::PageDark) => Self {
                progress: [0x8B, 0x54, 0xF6],
                progress_end: [0x72, 0x6C, 0xF7],
                danger: [0xEC, 0x86, 0x4A],
                needs_input: [0xF8, 0xFC, 0xBF],
                selection: [0x2E, 0x8C, 0x63],
                neutral: [0xAC, 0xA1, 0xBB],
            },
            (Vision::Deuteranopia, Surface::Stage) => Self {
                progress: [0x8B, 0x54, 0xF6],
                progress_end: [0x72, 0x6C, 0xF7],
                danger: [0xEC, 0x86, 0x4A],
                needs_input: [0xF8, 0xFC, 0xBF],
                selection: [0x2E, 0x8C, 0x63],
                neutral: [0xAC, 0xA1, 0xBB],
            },
            (Vision::Deuteranopia, Surface::PageLight) => Self {
                progress: [0xD2, 0x5C, 0xD8],
                progress_end: [0xA2, 0x47, 0xD3],
                danger: [0x4F, 0x13, 0x06],
                needs_input: [0x5B, 0x74, 0x1B],
                selection: [0x16, 0x06, 0xA7],
                neutral: [0x70, 0x70, 0x74],
            },

            // Tritanopia: min simulated CIEDE2000 24.01 (dark/stage) and 20.89
            // (light); at least 20 from text (same exceptions above); `danger`
            // at least 4.5:1 against `--menu` and a card, not only `--base`
            // (same fix as protanopia, above).
            (Vision::Tritanopia, Surface::PageDark) => Self {
                progress: [0xA4, 0x58, 0xE4],
                progress_end: [0xD4, 0x6E, 0xE8],
                danger: [0xFD, 0x5B, 0x2B],
                needs_input: [0xFB, 0xC9, 0x28],
                selection: [0x36, 0xF1, 0xA6],
                neutral: [0x9B, 0xA4, 0x91],
            },
            (Vision::Tritanopia, Surface::Stage) => Self {
                progress: [0xA4, 0x58, 0xE4],
                progress_end: [0xD4, 0x6E, 0xE8],
                danger: [0xFD, 0x5B, 0x2B],
                needs_input: [0xFB, 0xC9, 0x28],
                selection: [0x36, 0xF1, 0xA6],
                neutral: [0x9B, 0xA4, 0x91],
            },
            (Vision::Tritanopia, Surface::PageLight) => Self {
                progress: [0xE3, 0x51, 0xC6],
                progress_end: [0xDF, 0x3B, 0x8D],
                danger: [0xA1, 0x3D, 0x0A],
                needs_input: [0x49, 0x36, 0x0E],
                selection: [0x95, 0x1F, 0xD2],
                neutral: [0x3E, 0x48, 0x42],
            },

            // Achromatopsia: min simulated (luminance-only) CIEDE2000 9.69
            // (dark/stage) and 9.91 (light) between the five meanings; 2.80
            // (dark/stage) and 3.74 (light) from text and text-2 (`neutral`
            // excepted, module docs). `danger` is at least 4.5:1 against
            // `--menu` as well as `--base`.
            (Vision::Achromatopsia, Surface::PageDark) => Self {
                progress: [0xB3, 0x14, 0x92],
                progress_end: [0xEC, 0x55, 0xEB],
                danger: [0xEC, 0x75, 0x4C],
                needs_input: [0xD7, 0xEB, 0x97],
                selection: [0x3F, 0xC9, 0xD9],
                neutral: [0x7D, 0x7B, 0x74],
            },
            (Vision::Achromatopsia, Surface::Stage) => Self {
                progress: [0xB3, 0x14, 0x92],
                progress_end: [0xEC, 0x55, 0xEB],
                danger: [0xEC, 0x75, 0x4C],
                needs_input: [0xD7, 0xEB, 0x97],
                selection: [0x3F, 0xC9, 0xD9],
                neutral: [0x7D, 0x7B, 0x74],
            },
            (Vision::Achromatopsia, Surface::PageLight) => Self {
                progress: [0x9D, 0x7A, 0xD9],
                progress_end: [0xAF, 0x66, 0xD3],
                danger: [0xB6, 0x47, 0x0F],
                needs_input: [0x32, 0x28, 0x03],
                selection: [0x10, 0x50, 0x59],
                neutral: [0x02, 0x02, 0x02],
            },
        }
    }

    /// The color of a meaning.
    pub fn color(&self, m: Meaning) -> Rgb {
        match m {
            Meaning::Progress => self.progress,
            Meaning::Danger => self.danger,
            Meaning::NeedsInput => self.needs_input,
            Meaning::Selection => self.selection,
            Meaning::Neutral => self.neutral,
        }
    }
}

/// A color the style guide writes as `rgba(r,g,b,a)`: sRGB bytes and an alpha in 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub rgb: Rgb,
    pub alpha: f32,
}

/// The stage's literal colors, which carry no meaning (style guide 1.8, section 3.4,
/// and the STG rules). One source for the shell's stage chrome and the galaxy's
/// renderer: neither restates a value. "White at N%" is stored as its alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageLiterals {
    /// The background, in both themes.
    pub background: Rgb,
    /// A node at rest.
    pub node: Rgba,
    /// A node lit as the selected node's neighbor.
    pub neighbor: Rgba,
    /// An edge: white at this alpha (at camera scale 1 and below).
    pub edge_white: f32,
    /// A task card's fill (STG-DRAW-5).
    pub card: Rgb,
    /// A task card's border: white at this alpha.
    pub card_border_white: f32,
    /// An actor disk's fill (STG-DRAW-7).
    pub actor: Rgb,
    /// The relation (waits-on) arrow.
    pub relation: Rgb,
    /// A label: white at this alpha.
    pub label_white: f32,
    /// A group label: white at this alpha.
    pub group_label_white: f32,
    /// The text halo: black at this alpha.
    pub halo_black: f32,
    /// The dark icon on a stage danger pill (STG-DRAW-11).
    pub pill_icon: Rgb,
    /// An actor dot on the minimap and the "off" switch thumb (STG-8, CMP-SET-4).
    pub light: Rgb,
}

/// The stage literals of style guide 1.8 (section 3.4).
pub const STAGE: StageLiterals = StageLiterals {
    background: [0x07, 0x08, 0x0F],
    node: Rgba {
        rgb: [160, 166, 186],
        alpha: 0.58,
    },
    neighbor: Rgba {
        rgb: [225, 229, 240],
        alpha: 0.9,
    },
    edge_white: 0.05,
    card: [0x17, 0x1A, 0x26],
    card_border_white: 0.34,
    actor: [0x1E, 0x22, 0x33],
    relation: [0xC3, 0xC8, 0xD8],
    label_white: 0.88,
    group_label_white: 0.55,
    halo_black: 0.85,
    pill_icon: [0x0D, 0x0F, 0x17],
    light: [0xED, 0xEF, 0xF7],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Section 3.4, stated literally so an edit to `STAGE` fails here.
    #[test]
    fn the_stage_literals_are_section_3_4() {
        assert_eq!(STAGE.background, [0x07, 0x08, 0x0F]);
        assert_eq!((STAGE.node.rgb, STAGE.node.alpha), ([160, 166, 186], 0.58));
        assert_eq!(
            (STAGE.neighbor.rgb, STAGE.neighbor.alpha),
            ([225, 229, 240], 0.9)
        );
        assert_eq!(
            (STAGE.card, STAGE.actor),
            ([0x17, 0x1A, 0x26], [0x1E, 0x22, 0x33])
        );
        assert_eq!(STAGE.relation, [0xC3, 0xC8, 0xD8]);
        assert_eq!(
            (
                STAGE.edge_white,
                STAGE.card_border_white,
                STAGE.label_white,
                STAGE.group_label_white,
                STAGE.halo_black
            ),
            (0.05, 0.34, 0.88, 0.55, 0.85)
        );
    }

    /// On every surface and every vision mode, the five meanings are five
    /// different raw colors (a weaker check than the simulated distance
    /// tests below, but one that also catches a copy-paste of the same hex
    /// into two fields).
    #[test]
    fn five_meanings_five_colors_on_every_surface_and_vision() {
        for v in Vision::ALL {
            for s in [Surface::PageDark, Surface::PageLight, Surface::Stage] {
                let p = Semantic::for_surface(s, v);
                let all = Meaning::ALL.map(|m| p.color(m));
                for (i, a) in all.iter().enumerate() {
                    for b in &all[i + 1..] {
                        assert_ne!(a, b, "{v:?} {s:?}");
                    }
                }
            }
        }
    }

    /// Color vision deficiency simulation and CIEDE2000, self-contained so the
    /// numbers this module's docs cite are checked here, not assumed. See the
    /// module docs at the top of `palette.rs` for what is cited from where.
    pub mod cvd {
        use super::Rgb;

        /// Machado, Oliveira and Fernandes, "A Physiologically-based Model
        /// for Simulation of Color Vision Deficiency", IEEE TVCG 15(6), 2009,
        /// Table 1, severity 1.0 (full dichromacy), applied to linearized
        /// sRGB. Cross-checked (second implementation) against
        /// `colour.blindness.matrix_cvd_Machado2009` from the `colour-science`
        /// Python package, which reproduces the same three matrices; that
        /// package is not a dependency of this crate, so the check is design
        /// evidence, not a test here.
        const PROTAN: [[f64; 3]; 3] = [
            [0.152_286, 1.052_583, -0.204_868],
            [0.114_503, 0.786_281, 0.099_216],
            [-0.003_882, -0.048_116, 1.051_998],
        ];
        const DEUTAN: [[f64; 3]; 3] = [
            [0.367_322, 0.860_646, -0.227_968],
            [0.280_085, 0.672_501, 0.047_413],
            [-0.011_820, 0.042_940, 0.968_881],
        ];
        const TRITAN: [[f64; 3]; 3] = [
            [1.255_528, -0.076_749, -0.178_779],
            [-0.078_411, 0.930_809, 0.147_602],
            [0.004_733, 0.691_367, 0.303_900],
        ];

        /// sRGB channel (0 to 255) to linear light (0 to 1), IEC 61966-2-1.
        fn to_linear(c: u8) -> f64 {
            let c = f64::from(c) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }

        /// Linear light (0 to 1, clamped) to an sRGB channel.
        fn to_srgb(c: f64) -> u8 {
            let c = c.clamp(0.0, 1.0);
            let v = if c <= 0.003_130_8 {
                c * 12.92
            } else {
                1.055 * c.powf(1.0 / 2.4) - 0.055
            };
            (v * 255.0).round().clamp(0.0, 255.0) as u8
        }

        /// The kind of vision deficiency to simulate. There is no `Standard`:
        /// simulating "no deficiency" is the identity, which the caller
        /// never needs to ask for.
        #[derive(Debug, Clone, Copy)]
        pub enum Deficiency {
            Protan,
            Deutan,
            Tritan,
            /// Total color blindness: relative luminance only (Rec. 709
            /// coefficients, the same ones `contrast_ratio` below and
            /// the style guide's contrast script
            /// use), not a Machado matrix (achromatopsia is not a
            /// dichromacy, so that model does not apply to it).
            Achroma,
        }

        /// `rgb` as it would appear to someone with `deficiency`, at full
        /// severity.
        pub fn simulate(rgb: Rgb, deficiency: Deficiency) -> Rgb {
            let lin = rgb.map(to_linear);
            if let Deficiency::Achroma = deficiency {
                let y = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
                let g = to_srgb(y);
                return [g, g, g];
            }
            let m = match deficiency {
                Deficiency::Protan => &PROTAN,
                Deficiency::Deutan => &DEUTAN,
                Deficiency::Tritan => &TRITAN,
                Deficiency::Achroma => unreachable!("handled above"),
            };
            let mut out = [0u8; 3];
            for (row, o) in m.iter().zip(out.iter_mut()) {
                let v = row[0] * lin[0] + row[1] * lin[1] + row[2] * lin[2];
                *o = to_srgb(v);
            }
            out
        }

        /// CIE L*a*b*, D65 white point, from sRGB (ITU-R BT.709 primaries).
        pub fn rgb_to_lab(rgb: Rgb) -> (f64, f64, f64) {
            let lin = rgb.map(to_linear);
            let (r, g, b) = (lin[0], lin[1], lin[2]);
            let x = 0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b;
            let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175_0 * b;
            let z = 0.019_333_9 * r + 0.119_192_0 * g + 0.950_304_1 * b;
            const XN: f64 = 0.950_47;
            const YN: f64 = 1.0;
            const ZN: f64 = 1.088_83;
            fn f(t: f64) -> f64 {
                const D: f64 = 6.0 / 29.0;
                if t > D * D * D {
                    t.cbrt()
                } else {
                    t / (3.0 * D * D) + 4.0 / 29.0
                }
            }
            let (fx, fy, fz) = (f(x / XN), f(y / YN), f(z / ZN));
            let l = 116.0 * fy - 16.0;
            let a = 500.0 * (fx - fy);
            let bb = 200.0 * (fy - fz);
            (l, a, bb)
        }

        /// CIEDE2000 (Sharma, Wu and Dalal, 2005), `kL = kC = kH = 1`.
        /// `tests::cvd::matches_the_published_reference_pairs` checks this
        /// against the paper's own worked examples.
        pub fn ciede2000(lab1: (f64, f64, f64), lab2: (f64, f64, f64)) -> f64 {
            let (l1, a1, b1) = lab1;
            let (l2, a2, b2) = lab2;
            let c1 = a1.hypot(b1);
            let c2 = a2.hypot(b2);
            let c_bar = (c1 + c2) / 2.0;
            let g = 0.5 * (1.0 - (c_bar.powi(7) / (c_bar.powi(7) + 25f64.powi(7))).sqrt());
            let a1p = a1 * (1.0 + g);
            let a2p = a2 * (1.0 + g);
            let c1p = a1p.hypot(b1);
            let c2p = a2p.hypot(b2);
            let hp = |ap: f64, b: f64| -> f64 {
                if ap == 0.0 && b == 0.0 {
                    0.0
                } else {
                    let h = b.atan2(ap).to_degrees();
                    if h < 0.0 { h + 360.0 } else { h }
                }
            };
            let h1p = hp(a1p, b1);
            let h2p = hp(a2p, b2);
            let dlp = l2 - l1;
            let dcp = c2p - c1p;
            let dhp = if c1p * c2p == 0.0 {
                0.0
            } else {
                let mut dh = h2p - h1p;
                if dh > 180.0 {
                    dh -= 360.0;
                } else if dh < -180.0 {
                    dh += 360.0;
                }
                dh
            };
            let d_hp_big = 2.0 * (c1p * c2p).sqrt() * (dhp.to_radians() / 2.0).sin();
            let l_bar_p = (l1 + l2) / 2.0;
            let c_bar_p = (c1p + c2p) / 2.0;
            let h_bar_p = if c1p * c2p == 0.0 {
                h1p + h2p
            } else {
                let dh = (h1p - h2p).abs();
                if dh <= 180.0 {
                    (h1p + h2p) / 2.0
                } else if h1p + h2p < 360.0 {
                    (h1p + h2p + 360.0) / 2.0
                } else {
                    (h1p + h2p - 360.0) / 2.0
                }
            };
            let t = 1.0 - 0.17 * (h_bar_p - 30.0).to_radians().cos()
                + 0.24 * (2.0 * h_bar_p).to_radians().cos()
                + 0.32 * (3.0 * h_bar_p + 6.0).to_radians().cos()
                - 0.20 * (4.0 * h_bar_p - 63.0).to_radians().cos();
            let d_theta = 30.0 * (-(((h_bar_p - 275.0) / 25.0).powi(2))).exp();
            let rc = 2.0 * (c_bar_p.powi(7) / (c_bar_p.powi(7) + 25f64.powi(7))).sqrt();
            let sl =
                1.0 + (0.015 * (l_bar_p - 50.0).powi(2)) / (20.0 + (l_bar_p - 50.0).powi(2)).sqrt();
            let sc = 1.0 + 0.045 * c_bar_p;
            let sh = 1.0 + 0.015 * c_bar_p * t;
            let rt = -(2.0 * d_theta.to_radians()).sin() * rc;
            let dl = dlp / sl;
            let dc = dcp / sc;
            let dh = d_hp_big / sh;
            (dl * dl + dc * dc + dh * dh + rt * dc * dh).sqrt()
        }

        /// The smallest CIEDE2000 between any two of `colors`, after
        /// simulating each for `deficiency`.
        pub fn min_simulated_distance(colors: &[Rgb], deficiency: Deficiency) -> f64 {
            let labs: Vec<_> = colors
                .iter()
                .map(|c| rgb_to_lab(simulate(*c, deficiency)))
                .collect();
            let mut min = f64::MAX;
            for i in 0..labs.len() {
                for j in (i + 1)..labs.len() {
                    min = min.min(ciede2000(labs[i], labs[j]));
                }
            }
            min
        }

        /// WCAG 2.2 relative luminance (Rec. 709 coefficients on linear
        /// light), the same formula
        /// the style guide's contrast script uses.
        fn relative_luminance(rgb: Rgb) -> f64 {
            let lin = rgb.map(to_linear);
            0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2]
        }

        /// WCAG 2.2 contrast ratio between two opaque sRGB colors.
        pub fn contrast_ratio(fg: Rgb, bg: Rgb) -> f64 {
            let (a, b) = (relative_luminance(fg), relative_luminance(bg));
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        }

        #[test]
        fn matches_the_published_reference_pairs() {
            // Sharma, Wu and Dalal (2005), "The CIEDE2000 Color-Difference
            // Formula: Implementation Notes, Supplementary Test Data, and
            // Mathematical Observations", Table 1: all 34 published reference
            // pairs (the table was re-supplied from a primary source; the
            // draft this replaced hand-transcribed 9 of the 34 from memory
            // and got 2 of those 9 wrong, since corrected here against a
            // primary transcription of the table).
            type Lab = (f64, f64, f64);
            let cases: &[(Lab, Lab, f64)] = &[
                (
                    (50.0000, 2.6772, -79.7751),
                    (50.0000, 0.0000, -82.7485),
                    2.0425,
                ),
                (
                    (50.0000, 3.1571, -77.2803),
                    (50.0000, 0.0000, -82.7485),
                    2.8615,
                ),
                (
                    (50.0000, 2.8361, -74.0200),
                    (50.0000, 0.0000, -82.7485),
                    3.4412,
                ),
                (
                    (50.0000, -1.3802, -84.2814),
                    (50.0000, 0.0000, -82.7485),
                    1.0000,
                ),
                (
                    (50.0000, -1.1848, -84.8006),
                    (50.0000, 0.0000, -82.7485),
                    1.0000,
                ),
                (
                    (50.0000, -0.9009, -85.5211),
                    (50.0000, 0.0000, -82.7485),
                    1.0000,
                ),
                (
                    (50.0000, 0.0000, 0.0000),
                    (50.0000, -1.0000, 2.0000),
                    2.3669,
                ),
                (
                    (50.0000, -1.0000, 2.0000),
                    (50.0000, 0.0000, 0.0000),
                    2.3669,
                ),
                (
                    (50.0000, 2.4900, -0.0010),
                    (50.0000, -2.4900, 0.0009),
                    7.1792,
                ),
                (
                    (50.0000, 2.4900, -0.0010),
                    (50.0000, -2.4900, 0.0010),
                    7.1792,
                ),
                (
                    (50.0000, 2.4900, -0.0010),
                    (50.0000, -2.4900, 0.0011),
                    7.2195,
                ),
                (
                    (50.0000, 2.4900, -0.0010),
                    (50.0000, -2.4900, 0.0012),
                    7.2195,
                ),
                (
                    (50.0000, -0.0010, 2.4900),
                    (50.0000, 0.0009, -2.4900),
                    4.8045,
                ),
                (
                    (50.0000, -0.0010, 2.4900),
                    (50.0000, 0.0010, -2.4900),
                    4.8045,
                ),
                (
                    (50.0000, -0.0010, 2.4900),
                    (50.0000, 0.0011, -2.4900),
                    4.7461,
                ),
                (
                    (50.0000, 2.5000, 0.0000),
                    (50.0000, 0.0000, -2.5000),
                    4.3065,
                ),
                (
                    (50.0000, 2.5000, 0.0000),
                    (73.0000, 25.0000, -18.0000),
                    27.1492,
                ),
                (
                    (50.0000, 2.5000, 0.0000),
                    (61.0000, -5.0000, 29.0000),
                    22.8977,
                ),
                (
                    (50.0000, 2.5000, 0.0000),
                    (56.0000, -27.0000, -3.0000),
                    31.9030,
                ),
                (
                    (50.0000, 2.5000, 0.0000),
                    (58.0000, 24.0000, 15.0000),
                    19.4535,
                ),
                ((50.0000, 2.5000, 0.0000), (50.0000, 3.1736, 0.5854), 1.0000),
                ((50.0000, 2.5000, 0.0000), (50.0000, 3.2972, 0.0000), 1.0000),
                ((50.0000, 2.5000, 0.0000), (50.0000, 1.8634, 0.5757), 1.0000),
                ((50.0000, 2.5000, 0.0000), (50.0000, 3.2592, 0.3350), 1.0000),
                (
                    (60.2574, -34.0099, 36.2677),
                    (60.4626, -34.1751, 39.4387),
                    1.2644,
                ),
                (
                    (63.0109, -31.0961, -5.8663),
                    (62.8187, -29.7946, -4.0864),
                    1.2630,
                ),
                (
                    (61.2901, 3.7196, -5.3901),
                    (61.4292, 2.2480, -4.9620),
                    1.8731,
                ),
                (
                    (35.0831, -44.1164, 3.7933),
                    (35.0232, -40.0716, 1.5901),
                    1.8645,
                ),
                (
                    (22.7233, 20.0904, -46.6940),
                    (23.0331, 14.9730, -42.5619),
                    2.0373,
                ),
                (
                    (36.4612, 47.8580, 18.3852),
                    (36.2715, 50.5065, 21.2231),
                    1.4146,
                ),
                (
                    (90.8027, -2.0831, 1.4410),
                    (91.1528, -1.6435, 0.0447),
                    1.4441,
                ),
                (
                    (90.9257, -0.5406, -0.9208),
                    (88.6381, -0.8985, -0.7239),
                    1.5381,
                ),
                (
                    (6.7747, -0.2908, -2.4247),
                    (5.8714, -0.0985, -2.2286),
                    0.6377,
                ),
                (
                    (2.0776, 0.0795, -1.1350),
                    (0.9033, -0.0636, -0.5514),
                    0.9082,
                ),
            ];
            for (lab1, lab2, expected) in cases {
                let got = ciede2000(*lab1, *lab2);
                let rev = ciede2000(*lab2, *lab1);
                assert!(
                    (got - expected).abs() < 5e-5 && (rev - expected).abs() < 5e-5,
                    "{lab1:?} vs {lab2:?}: got {got} (reversed {rev}), want {expected}"
                );
            }
        }
    }

    use cvd::Deficiency;

    /// The page's body and secondary text colors, standard vision (they do
    /// not change with `Vision`), by surface: `--text` and `--text-2` (style
    /// guide 3.2). `Standard` mode's `neutral` and `text-2` are the same
    /// value by design (COL-5), which is why `neutral` is excluded from
    /// [`vision_modes_stay_distinct_from_text`].
    fn text_colors(surface: Surface) -> Option<(Rgb, Rgb)> {
        match surface {
            Surface::PageDark => Some(([0xED, 0xEF, 0xF7], [0x9A, 0xA0, 0xB4])),
            Surface::PageLight => Some(([0x10, 0x13, 0x1C], [0x5A, 0x60, 0x72])),
            Surface::Stage => None,
        }
    }

    /// `--menu` by surface, for `danger`'s "degraded state text on menu"
    /// pairing (style guide 14.1); `None` on the stage, which has no menu.
    fn menu_color(surface: Surface) -> Option<Rgb> {
        match surface {
            Surface::PageDark => Some([0x1D, 0x21, 0x30]),
            Surface::PageLight => Some([0xFF, 0xFF, 0xFF]),
            Surface::Stage => None,
        }
    }

    /// Chromatic modes keep a minimum simulated CIEDE2000 of 20 between every
    /// pair of the five meanings, on every surface. Achromatopsia (luminance
    /// only, under a WCAG AA contrast floor on three surfaces at once) cannot
    /// reach that bar; its own floor is 9.5 (module docs explain the
    /// ceiling).
    #[test]
    fn vision_modes_stay_distinct_under_simulation() {
        let cases: &[(Vision, Deficiency, f64)] = &[
            (Vision::Protanopia, Deficiency::Protan, 20.0),
            (Vision::Deuteranopia, Deficiency::Deutan, 20.0),
            (Vision::Tritanopia, Deficiency::Tritan, 20.0),
            (Vision::Achromatopsia, Deficiency::Achroma, 9.5),
        ];
        for (vision, deficiency, min_needed) in cases {
            for surface in [Surface::PageDark, Surface::PageLight, Surface::Stage] {
                let p = Semantic::for_surface(surface, *vision);
                let colors: Vec<Rgb> = Meaning::ALL.map(|m| p.color(m)).to_vec();
                let min = cvd::min_simulated_distance(&colors, *deficiency);
                assert!(
                    min >= *min_needed,
                    "{vision:?} {surface:?}: simulated min dE00 {min:.2} < {min_needed}"
                );
            }
        }
    }

    /// M2: every meaning except `neutral` (COL-5 exemption, see
    /// [`text_colors`]) stays at least the mode's own bar away from the
    /// page's body and secondary text, simulated. `needs_input` reading as
    /// body text (achromatopsia, ΔE00 3.06 dark and 1.76 light before this
    /// fix) or `neutral` reading as text (tritanopia light, 5.4) is exactly
    /// the failure this guards against.
    #[test]
    fn vision_modes_stay_distinct_from_text() {
        let cases: &[(Vision, Deficiency, f64)] = &[
            (Vision::Protanopia, Deficiency::Protan, 20.0),
            (Vision::Deuteranopia, Deficiency::Deutan, 20.0),
            (Vision::Tritanopia, Deficiency::Tritan, 20.0),
            (Vision::Achromatopsia, Deficiency::Achroma, 2.5),
        ];
        for (vision, deficiency, min_needed) in cases {
            for surface in [Surface::PageDark, Surface::PageLight] {
                let (text, text_2) = text_colors(surface).expect("page surface has text");
                let p = Semantic::for_surface(surface, *vision);
                for m in [
                    Meaning::Progress,
                    Meaning::Danger,
                    Meaning::NeedsInput,
                    Meaning::Selection,
                ] {
                    let c = p.color(m);
                    for (label, t) in [("text", text), ("text-2", text_2)] {
                        let d = cvd::ciede2000(
                            cvd::rgb_to_lab(cvd::simulate(c, *deficiency)),
                            cvd::rgb_to_lab(cvd::simulate(t, *deficiency)),
                        );
                        assert!(
                            d >= *min_needed,
                            "{vision:?} {surface:?} {m:?} vs {label}: dE00 {d:.2} < {min_needed}"
                        );
                    }
                }
            }
        }
    }

    /// L4: `progress_end` keeps 3:1 contrast on its surface (the same floor
    /// `progress` itself needs, since both only ever draw a dot, a bar or a
    /// scrubber knob) and stays at the mode's own bar away from `neutral`
    /// specifically (module docs: it is not checked against the other three
    /// meanings, since it never appears apart from `progress`). Before this
    /// fix, achromatopsia's light `progress_end` was 1.75:1, and its dark
    /// `progress_end` was ΔE00 5.05 from `neutral`.
    #[test]
    fn vision_modes_progress_end_is_usable() {
        let cases: &[(Vision, Deficiency, f64)] = &[
            (Vision::Protanopia, Deficiency::Protan, 20.0),
            (Vision::Deuteranopia, Deficiency::Deutan, 20.0),
            (Vision::Tritanopia, Deficiency::Tritan, 20.0),
            (Vision::Achromatopsia, Deficiency::Achroma, 9.5),
        ];
        const PAGE_DARK_BG: Rgb = [0x0D, 0x0F, 0x17];
        const PAGE_LIGHT_BG: Rgb = [0xF7, 0xF8, 0xFC];
        const STAGE_BG: Rgb = [0x07, 0x08, 0x0F];
        for (vision, deficiency, min_needed) in cases {
            for (surface, bg) in [
                (Surface::PageDark, PAGE_DARK_BG),
                (Surface::PageLight, PAGE_LIGHT_BG),
                (Surface::Stage, STAGE_BG),
            ] {
                let p = Semantic::for_surface(surface, *vision);
                let ratio = cvd::contrast_ratio(p.progress_end, bg);
                assert!(
                    ratio >= 3.0,
                    "{vision:?} {surface:?}: progress_end contrast {ratio:.2} < 3.0"
                );
                let d = cvd::ciede2000(
                    cvd::rgb_to_lab(cvd::simulate(p.progress_end, *deficiency)),
                    cvd::rgb_to_lab(cvd::simulate(p.neutral, *deficiency)),
                );
                assert!(
                    d >= *min_needed,
                    "{vision:?} {surface:?}: progress_end vs neutral dE00 {d:.2} < {min_needed}"
                );
            }
        }
    }

    /// Every meaning meets WCAG 2.2 AA against its surface's background, on
    /// every vision mode: 4.5:1 on a page (these colors also draw text and
    /// icons there), 3:1 on the stage (there they mark non-text elements
    /// only), and 3:1 for `progress` everywhere (it only ever draws a dot, a
    /// bar or a scrubber knob, matching the style guide's own "Live dot and
    /// scrubber knob (accent)" row, 14.1). `danger` also keeps 4.5:1 against
    /// `--menu` (H1: the "degraded state text on menu" pairing), on the two
    /// page surfaces.
    #[test]
    fn vision_modes_meet_wcag_aa() {
        const PAGE_DARK_BG: Rgb = [0x0D, 0x0F, 0x17];
        const PAGE_LIGHT_BG: Rgb = [0xF7, 0xF8, 0xFC];
        const STAGE_BG: Rgb = [0x07, 0x08, 0x0F];
        for vision in Vision::ALL {
            for (surface, bg, page) in [
                (Surface::PageDark, PAGE_DARK_BG, true),
                (Surface::PageLight, PAGE_LIGHT_BG, true),
                (Surface::Stage, STAGE_BG, false),
            ] {
                let p = Semantic::for_surface(surface, vision);
                for m in Meaning::ALL {
                    let need = if page && m != Meaning::Progress {
                        4.5
                    } else {
                        3.0
                    };
                    let ratio = cvd::contrast_ratio(p.color(m), bg);
                    assert!(
                        ratio >= need,
                        "{vision:?} {surface:?} {m:?}: contrast {ratio:.2} < {need}"
                    );
                }
                if let Some(menu) = menu_color(surface) {
                    let ratio = cvd::contrast_ratio(p.danger, menu);
                    assert!(
                        ratio >= 4.5,
                        "{vision:?} {surface:?}: danger on menu {ratio:.2} < 4.5"
                    );
                }
            }
        }
    }
}
