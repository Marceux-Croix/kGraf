// Ported from openai/codex codex-rs/apply-patch/src/file_update.rs
// at e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0 (`compute_replacements`
// and `apply_replacements` only; the rest of that file is the `ExecutorFileSystem`
// I/O glue codex uses, which this crate does not vendor, see `apply_patch::mod`
// doc comment).
// Changes: dropped the `ApplyPatchFileUpdateMode::PreserveLineEndings` branch (and
// the `text_file::SourceFile` type it needed) along with the parameter that
// selected it. The same result comes from a simpler rule in `derive_new_contents`:
// the file's dominant line ending (LF or CRLF) and its final-newline state are
// detected first, the edit runs on LF lines, and both are put back afterward. A
// file with mixed endings takes its dominant one on the lines it rewrites as a
// whole (the join uses one ending).
// Also added (not ported): each `Err` case now appends
// `seek_sequence::describe_mismatch`'s diagnostic -- see that function's doc
// comment for why. It only adds text to an error message a caller already
// receives; it does not change whether, or where, a pattern matches.

use super::parser::UpdateFileChunk;
use super::seek_sequence;

/// `(start_index, old_len, new_lines)`: replace `old_len` lines starting at
/// `start_index` with `new_lines`.
type Replacement = (usize, usize, Vec<String>);

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`.
pub(crate) fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<Vec<Replacement>, String> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        // If a chunk has a `change_context`, we use seek_sequence to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence::seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                /*eof*/ false,
            ) {
                line_index = idx + 1;
            } else {
                let pattern = std::slice::from_ref(ctx_line);
                let hint = seek_sequence::describe_mismatch(original_lines, pattern, line_index);
                return Err(format!(
                    "Failed to find context '{ctx_line}' in {path}. {hint}"
                ));
            }
        }

        if chunk.old_lines.is_empty() {
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
            seek_sequence::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // Retry without the trailing empty line which represents the final
            // newline in the file.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }

            found = seek_sequence::seek_sequence(
                original_lines,
                pattern,
                line_index,
                chunk.is_end_of_file,
            );
        }

        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            // Mirror `seek_sequence`'s own eof branch (`replace.rs` calls it
            // just above with the same `chunk.is_end_of_file`) so the
            // diagnostic looks at the same window `seek_sequence` searched,
            // not a different one.
            let search_start = if chunk.is_end_of_file && original_lines.len() >= pattern.len() {
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

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok(replacements)
}

/// Apply the `(start_index, old_len, new_lines)` replacements to `original_lines`,
/// returning the modified file contents as a vector of lines.
pub(crate) fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[Replacement],
) -> Vec<String> {
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

/// Apply `chunks` to `original`, returning the new file contents. The file
/// keeps its own line ending (LF or CRLF) and its final-newline state: a file
/// that did not end in a newline still does not, and one that did still does.
/// The edit itself runs on LF lines, so patch lines (always LF) match a CRLF
/// file exactly.
pub(crate) fn derive_new_contents(
    original: &str,
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<String, String> {
    let crlf = uses_crlf(original);
    let ending = if crlf { "\r\n" } else { "\n" };
    let final_newline = original.is_empty() || original.ends_with('\n');
    let mut original_lines: Vec<String> = original
        .split('\n')
        .map(|line| {
            let line = if crlf {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                line
            };
            line.to_owned()
        })
        .collect();
    // Drop the trailing empty element that results from the final newline so
    // that line counts match the behaviour of standard `diff`.
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if new_lines.is_empty() {
        return Ok(String::new());
    }
    if final_newline {
        new_lines.push(String::new());
    }
    Ok(new_lines.join(ending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_patch::parse_patch;
    use crate::apply_patch::parser::Hunk;

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
        let new = derive_new_contents("foo\nbar\nqux\n", "f", &chunks).expect("applies");
        assert_eq!(new, "foo\nbaz\nqux\n");
    }

    /// A CRLF file stays CRLF on every line, not just the rewritten ones.
    #[test]
    fn a_crlf_file_keeps_its_line_endings() {
        let chunks = update_chunks("*** Update File: f\n@@\n foo\n-bar\n+baz\n+extra");
        let new = derive_new_contents("foo\r\nbar\r\nqux\r\n", "f", &chunks).expect("applies");
        assert_eq!(new, "foo\r\nbaz\r\nextra\r\nqux\r\n");
        // A pure addition at the end keeps them too.
        let chunks = update_chunks("*** Update File: f\n@@\n+tail");
        let new = derive_new_contents("a\r\nb\r\n", "f", &chunks).expect("applies");
        assert_eq!(new, "a\r\nb\r\ntail\r\n");
    }

    /// An LF file stays LF, and a file whose last line has no newline does
    /// not gain one; a file that ended in a newline keeps it.
    #[test]
    fn the_final_newline_state_is_kept() {
        let chunks = update_chunks("*** Update File: f\n@@\n foo\n-bar\n+baz");
        assert_eq!(
            derive_new_contents("foo\nbar", "f", &chunks).expect("applies"),
            "foo\nbaz"
        );
        assert_eq!(
            derive_new_contents("foo\nbar\n", "f", &chunks).expect("applies"),
            "foo\nbaz\n"
        );
        assert_eq!(
            derive_new_contents("foo\r\nbar", "f", &chunks).expect("applies"),
            "foo\r\nbaz"
        );
        // A pure addition to a file with no final newline.
        let chunks = update_chunks("*** Update File: f\n@@\n+tail");
        assert_eq!(
            derive_new_contents("only", "f", &chunks).expect("applies"),
            "only\ntail"
        );
    }

    #[test]
    fn a_pure_addition_chunk_inserts_at_end_of_file() {
        let chunks = update_chunks("*** Update File: f\n@@\n+new-line");
        let new = derive_new_contents("only\n", "f", &chunks).expect("applies");
        assert_eq!(new, "only\nnew-line\n");
    }

    #[test]
    fn a_missing_context_line_is_reported_with_the_path() {
        let chunks = update_chunks("*** Update File: f\n@@\n missing-context\n-x\n+y");
        let err = derive_new_contents("a\nb\n", "f", &chunks).unwrap_err();
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
        let err = derive_new_contents(&original, "src/lib.rs", &chunks).unwrap_err();
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
        let err = derive_new_contents("a\nb\nc\n", "f", &chunks).unwrap_err();
        assert!(
            err.contains("unified-diff hunk header"),
            "must name the mistake by kind: {err}"
        );
    }
}
