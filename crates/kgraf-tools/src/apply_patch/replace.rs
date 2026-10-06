// Ported from openai/codex codex-rs/apply-patch/src/file_update.rs
// at e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0 (`compute_replacements`
// and `apply_replacements` only; the rest of that file is the `ExecutorFileSystem`
// I/O glue codex uses, which this crate does not vendor, see `apply_patch::mod`
// doc comment).
// Changes: dropped the `ApplyPatchFileUpdateMode::PreserveLineEndings` branch (and
// the `text_file::SourceFile` type it needed) along with the parameter that
// selected it. The same result comes from a simpler rule in `derive_new_contents`:
// the file's dominant line ending (LF or CRLF) and its final-newline state are
// detected first, the edit runs on lines without their endings, and every line
// the patch did not touch keeps its own ending. Only the lines the patch adds
// take the dominant ending, so a file with mixed endings changes only where
// it was edited.
// Also added (not ported): each `Err` case now appends
// `seek_sequence::describe_mismatch`'s diagnostic -- see that function's doc
// comment for why. It only adds text to an error message a caller already
// receives; it does not change whether, or where, a pattern matches.
// Also changed: a chunk placed by a tolerant rule (`Seek::Tolerant`) is no
// longer written back as the patch's text. Its context lines are the file's
// own lines (upstream's `PreserveLineEndings` branch keeps context lines the
// same way; dropping that branch had dropped this too), and its added lines
// are moved to the file's indentation, or the chunk is refused
// (`apply_patch::reindent`). A Python or YAML file that parsed before such an
// edit and does not after it is refused (`apply_patch::syntax`).

use super::parser::UpdateFileChunk;
use super::seek_sequence::{self, Seek};
use super::{reindent, syntax};

/// The result note for a line group that matched only under a tolerant rule.
fn tolerant_note(path: &str, what: &str, index: usize) -> String {
    format!(
        "note: in {path}, {what} matched at line {} only after ignoring whitespace or \
         typographic punctuation differences; they are not exact copies of the file, so the \
         file's own copies of the unchanged lines were kept",
        index + 1
    )
}

/// The refusal for a tolerant match that fits in several places.
fn ambiguous(path: &str, what: &str, at: &[usize]) -> String {
    let lines: Vec<String> = at.iter().take(8).map(|i| (i + 1).to_string()).collect();
    format!(
        "{what} match more than one place in {path} (lines {}) once whitespace differences are \
         ignored, so none was chosen. Copy the lines exactly as the file has them, or add a \
         unique line before them.",
        lines.join(", ")
    )
}

/// The refusal for an update chunk that adds lines and removes or keeps none.
fn anchorless_addition(path: &str, chunk: &UpdateFileChunk) -> String {
    let first = chunk.new_lines.first().map_or("", String::as_str);
    format!(
        "An Update File chunk for {path} has only `+` lines, so it names no place in the file and \
         was not applied. To insert lines, put an unchanged line from the file just before them, \
         starting with a space, so they go after it. To append at the end, finish the chunk with \
         `*** End of File`. To make a new file or replace one, use `*** Add File: {path}` (first \
         added line: '{first}')."
    )
}

/// `(start_index, old_len, new_lines)`: replace `old_len` lines starting at
/// `start_index` with `new_lines`.
type Replacement = (usize, usize, Vec<String>);

/// The replacements for one file, the notes for the result, and whether any
/// chunk was placed by a tolerant rule.
struct Computed {
    replacements: Vec<Replacement>,
    notes: Vec<String>,
    tolerant: bool,
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`, plus a note for each place a line
/// matched only under a tolerant rule (whitespace, typographic punctuation).
/// A chunk placed by a tolerant rule is written in the file's own whitespace
/// ([`reindent::fit_to_file`]), or refused when that cannot be measured.
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<Computed, String> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut tolerant = false;
    let mut line_index: usize = 0;

    for chunk in chunks {
        // If a chunk has a `change_context`, we use `seek` to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            let pattern = std::slice::from_ref(ctx_line);
            match seek_sequence::seek(original_lines, pattern, line_index, /*eof*/ false) {
                Seek::Exact(idx) => line_index = idx + 1,
                Seek::Tolerant(idx) => {
                    notes.push(tolerant_note(path, "the `@@` context line", idx));
                    line_index = idx + 1;
                }
                Seek::Ambiguous(at) => {
                    return Err(ambiguous(
                        path,
                        &format!("the `@@` context '{ctx_line}'"),
                        &at,
                    ));
                }
                Seek::Missing => {
                    let hint =
                        seek_sequence::describe_mismatch(original_lines, pattern, line_index);
                    return Err(format!(
                        "Failed to find context '{ctx_line}' in {path}. {hint}"
                    ));
                }
            }
        }

        if chunk.old_lines.is_empty() {
            // A chunk of only `+` lines names no line to go after. Placing it
            // at the end of the file regardless was a silent append (a file
            // that gained the same block once per retry), so it is refused
            // unless the file is empty or the chunk says `*** End of File`.
            if !original_lines.is_empty() && !chunk.is_end_of_file {
                return Err(anchorless_addition(path, chunk));
            }
            // Preserve the legacy split representation's handling of a final
            // empty line: a trailing empty element in `original_lines` marks
            // the file's final newline, so the true insertion point is just
            // before it.
            let insertion_idx = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        // Otherwise, try to match the existing lines in the file with the old lines
        // from the chunk. If found, schedule that region for replacement.
        let mut pattern: &[String] = &chunk.old_lines;
        let mut found =
            seek_sequence::seek(original_lines, pattern, line_index, chunk.is_end_of_file);

        let mut new_slice: &[String] = &chunk.new_lines;

        if matches!(found, Seek::Missing) && pattern.last().is_some_and(String::is_empty) {
            // Retry without the trailing empty line which represents the final
            // newline in the file.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }

            found = seek_sequence::seek(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        match found {
            Seek::Exact(start_idx) => {
                replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
                line_index = start_idx + pattern.len();
            }
            Seek::Tolerant(start_idx) => {
                // The patch's lines are not exact copies of the file's, so
                // writing them back as they are would put the patch's
                // whitespace and punctuation into the file: context lines are
                // taken from the file and added lines moved to its indentation.
                let fitted = reindent::fit_to_file(
                    path,
                    original_lines,
                    start_idx,
                    pattern,
                    new_slice,
                    &chunk.context_line_indices,
                )?;
                notes.push(tolerant_note(
                    path,
                    "the patch's unchanged and `-` lines",
                    start_idx,
                ));
                notes.extend(fitted.shift_note);
                tolerant = true;
                replacements.push((start_idx, pattern.len(), fitted.lines));
                line_index = start_idx + pattern.len();
            }
            Seek::Ambiguous(at) => {
                return Err(ambiguous(path, "the patch's unchanged and `-` lines", &at));
            }
            Seek::Missing => {
                // Mirror `seek`'s own eof branch so the diagnostic looks at the
                // same window `seek` searched, not a different one.
                let search_start = if chunk.is_end_of_file && original_lines.len() >= pattern.len()
                {
                    original_lines.len() - pattern.len()
                } else {
                    line_index
                };
                let hint = seek_sequence::describe_mismatch(original_lines, pattern, search_start);
                return Err(format!(
                    "Failed to find expected lines in {}:\n{}\n{hint}",
                    path,
                    chunk.old_lines.join("\n"),
                ));
            }
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok(Computed {
        replacements,
        notes,
        tolerant,
    })
}

/// Apply the `(start_index, old_len, new_lines)` replacements to `original_lines`,
/// returning the modified file contents as a vector of lines.
pub(crate) fn apply_replacements<T: Clone>(
    mut lines: Vec<T>,
    replacements: &[(usize, usize, Vec<T>)],
) -> Vec<T> {
    // We must apply replacements in descending order so that earlier replacements
    // don't shift the positions of later ones.
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        let start_idx = *start_idx;
        let old_len = *old_len;

        // Remove old lines.
        for _ in 0..old_len {
            if start_idx < lines.len() {
                lines.remove(start_idx);
            }
        }

        // Insert new lines.
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(start_idx + offset, new_line.clone());
        }
    }

    lines
}

/// The dominant line ending of `text`: CRLF when more than half of its line
/// feeds follow a carriage return, LF otherwise (and for a file with no line
/// feed at all).
fn uses_crlf(text: &str) -> bool {
    let feeds = text.matches('\n').count();
    let crlf = text.matches("\r\n").count();
    feeds > 0 && crlf * 2 > feeds
}

/// How far past the current old line a context line is looked for when
/// lining up a replaced region; bounds the work on a large region.
const CONTEXT_LOOKAHEAD: usize = 64;

/// The endings for `new`, the lines that replace `old` (whose endings are
/// `old_endings`). A replaced region holds the chunk's context lines as well
/// as its changes; a new line equal to an old line at or shortly after the
/// last one matched is that context line and keeps its ending. Every other
/// new line is an added line and takes `dominant`.
fn region_endings(
    old: &[String],
    old_endings: &[&'static str],
    new: &[String],
    dominant: &'static str,
) -> Vec<&'static str> {
    let mut next = 0;
    new.iter()
        .map(|line| {
            let window = old.len().min(next + CONTEXT_LOOKAHEAD);
            match (next..window).find(|&k| old[k] == *line) {
                Some(k) => {
                    next = k + 1;
                    old_endings[k]
                }
                None => dominant,
            }
        })
        .collect()
}

/// Apply `chunks` to `original`, returning the new file contents. Each line
/// the patch does not touch keeps its own ending (LF or CRLF), and the file
/// keeps its final-newline state: a file that did not end in a newline still
/// does not, and one that did still does. Lines the patch adds take the
/// file's dominant ending. The edit itself runs on lines without their
/// endings, so patch lines (always LF) match a CRLF line exactly.
pub(crate) fn derive_new_contents(
    original: &str,
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<(String, Vec<String>), String> {
    let dominant = if uses_crlf(original) { "\r\n" } else { "\n" };
    let final_newline = original.is_empty() || original.ends_with('\n');
    let segments: Vec<&str> = original.split('\n').collect();
    let last = segments.len() - 1;
    let mut original_lines = Vec::with_capacity(segments.len());
    let mut endings: Vec<&'static str> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        if i == last {
            // After the final line feed (empty when the file ends in one), or
            // a final line with no line feed at all: it has no ending of its
            // own, and takes the dominant one if a later line is added.
            original_lines.push((*segment).to_owned());
            endings.push(dominant);
        } else if let Some(content) = segment.strip_suffix('\r') {
            original_lines.push(content.to_owned());
            endings.push("\r\n");
        } else {
            original_lines.push((*segment).to_owned());
            endings.push("\n");
        }
    }
    // Drop the trailing empty element that results from the final newline so
    // that line counts match the behaviour of standard `diff`.
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
        endings.pop();
    }

    let Computed {
        replacements,
        notes,
        tolerant,
    } = compute_replacements(&original_lines, path, chunks)?;
    let ending_replacements: Vec<(usize, usize, Vec<&'static str>)> = replacements
        .iter()
        .map(|(start, old_len, new)| {
            let old = &original_lines[*start..*start + *old_len];
            let old_endings = &endings[*start..*start + *old_len];
            let new_endings = region_endings(old, old_endings, new, dominant);
            (*start, *old_len, new_endings)
        })
        .collect();
    let new_lines = apply_replacements(original_lines, &replacements);
    let new_endings = apply_replacements(endings, &ending_replacements);
    let mut out = String::with_capacity(original.len());
    let count = new_lines.len();
    for (i, (line, ending)) in new_lines.iter().zip(&new_endings).enumerate() {
        out.push_str(line);
        if i + 1 < count || final_newline {
            out.push_str(ending);
        }
    }
    if tolerant {
        syntax::refuse_if_newly_broken(path, original, &out)?;
    }
    Ok((out, notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_patch::parse_patch;
    use crate::apply_patch::parser::Hunk;

    fn derive(original: &str, path: &str, chunks: &[UpdateFileChunk]) -> Result<String, String> {
        derive_new_contents(original, path, chunks).map(|(text, _)| text)
    }

    fn update_chunks(patch_body: &str) -> Vec<UpdateFileChunk> {
        let patch = format!("*** Begin Patch\n{patch_body}\n*** End Patch");
        let hunks = parse_patch(&patch).expect("parses");
        let [Hunk::UpdateFile { chunks, .. }] = hunks.as_slice() else {
            panic!("expected exactly one update hunk, got {hunks:?}");
        };
        chunks.clone()
    }

    #[test]
    fn a_single_line_replacement_keeps_surrounding_lines() {
        let chunks = update_chunks("*** Update File: f\n@@\n foo\n-bar\n+baz");
        let new = derive("foo\nbar\nqux\n", "f", &chunks).expect("applies");
        assert_eq!(new, "foo\nbaz\nqux\n");
    }

    /// A CRLF file stays CRLF on every line, not just the rewritten ones.
    #[test]
    fn a_crlf_file_keeps_its_line_endings() {
        let chunks = update_chunks("*** Update File: f\n@@\n foo\n-bar\n+baz\n+extra");
        let new = derive("foo\r\nbar\r\nqux\r\n", "f", &chunks).expect("applies");
        assert_eq!(new, "foo\r\nbaz\r\nextra\r\nqux\r\n");
        // A pure addition at the end keeps them too.
        let chunks = update_chunks("*** Update File: f\n@@\n+tail\n*** End of File");
        let new = derive("a\r\nb\r\n", "f", &chunks).expect("applies");
        assert_eq!(new, "a\r\nb\r\ntail\r\n");
    }

    /// A file with mixed endings changes only where it was edited: context
    /// and untouched lines keep their own ending, added lines take the
    /// dominant one.
    #[test]
    fn a_mixed_file_changes_only_where_it_was_edited() {
        // CRLF dominant, one LF line the patch does not touch.
        let chunks = update_chunks("*** Update File: f\n@@\n a\n-b\n+B");
        let new = derive("a\r\nb\r\nlf\nc\r\n", "f", &chunks).expect("applies");
        assert_eq!(new, "a\r\nB\r\nlf\nc\r\n");
        // LF dominant, one CRLF line the patch does not touch.
        let chunks = update_chunks("*** Update File: f\n@@\n-a\n+A\n+added");
        let new = derive("a\nb\ncr\r\nc\n", "f", &chunks).expect("applies");
        assert_eq!(new, "A\nadded\nb\ncr\r\nc\n");
        // A CRLF line in an LF file still matches patch context.
        let chunks = update_chunks("*** Update File: f\n@@\n cr\n-c\n+C");
        let new = derive("a\ncr\r\nc\nd\n", "f", &chunks).expect("applies");
        assert_eq!(new, "a\ncr\r\nC\nd\n");
        // A line added after a final line with no newline gives it one.
        let chunks = update_chunks("*** Update File: f\n@@\n last\n+more");
        let new = derive("x\r\nlast", "f", &chunks).expect("applies");
        assert_eq!(new, "x\r\nlast\r\nmore");
    }

    /// An LF file stays LF, and a file whose last line has no newline does
    /// not gain one; a file that ended in a newline keeps it.
    #[test]
    fn the_final_newline_state_is_kept() {
        let chunks = update_chunks("*** Update File: f\n@@\n foo\n-bar\n+baz");
        assert_eq!(
            derive("foo\nbar", "f", &chunks).expect("applies"),
            "foo\nbaz"
        );
        assert_eq!(
            derive("foo\nbar\n", "f", &chunks).expect("applies"),
            "foo\nbaz\n"
        );
        assert_eq!(
            derive("foo\r\nbar", "f", &chunks).expect("applies"),
            "foo\r\nbaz"
        );
        // A pure addition to a file with no final newline.
        let chunks = update_chunks("*** Update File: f\n@@\n+tail\n*** End of File");
        assert_eq!(derive("only", "f", &chunks).expect("applies"), "only\ntail");
    }

    #[test]
    fn a_pure_addition_chunk_marked_end_of_file_appends() {
        let chunks = update_chunks("*** Update File: f\n@@\n+new-line\n*** End of File");
        let new = derive("only\n", "f", &chunks).expect("applies");
        assert_eq!(new, "only\nnew-line\n");
    }

    #[test]
    fn a_missing_context_line_is_reported_with_the_path() {
        let chunks = update_chunks("*** Update File: f\n@@\n missing-context\n-x\n+y");
        let err = derive("a\nb\n", "f", &chunks).unwrap_err();
        assert!(err.contains("missing-context"));
        assert!(err.contains('f'));
    }

    /// Regression, 2026-09-28: `claude-haiku-4-5-20251001`'s actual first
    /// attempt at the `refactor-parser` bench brief (captured live via
    /// `KGRAF_BENCH_KEEP_DIR`), reproduced against the brief's own fixture
    /// content. The `@@` anchor names only a fragment of the real function
    /// signature line; the error must say so, not just that it failed.
    #[test]
    fn a_truncated_at_context_names_the_real_line_it_almost_matched() {
        let original = [
            "//! Extract the per-field parsing below into a new public function named",
            "//! exactly `parse_int_field(field: &str) -> i64` (trim the field, parse it,",
            "//! default to 0 on error) and call it from `tally_csv_ints` for each field.",
            "//! `tally_csv_ints`'s behavior must not change. Do not change",
            "//! `tests/behavior.rs` or `tests/structure.rs`.",
            "",
            "pub fn tally_csv_ints(line: &str) -> i64 {",
            "    let mut total = 0i64;",
            "    for field in line.split(',') {",
            "        let trimmed = field.trim();",
            "        let value: i64 = trimmed.parse().unwrap_or(0);",
            "        total += value;",
            "    }",
            "    total",
            "}",
            "",
        ]
        .join("\n");
        let patch_body = [
            "*** Update File: src/lib.rs",
            "@@ pub fn tally_csv_ints",
            "+pub fn parse_int_field(field: &str) -> i64 {",
            "+    let trimmed = field.trim();",
            "+    trimmed.parse().unwrap_or(0)",
            "+}",
            "+",
            " pub fn tally_csv_ints(line: &str) -> i64 {",
            "     let mut total = 0i64;",
            "     for field in line.split(',') {",
            "-        let trimmed = field.trim();",
            "-        let value: i64 = trimmed.parse().unwrap_or(0);",
            "-        total += value;",
            "+        total += parse_int_field(field);",
            "     }",
            "     total",
            " }",
        ]
        .join("\n");
        let chunks = update_chunks(&patch_body);
        let err = derive(&original, "src/lib.rs", &chunks).unwrap_err();
        assert!(
            err.contains("pub fn tally_csv_ints(line: &str) -> i64 {"),
            "must quote the real full line back: {err}"
        );
        assert!(
            err.contains("whole line"),
            "must explain a `@@` line needs the whole line: {err}"
        );
    }

    /// Regression, 2026-09-28: the same brief's second failing attempt used a
    /// bare unified-diff hunk header (`@@ -5,11 +5,15 @@`) as a `@@` context
    /// line -- exactly the syntax the tool description warns against.
    #[test]
    fn a_unified_diff_style_at_context_is_named_as_such() {
        let chunks = update_chunks(
            "*** Update File: f\n\
             @@ -5,11 +5,15 @@\n\
             +new\n",
        );
        let err = derive("a\nb\nc\n", "f", &chunks).unwrap_err();
        assert!(
            err.contains("unified-diff hunk header"),
            "must name the mistake by kind: {err}"
        );
    }

    /// A greenfield worker sent `*** Update File: Cargo.toml` with only `+`
    /// lines three times; each applied as an append and the file held three
    /// `[workspace]` tables.
    #[test]
    fn an_update_of_only_added_lines_is_refused_not_appended() {
        let chunks = update_chunks(
            "*** Update File: Cargo.toml\n+[workspace]\n+members = [\"wcl-core\", \"wcl\"]",
        );
        let original = "[package]\nname = \"wcl-workspace\"\n\n[workspace]\nmembers = [\"wcl-core\", \"wcl\"]\n";
        let err = derive(original, "Cargo.toml", &chunks).unwrap_err();
        assert!(err.contains("only `+` lines"), "{err}");
        assert!(err.contains("Cargo.toml"), "{err}");
        assert!(err.contains("*** End of File"), "{err}");
        assert!(err.contains("*** Add File: Cargo.toml"), "{err}");
        // An `@@` context does not anchor it either: it used to append at the end.
        let chunks = update_chunks("*** Update File: f\n@@ b\n+new");
        let err = derive("a\nb\nc\n", "f", &chunks).unwrap_err();
        assert!(err.contains("only `+` lines"), "{err}");
    }

    #[test]
    fn an_update_of_only_added_lines_to_an_empty_file_is_applied() {
        let chunks = update_chunks("*** Update File: f\n+one\n+two");
        assert_eq!(derive("", "f", &chunks).expect("applies"), "one\ntwo\n");
    }

    /// A tolerant match keeps the file's own copy of each context line
    /// (trailing whitespace included) and moves the added lines by the
    /// measured indentation offset.
    #[test]
    fn indentation_and_trailing_whitespace_differences_match_once_with_a_note() {
        let chunks =
            update_chunks("*** Update File: f\n@@\n fn a() {\n-    old();\n+    new();\n }");
        // The file has the whole block 4 spaces deeper, and trailing spaces.
        let (new, notes) =
            derive_new_contents("    fn a() {  \n        old();\n    }\n", "f", &chunks)
                .expect("applies");
        assert_eq!(new, "    fn a() {  \n        new();\n    }\n");
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].starts_with("note: in f"), "{notes:?}");
        assert!(
            notes[1].contains("4 spaces less indentation than the file")
                && notes[1].contains("1 added line was re-indented"),
            "{notes:?}"
        );
        // Only trailing whitespace differs: the context line keeps the
        // file's, the added line needs no move.
        let (new, notes) =
            derive_new_contents("fn a() {  \n    old();\n}\n", "f", &chunks).expect("applies");
        assert_eq!(new, "fn a() {  \n    new();\n}\n");
        assert_eq!(notes.len(), 1, "{notes:?}");
        // An exact match carries no note.
        let (_, notes) =
            derive_new_contents("fn a() {\n    old();\n}\n", "f", &chunks).expect("applies");
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// Spaces in the patch against a tab in the file, with an unindented
    /// line pinning the offset at zero, is two offsets but one tab width:
    /// the added line is written with the file's tab (gofmt's Go, a
    /// Makefile). Mixed widths, or an added line between widths, are refused,
    /// file named.
    #[test]
    fn spaces_against_tabs_are_written_as_the_file_s_tabs() {
        let chunks =
            update_chunks("*** Update File: f\n@@\n fn a() {\n-    old();\n+    new();\n }");
        assert_eq!(
            derive("fn a() {\n\told();\n}\n", "f", &chunks).expect("applies"),
            "fn a() {\n\tnew();\n}\n"
        );
        let uneven =
            update_chunks("*** Update File: f\n@@\n fn a() {\n-    old();\n+      new();\n }");
        let err = derive("fn a() {\n\told();\n}\n", "f", &uneven).unwrap_err();
        assert!(err.contains("whole number of 4 spaces"), "{err}");
        // Two widths (4 spaces for one tab, 2 for the next): not tabs at one width.
        let mixed = update_chunks(
            "*** Update File: f\n@@\n fn a() {\n-    if x {\n-      old();\n+    if x {\n+      new();\n }",
        );
        let err = derive("fn a() {\n\tif x {\n\t\told();\n}\n", "f", &mixed).unwrap_err();
        assert!(
            err.contains("not shifted from the file by one amount"),
            "{err}"
        );
    }

    #[test]
    fn a_whitespace_tolerant_match_that_fits_twice_is_refused() {
        let chunks = update_chunks("*** Update File: f\n@@\n-x = 1\n+x = 2");
        let err = derive("  x = 1\n\tx = 1\n", "f", &chunks).unwrap_err();
        assert!(err.contains("more than one place"), "{err}");
        assert!(err.contains("lines 1, 2"), "{err}");
        // The same lines matched exactly keep the first-match rule.
        assert_eq!(
            derive("x = 1\nx = 1\n", "f", &chunks).expect("applies"),
            "x = 2\nx = 1\n"
        );
    }

    #[test]
    fn a_context_line_that_fits_twice_after_ignoring_whitespace_is_refused() {
        let chunks = update_chunks("*** Update File: f\n@@ fn a() {\n-    old();\n+    new();");
        let err = derive(" fn a() {\n    old();\n\tfn a() {\n", "f", &chunks).unwrap_err();
        assert!(err.contains("`@@` context"), "{err}");
    }
}
