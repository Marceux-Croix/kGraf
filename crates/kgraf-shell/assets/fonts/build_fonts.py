"""Builds the fonts the shell embeds (decision 0055: reuse of existing permissively licensed code).

Run from the repository root:

    uv run --no-project --with fonttools==4.66.0 --with brotli==1.2.0 \
        python crates/kgraf-shell/assets/fonts/build_fonts.py

The versions are pinned: the checked-in outputs were built with them, and a rerun
with them gives the same bytes (timestamps are fixed below).

Inputs are in this crate's `assets/` (see `assets/SOURCES.md`, which lists the assets
of the Stage style):

- `fonts/woff2/`: Instrument Sans 400, 500 and 700 and Fragment Mono 400, latin subset
  (OFL 1.1; the style guide's type families). They are decompressed to TrueType, because
  egui reads TrueType and OpenType, not woff2. The latin subset is kept; egui's
  default fonts stay behind them as fallbacks for any other script. The family names
  are kept as they are (AST-4; neither font declares a reserved font name).
- Tabular numerals (TYP-2): egui applies no OpenType features, so Instrument Sans's
  own `tnum` feature is baked in for the digits only. Its one `tnum` lookup is a
  single substitution; the `cmap` entries of the digits 0 to 9 are pointed at the
  glyphs it gives them (`zero.tf` to `nine.tf`, each 600 units wide). The lookup
  also covers `+ - = < > %` and the multiply and divide signs; those keep their
  normal glyphs, so the spacing of prose is unchanged (TYP-2 is about numbers).
  Glyph outlines, names, metrics and the other tables are unchanged. Fragment Mono is monospaced (every
  digit 618 units) and is only decompressed.
- `icons/`: Material Symbols Outlined SVGs, weight 400 (Apache 2.0). Only the icons the shell
  draws become glyphs of one small TrueType font, `kgraf-icons.ttf`, at the Private
  Use Area code points in the order of `ICONS` below. `src/icons.rs` names the same
  code points; a test there checks that each one has a glyph.

The outputs are checked in, so a build needs no Python.
"""

from pathlib import Path
import re

from fontTools.ttLib import TTFont
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.pens.cu2quPen import Cu2QuPen
from fontTools.pens.transformPen import TransformPen
from fontTools.svgLib.path import parse_path

OUT = Path(__file__).resolve().parent
ASSETS = OUT.parent

# (source, output, bake tabular numerals)
FONTS = [
    ("instrument-sans-latin-400-normal.woff2", "InstrumentSans-Regular.ttf", True),
    ("instrument-sans-latin-500-normal.woff2", "InstrumentSans-Medium.ttf", True),
    ("instrument-sans-latin-700-normal.woff2", "InstrumentSans-Bold.ttf", True),
    ("fragment-mono-latin-400-normal.woff2", "FragmentMono-Regular.ttf", False),
]

# Icon names (Material Symbols file stems), in code point order from FIRST.
# Keep in step with `src/icons.rs`.
ICONS = [
    "menu", "search", "close", "play_arrow-fill", "pause-fill", "notifications",
    "notifications-fill", "bubble_chart", "bubble_chart-fill", "dns", "dns-fill",
    "settings", "settings-fill", "dark_mode", "light_mode", "brightness_auto", "memory",
    "legend_toggle", "map", "map-fill", "remove", "add", "center_focus_strong", "replay",
    "keyboard_arrow_up", "keyboard_arrow_down", "chevron_right", "more_vert", "person",
    "smart_toy", "send", "check", "keep-fill", "gpp_maybe", "help", "warning-fill",
    "error-fill", "info", "schedule", "account_tree", "history", "undo", "block",
    "check_circle-fill", "visibility", "arrow_back",
]
FIRST = 0xE000
UPM = 960
# Material Symbols draw in a 960-unit box from y = -960 (top) to 0 (bottom). Each
# glyph is flipped to font coordinates (y up) and lowered by DESCENT, so it sits on
# the text line the way an icon font's glyphs do.
DESCENT = 150


def tnum_mapping(font):
    """The glyph substitutions of the font's `tnum` feature (single substitutions)."""
    gsub = font["GSUB"].table
    mapping = {}
    for record in gsub.FeatureList.FeatureRecord:
        if record.FeatureTag != "tnum":
            continue
        for index in record.Feature.LookupListIndex:
            lookup = gsub.LookupList.Lookup[index]
            for sub in lookup.SubTable:
                if lookup.LookupType == 7:
                    sub = sub.ExtSubTable
                if not hasattr(sub, "mapping"):
                    raise SystemExit(f"tnum lookup {index} is not a single substitution")
                mapping.update(sub.mapping)
    if not mapping:
        raise SystemExit("no tnum feature to bake")
    return mapping


DIGITS = range(ord("0"), ord("9") + 1)


def bake_tnum(font):
    """Points the digits' cmap entries at their tabular glyphs. Returns the code
    points changed; fails when a digit has no tabular glyph."""
    mapping = tnum_mapping(font)
    changed = set()
    for table in font["cmap"].tables:
        for code, glyph in list(table.cmap.items()):
            if code in DIGITS and glyph in mapping:
                table.cmap[code] = mapping[glyph]
                changed.add(code)
    if set(DIGITS) - changed:
        raise SystemExit("a digit has no tabular glyph")
    return sorted(changed)


def convert_text_fonts():
    for src, dst, tnum in FONTS:
        # The upstream head timestamps are kept, so a rebuild gives the same bytes.
        font = TTFont(OUT / "woff2" / src, recalcTimestamp=False)
        font.flavor = None
        note = ""
        if tnum:
            codes = bake_tnum(font)
            note = ", tabular: " + "".join(chr(c) for c in codes)
        font.save(OUT / dst)
        print(f"{dst}: {(OUT / dst).stat().st_size} bytes{note}")


def icon_font():
    names = [".notdef"] + [f"i_{n.replace('-', '_')}" for n in ICONS]
    cmap = {FIRST + i: names[i + 1] for i in range(len(ICONS))}
    glyphs = {".notdef": TTGlyphPen(None).glyph()}
    metrics = {".notdef": (UPM, 0)}
    for name, gname in zip(ICONS, names[1:]):
        svg = (ASSETS / "icons" / f"{name}.svg").read_text()
        d = " ".join(re.findall(r'\sd="([^"]+)"', svg))
        pen = TTGlyphPen(None)
        flip = TransformPen(
            Cu2QuPen(pen, max_err=1.0, reverse_direction=True),
            (1, 0, 0, -1, 0, -DESCENT),
        )
        parse_path(d, flip)
        glyphs[gname] = pen.glyph()
        metrics[gname] = (UPM, 0)
    fb = FontBuilder(UPM, isTTF=True)
    fb.setupGlyphOrder(names)
    fb.setupCharacterMap(cmap)
    fb.setupGlyf(glyphs)
    fb.setupHorizontalMetrics(metrics)
    fb.setupHorizontalHeader(ascent=UPM - DESCENT, descent=-DESCENT)
    fb.setupNameTable({"familyName": "kGraf Icons", "styleName": "Regular"})
    fb.setupOS2(
        sTypoAscender=UPM - DESCENT,
        sTypoDescender=-DESCENT,
        usWinAscent=UPM - DESCENT,
        usWinDescent=DESCENT,
    )
    fb.setupPost()
    # A fixed timestamp, so a rebuild from the same SVGs gives the same bytes.
    fb.font["head"].created = fb.font["head"].modified = 3_900_000_000
    fb.font["head"].checkSumAdjustment = 0
    fb.save(OUT / "kgraf-icons.ttf")
    size = (OUT / "kgraf-icons.ttf").stat().st_size
    print(f"kgraf-icons.ttf: {size} bytes, {len(ICONS)} glyphs from U+{FIRST:04X}")


if __name__ == "__main__":
    convert_text_fonts()
    icon_font()
