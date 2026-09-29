//! Material Symbols Outlined glyphs (Apache 2.0), as characters of the embedded
//! `kgraf-icons.ttf` (`assets/fonts/build_fonts.py` makes it from the mockup's
//! vendored SVGs). The code points follow the script's `ICONS` order from U+E000;
//! a test checks that each one has a glyph.
//!
//! Drawn with [`crate::theme::icon_font`], so an icon takes the text color like
//! the mockup's CSS masks take `currentColor`.

pub const MENU: char = '\u{E000}';
pub const SEARCH: char = '\u{E001}';
pub const CLOSE: char = '\u{E002}';
pub const PLAY: char = '\u{E003}';
pub const PAUSE: char = '\u{E004}';
pub const BELL: char = '\u{E005}';
pub const BELL_FILL: char = '\u{E006}';
pub const GRAPH: char = '\u{E007}';
pub const GRAPH_FILL: char = '\u{E008}';
pub const PROVIDERS: char = '\u{E009}';
pub const PROVIDERS_FILL: char = '\u{E00A}';
pub const SETTINGS: char = '\u{E00B}';
pub const SETTINGS_FILL: char = '\u{E00C}';
pub const DARK_MODE: char = '\u{E00D}';
pub const LIGHT_MODE: char = '\u{E00E}';
pub const AUTO_THEME: char = '\u{E00F}';
pub const MEMORY: char = '\u{E010}';
pub const LEGEND: char = '\u{E011}';
pub const MAP: char = '\u{E012}';
pub const MAP_FILL: char = '\u{E013}';
pub const REMOVE: char = '\u{E014}';
pub const ADD: char = '\u{E015}';
pub const FIT: char = '\u{E016}';
pub const REPLAY: char = '\u{E017}';
pub const ARROW_UP: char = '\u{E018}';
pub const ARROW_DOWN: char = '\u{E019}';
pub const CHEVRON_RIGHT: char = '\u{E01A}';
pub const MORE: char = '\u{E01B}';
pub const PERSON: char = '\u{E01C}';
pub const BOT: char = '\u{E01D}';
pub const SEND: char = '\u{E01E}';
pub const CHECK: char = '\u{E01F}';
pub const PIN: char = '\u{E020}';
pub const SHIELD: char = '\u{E021}';
pub const HELP: char = '\u{E022}';
pub const WARNING: char = '\u{E023}';
pub const ERROR: char = '\u{E024}';
pub const INFO: char = '\u{E025}';
pub const SCHEDULE: char = '\u{E026}';
pub const TREE: char = '\u{E027}';
pub const HISTORY: char = '\u{E028}';
pub const UNDO: char = '\u{E029}';
pub const BLOCK: char = '\u{E02A}';
pub const CHECK_CIRCLE: char = '\u{E02B}';
pub const VISIBILITY: char = '\u{E02C}';
pub const BACK: char = '\u{E02D}';

/// Every icon, for the glyph test.
pub const ALL: [char; 46] = [
    MENU,
    SEARCH,
    CLOSE,
    PLAY,
    PAUSE,
    BELL,
    BELL_FILL,
    GRAPH,
    GRAPH_FILL,
    PROVIDERS,
    PROVIDERS_FILL,
    SETTINGS,
    SETTINGS_FILL,
    DARK_MODE,
    LIGHT_MODE,
    AUTO_THEME,
    MEMORY,
    LEGEND,
    MAP,
    MAP_FILL,
    REMOVE,
    ADD,
    FIT,
    REPLAY,
    ARROW_UP,
    ARROW_DOWN,
    CHEVRON_RIGHT,
    MORE,
    PERSON,
    BOT,
    SEND,
    CHECK,
    PIN,
    SHIELD,
    HELP,
    WARNING,
    ERROR,
    INFO,
    SCHEDULE,
    TREE,
    HISTORY,
    UNDO,
    BLOCK,
    CHECK_CIRCLE,
    VISIBILITY,
    BACK,
];

/// An icon as rich text at `size`.
pub fn text(icon: char, size: f32) -> egui::RichText {
    egui::RichText::new(icon.to_string()).font(crate::theme::icon_font(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each named code point has a glyph in the embedded icon font, so no icon is
    /// drawn as a replacement box (fail loud: a missing glyph would be silent).
    #[test]
    fn every_icon_has_a_glyph() {
        let ctx = egui::Context::default();
        crate::theme::install(
            &ctx,
            crate::settings::Appearance::Dark,
            kgraf_core::palette::Vision::default(),
        );
        let mut out = ctx.run_ui(Default::default(), |ui| {
            ui.fonts_mut(|f| {
                let font = crate::theme::icon_font(24.0);
                for c in ALL {
                    assert!(f.has_glyph(&font, c), "U+{:04X} has no glyph", c as u32);
                }
                assert!(!f.has_glyph(&font, '\u{E02E}'), "one past the last icon");
            });
        });
        out.textures_delta.clear();
        let distinct: std::collections::BTreeSet<char> = ALL.into_iter().collect();
        assert_eq!(distinct.len(), ALL.len());
    }
}
