# Sources of the desktop shell's assets

The shell draws the Stage style (`docs/STYLE_GUIDE.md`). Its assets are listed in the guide's
section 15 and ship in this tree (AST-1). The app makes no network request for them.

## Fonts (`fonts/`)

Style guide 1.9, table 4.1 and section 15. Both fonts keep their real family names (AST-4);
neither declares a Reserved Font Name.

| File | Source | License |
|---|---|---|
| `woff2/instrument-sans-latin-400-normal.woff2`, `-500-`, `-700-` | npm `@fontsource/instrument-sans@5.3.0`, `files/`, latin subset, taken 2026-09-27 (upstream https://github.com/Instrument/instrument-sans; Copyright 2022 The Instrument Sans Project Authors) | SIL OFL 1.1, `fonts/InstrumentSans-OFL.txt` |
| `woff2/fragment-mono-latin-400-normal.woff2` | npm `@fontsource/fragment-mono@5.3.0`, `files/`, latin subset, taken 2026-09-27 (upstream https://github.com/weiweihuanghuang/fragment-mono; Copyright 2022 The Fragment-Mono Project Authors) | SIL OFL 1.1, `fonts/FragmentMono-OFL.txt` |
| `InstrumentSans-Regular.ttf`, `InstrumentSans-Medium.ttf`, `InstrumentSans-Bold.ttf` | the woff2 files above, decompressed to TrueType by `fonts/build_fonts.py` (egui reads TrueType, not woff2), with tabular digits baked in: the character map entries for `0` to `9` point at the glyphs the font's own `tnum` feature substitutes (`zero.tf` to `nine.tf`, 600 units wide) (TYP-2; egui applies no OpenType features). The signs the feature also covers keep their normal glyphs, so prose spacing is unchanged. Outlines, names and metrics unchanged. | SIL OFL 1.1 |
| `FragmentMono-Regular.ttf` | the woff2 file above, decompressed; unchanged (it is monospaced) | SIL OFL 1.1 |
| `kgraf-icons.ttf` | built by `fonts/build_fonts.py` from the SVGs in `icons/`, one glyph per icon at U+E000 upward (`src/icons.rs`) | Apache 2.0, `icons/MaterialSymbols-Apache-2.0.txt` |

The build is reproducible: fontTools 4.66.0 and brotli 1.2.0 are pinned in the command in
`build_fonts.py`, timestamps are kept, and a rerun gives the same bytes.

## Icons (`icons/`)

Material Symbols Outlined, weight 400, from npm `@material-symbols/svg-400@0.47.5`
(https://github.com/marella/material-symbols, which packages
https://github.com/google/material-design-icons), taken 2026-09-27. File names are the Material
Symbols names; `-fill` is the filled variant. Only the icons the shell draws are here (ICO-1).
License: Apache 2.0, `icons/MaterialSymbols-Apache-2.0.txt`.

## The kGraf mark

Drawn in code (`src/chrome.rs`, `kgraf_mark`): a "k" as graph edges with one `--accent`
node (AST-2). No image file.
