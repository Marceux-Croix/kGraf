// Ported from openai/codex codex-rs/apply-patch/src/seek_sequence.rs
// at e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0.
// Changes: the `update_file_mode` parameter was dropped. Upstream branches on it
// only to widen the end-of-file search window for `PreserveLineEndings`; kGraf
// matches on LF lines (`apply_patch::replace` strips and restores a CRLF file's
// carriage returns around the edit), so the `NormalizeToLf` arm is the only
// behavior kept. The tolerant passes (trailing whitespace, indentation,
// typographic punctuation) now require a single place to fit; several is
// `Seek::Ambiguous` instead of the first one.

/// A rule for when two lines are the same.
type Same = dyn Fn(&str, &str) -> bool;

/// How [`seek`] located a pattern.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Seek {
    /// Every line equal, at this index.
    Exact(usize),
    /// Found at this index only after ignoring whitespace or typographic
    /// punctuation differences; the one place the pattern fits under that rule.
    Tolerant(usize),
    /// Under a tolerant rule the pattern fits at several places (the indices);
    /// no one of them is chosen.
    Ambiguous(Vec<usize>),
    Missing,
}

/// Attempt to find the sequence of `pattern` lines within `lines` beginning at or after `start`.
/// Matches are attempted with decreasing strictness: exact match (the first one wins), then
/// ignoring trailing whitespace, then ignoring leading and trailing whitespace, then also
/// normalizing typographic punctuation. A tolerant rule must fit in exactly one place: when it
/// fits in several, the result is [`Seek::Ambiguous`], because picking the first would edit a
/// line the model may not have meant. When `eof` is true, we first try starting at the end-of-file
/// (so that patterns intended to match file endings are applied at the end).
///
/// Special cases handled defensively:
///  - Empty `pattern` returns `Exact(start)` (no-op match)
///  - `pattern.len() > lines.len()` returns `Missing` (cannot match, avoids an
///    out-of-bounds slice)
pub(crate) fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Seek {
    if pattern.is_empty() {
        return Seek::Exact(start);
    }
    if pattern.len() > lines.len() {
        return Seek::Missing;
    }
    let search_start = if eof {
        lines.len() - pattern.len()
    } else {
        start
    };
    let last = lines.len() - pattern.len();
    let fits = |i: usize, same: &Same| {
        pattern
            .iter()
            .enumerate()
            .all(|(p_idx, pat)| same(&lines[i + p_idx], pat))
    };
    if let Some(i) = (search_start..=last).find(|&i| lines[i..i + pattern.len()] == *pattern) {
        return Seek::Exact(i);
    }
    let rules: [&Same; 3] = [
        &|a, b| a.trim_end() == b.trim_end(),
        &|a, b| a.trim() == b.trim(),
        // Typographic dashes, quotes and spaces read as their ASCII forms, so
        // a patch typed in ASCII applies to a file that holds the typographic
        // ones (what `git apply` does when it ignores minor byte differences).
        &|a, b| normalise(a) == normalise(b),
    ];
    for same in rules {
        let hits: Vec<usize> = (search_start..=last).filter(|&i| fits(i, same)).collect();
        match hits.as_slice() {
            [] => {}
            [only] => return Seek::Tolerant(*only),
            _ => return Seek::Ambiguous(hits),
        }
    }
    Seek::Missing
}

fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// [`seek`] as an index: the old first-match behavior, for the unit tests of
/// the matching rules that do not care about ambiguity.
#[cfg(test)]
pub(crate) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    match seek(lines, pattern, start, eof) {
        Seek::Exact(i) | Seek::Tolerant(i) => Some(i),
        Seek::Ambiguous(_) | Seek::Missing => None,
    }
}

/// The longest a file line or a pattern line is echoed back in a hint, in
/// characters. A minified or generated file can hold a line of megabytes, and
/// the hint goes into a tool result the model reads.
const ECHO_LIMIT: usize = 160;

/// `line` cut to [`ECHO_LIMIT`] characters at a character boundary, with a
/// marker saying how much was left out.
fn clip(line: &str) -> String {
    match line.char_indices().nth(ECHO_LIMIT) {
        None => line.to_owned(),
        Some((cut, _)) => {
            let left_out = line[cut..].chars().count();
            format!("{}... [{left_out} more characters]", &line[..cut])
        }
    }
}

/// Builds a diagnostic for a [`seek_sequence`] call that returned `None`,
/// naming the nearest candidate region in `lines` and the first point where
/// it disagrees with `pattern`, plus a hint when `pattern`'s one line reads
/// like a unified-diff hunk header rather than a real source line.
///
/// **Why this exists (kGraf-specific, not ported).** Reference codex's own
/// error text is exactly "Failed to find context/expected lines in <path>"
/// (`file_update.rs`, ported verbatim in `replace.rs`) -- it never says what
/// *is* at that location, only what the caller tried. A caller that cannot
/// see the file has no way to correct a near-miss (a `@@` anchor that names
/// only part of a line, or a context block shifted by one line) without
/// guessing again from scratch, which is exactly what burned a model's whole
/// step budget in the 2026-09-28 Haiku bench failures this function was
/// written to fix. This is diagnostic text only: it changes nothing about
/// whether or where a patch matches (`seek_sequence` above is untouched), so
/// it cannot make a patch apply somewhere the reference implementation would
/// have refused.
pub(crate) fn describe_mismatch(lines: &[String], pattern: &[String], start: usize) -> String {
    if pattern.is_empty() || lines.is_empty() {
        return String::new();
    }

    // A single-line pattern (almost always a `@@` change_context) that is
    // contained *inside* a real line, but is not that whole line, is the
    // single most common near-miss: a model names `fn foo` when the file
    // reads `pub fn foo(x: i32) -> bool {`. `seek_sequence` never matches
    // this (a `@@` anchor must equal a whole line, exactly like the
    // reference), so calling it out by name saves a full round-trip.
    if pattern.len() == 1 {
        let needle = pattern[0].trim();
        if !needle.is_empty()
            && let Some((idx, hit)) = lines
                .iter()
                .enumerate()
                // From where the search began, as `seek_sequence` looked: a
                // line before `start` could never have matched.
                .skip(start)
                .find(|(_, line)| line.as_str() != pattern[0].as_str() && line.contains(needle))
        {
            let hit = clip(hit);
            return format!(
                "Line {} contains this text but is not an exact line match: '{hit}'. \
                 A `@@` context line must equal one whole line of the file, not part of \
                 one; either copy the full line or drop the `@@` line entirely and let \
                 the surrounding context lines locate the change.",
                idx + 1
            );
        }
        if looks_like_unified_diff_hunk_header(needle) {
            return format!(
                "'{}' reads like a unified-diff hunk header (`@@ -a,b +c,d @@`), which \
                 this format does not use. A `@@` line here takes a plain source line (e.g. a \
                 function signature) copied verbatim from the file, or nothing at all.",
                clip(needle)
            );
        }
    } else if looks_like_unified_diff_hunk_header(pattern[0].trim()) {
        return format!(
            "'{}' reads like a unified-diff hunk header (`@@ -a,b +c,d @@`), which this format \
             does not use.",
            clip(pattern[0].trim())
        );
    }

    let Some((best_start, score)) = best_candidate_window(lines, pattern, start) else {
        return format!(
            "No {}-line region of the file resembles this pattern at all.",
            pattern.len()
        );
    };
    if score == pattern.len() {
        // Every line matched loosely but the earlier exact/rstrip/trim/unicode
        // passes still failed: this happens when the *position* is wrong,
        // e.g. the pattern's own first line was already consumed by a `@@`
        // anchor immediately before it (the model repeated the anchor text as
        // the hunk's first context line, so the real match starts one line
        // earlier than `start` allows).
        return format!(
            "Every line of this pattern matches file content starting at line {}, but not at \
             or after line {} where the search began -- the pattern likely repeats a line \
             already consumed by an earlier `@@` context.",
            best_start + 1,
            start + 1
        );
    }
    let mismatch_offset = (0..pattern.len())
        .find(|&k| lines[best_start + k].trim() != pattern[k].trim())
        .unwrap_or(0);
    format!(
        "Nearest candidate is line {} ({score}/{} lines of the pattern already match there); \
         first disagreement is pattern line {} of {}: expected '{}', found '{}'.",
        best_start + 1,
        pattern.len(),
        mismatch_offset + 1,
        pattern.len(),
        clip(&pattern[mismatch_offset]),
        clip(&lines[best_start + mismatch_offset]),
    )
}

/// Finds the `pattern.len()`-line window of `lines` (searched from `start`
/// onward, matching where [`seek_sequence`] itself would have looked) that
/// agrees with `pattern` on the most lines (trim-insensitive, the loosest
/// tier `seek_sequence` allows), breaking ties toward the earliest window.
/// Returns `(start_index, score)`, or `None` if `pattern` cannot fit in
/// `lines` at all.
fn best_candidate_window(
    lines: &[String],
    pattern: &[String],
    start: usize,
) -> Option<(usize, usize)> {
    if pattern.len() > lines.len() {
        return None;
    }
    let last_start = lines.len() - pattern.len();
    let search_start = start.min(last_start);
    (search_start..=last_start)
        .map(|i| {
            let score = (0..pattern.len())
                .filter(|&k| lines[i + k].trim() == pattern[k].trim())
                .count();
            (i, score)
        })
        .max_by_key(|&(_, score)| score)
}

/// Heuristic check for a plain-diff hunk header (`-a,b +c,d` with an optional
/// trailing `@@`), the shape a model reaches for when it falls back to
/// familiar unified-diff syntax instead of this format's context lines. Not
/// exhaustive -- it only needs to catch the common case well enough to name
/// it in an error message, never to reject or accept a patch itself.
fn looks_like_unified_diff_hunk_header(s: &str) -> bool {
    let core = s
        .trim()
        .trim_start_matches("@@")
        .trim_end_matches("@@")
        .trim();
    let mut parts = core.split_whitespace();
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    fn is_range_token(tok: &str) -> bool {
        let tok = tok.trim_start_matches(['+', '-']);
        match tok.split_once(',') {
            Some((left, right)) => {
                !left.is_empty()
                    && !right.is_empty()
                    && left.chars().all(|c| c.is_ascii_digit())
                    && right.chars().all(|c| c.is_ascii_digit())
            }
            None => false,
        }
    }
    (a.starts_with('-') || a.starts_with('+'))
        && (b.starts_with('-') || b.starts_with('+'))
        && is_range_token(a)
        && is_range_token(b)
}

#[cfg(test)]
mod tests {
    use super::describe_mismatch;
    use super::looks_like_unified_diff_hunk_header;
    use super::seek_sequence;
    use std::string::ToString;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    /// A hint echoes a file line and a pattern line cut to a bound, at a
    /// character boundary, and looks for a partial-line hit only from where
    /// the search began.
    #[test]
    fn hints_clip_long_lines_and_scan_from_the_start_index() {
        let long = format!("needle {}", "\u{e9}".repeat(100_000));
        let lines = vec![long.clone(), "other".to_string(), "tail".to_string()];
        let hint = describe_mismatch(&lines, &to_vec(&["needle"]), 0);
        assert!(hint.len() < 1000, "{} bytes", hint.len());
        assert!(hint.contains("more characters]"), "{hint}");
        // The same partial hit before the start index is not offered.
        let hint = describe_mismatch(&lines, &to_vec(&["needle"]), 1);
        assert!(!hint.contains("contains this text"), "{hint}");
        // Multi-line: the disagreeing lines are clipped too.
        let lines = vec![long.clone(), long.clone()];
        let hint = describe_mismatch(&lines, &to_vec(&["x", "y"]), 0);
        assert!(hint.len() < 1000, "{} bytes", hint.len());
        // Short lines are echoed whole.
        let hint = describe_mismatch(&to_vec(&["pub fn foo(x: i32) {"]), &to_vec(&["fn foo"]), 0);
        assert!(hint.contains("'pub fn foo(x: i32) {'"), "{hint}");
    }

    #[test]
    fn test_exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["bar", "baz"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(1));
    }

    #[test]
    fn test_rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn test_trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn test_pattern_longer_than_input_returns_none() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        // Should not panic, must return None when pattern cannot possibly fit.
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), None);
    }

    #[test]
    fn test_unicode_punctuation_normalizes_to_ascii() {
        let lines = to_vec(&["import asyncio  \u{2013} avoids top\u{2011}level dep"]);
        let pattern = to_vec(&["import asyncio  - avoids top-level dep"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    // --- describe_mismatch --------------------------------------------------
    // Fixtures below are the three failing `@@`/old_lines patches
    // `claude-haiku-4-5-20251001` actually produced against the
    // `refactor-parser` bench brief (captured with `KGRAF_BENCH_KEEP_DIR`).
    // Each regression pins the specific
    // near-miss that burned the model's step budget without a correction
    // signal, so a future change to `describe_mismatch` cannot silently stop
    // naming it.

    /// Attempt 1: `@@ pub fn tally_csv_ints` against a file whose real line is
    /// `pub fn tally_csv_ints(line: &str) -> i64 {` -- a truncated `@@`
    /// anchor that is a substring of a real line but not the whole line.
    #[test]
    fn names_a_context_line_that_is_only_a_substring_of_a_real_line() {
        let lines = to_vec(&[
            "//! docs",
            "",
            "pub fn tally_csv_ints(line: &str) -> i64 {",
            "    0",
            "}",
        ]);
        let pattern = to_vec(&["pub fn tally_csv_ints"]);
        let msg = describe_mismatch(&lines, &pattern, 0);
        assert!(
            msg.contains("Line 3"),
            "must name the real line number: {msg}"
        );
        assert!(
            msg.contains("pub fn tally_csv_ints(line: &str) -> i64 {"),
            "must quote the real line's full text: {msg}"
        );
        assert!(
            msg.contains("whole line"),
            "must explain the whole-line rule: {msg}"
        );
    }

    /// Attempt 2: `@@ -5,11 +5,15 @@`, a plain unified-diff hunk header used
    /// as a `@@` context line.
    #[test]
    fn names_a_unified_diff_hunk_header_used_as_a_context_line() {
        let lines = to_vec(&["one", "two", "three"]);
        let pattern = to_vec(&["-5,11 +5,15 @@"]);
        let msg = describe_mismatch(&lines, &pattern, 0);
        assert!(
            msg.contains("unified-diff hunk header"),
            "must name the mistake by kind: {msg}"
        );
    }

    #[test]
    fn unified_diff_hunk_header_detector_matches_common_shapes() {
        assert!(looks_like_unified_diff_hunk_header("-5,11 +5,15 @@"));
        assert!(looks_like_unified_diff_hunk_header("-1,3 +1,3"));
        assert!(looks_like_unified_diff_hunk_header("@@ -1,3 +1,3 @@"));
        assert!(!looks_like_unified_diff_hunk_header(
            "pub fn tally_csv_ints"
        ));
        assert!(!looks_like_unified_diff_hunk_header(""));
        assert!(!looks_like_unified_diff_hunk_header("- not a range"));
    }

    /// Attempt 3: an old_lines block whose first line duplicates the `@@`
    /// anchor text immediately before it, so every line of the pattern
    /// matches the file loosely, just one line earlier than the search
    /// window `line_index` (set to just after the anchor) allows.
    #[test]
    fn names_a_pattern_that_only_matches_one_line_before_the_search_window() {
        let lines = to_vec(&["anchor", "body-1", "body-2"]);
        let pattern = to_vec(&["anchor", "body-1", "body-2"]);
        // `start` is 1, as if a `@@ anchor` context line had just been
        // matched at index 0 and consumed by `line_index = idx + 1`, but the
        // model repeated `anchor` as the pattern's own first line too.
        let msg = describe_mismatch(&lines, &pattern, 1);
        assert!(
            msg.contains("line 1") && msg.contains("line 2"),
            "must name both the real match position and where the search began: {msg}"
        );
        assert!(
            msg.contains("already consumed"),
            "must explain the repeated-anchor shape: {msg}"
        );
    }

    /// When more than one window could hold the pattern, the diagnostic must
    /// pick the window that agrees on the most lines, not merely the first
    /// one it can fit -- otherwise "nearest candidate" would be a
    /// mislabelled arbitrary guess.
    #[test]
    fn picks_the_window_with_the_most_agreement_not_the_first_that_fits() {
        let lines = to_vec(&["totally-wrong", "also-wrong", "keep", "close", "keep2"]);
        // Fits at index 0 (0/3 agree) and at index 2 (2/3 agree); the
        // diagnostic must report index 2, not index 0.
        let pattern = to_vec(&["keep", "expected", "keep2"]);
        let msg = describe_mismatch(&lines, &pattern, 0);
        assert!(msg.contains("line 3"), "must pick the closer window: {msg}");
        assert!(
            msg.contains("2/3"),
            "must report its real agreement count: {msg}"
        );
    }

    /// A genuine one-line difference (not a repeat, not a substring, not a
    /// diff-header): the diagnostic must still narrow to the exact
    /// disagreement rather than only reporting failure.
    #[test]
    fn names_the_first_disagreeing_line_in_an_otherwise_close_match() {
        let lines = to_vec(&["keep", "actual", "keep2"]);
        let pattern = to_vec(&["keep", "expected", "keep2"]);
        let msg = describe_mismatch(&lines, &pattern, 0);
        assert!(msg.contains("pattern line 2"), "{msg}");
        assert!(msg.contains("expected 'expected'"), "{msg}");
        assert!(msg.contains("found 'actual'"), "{msg}");
    }

    /// When the best candidate window still agrees on none of the pattern's
    /// lines, the message must say so plainly (`0/N`) rather than imply a
    /// near miss that is not really there.
    #[test]
    fn reports_zero_agreement_when_nothing_is_close() {
        let lines = to_vec(&["x", "y", "z"]);
        let pattern = to_vec(&["totally", "unrelated"]);
        let msg = describe_mismatch(&lines, &pattern, 0);
        assert!(msg.contains("0/2"), "{msg}");
    }

    /// `pattern` longer than every candidate window in `lines`: there is no
    /// window to name at all.
    #[test]
    fn reports_no_region_when_pattern_cannot_fit() {
        let lines = to_vec(&["only one line"]);
        let pattern = to_vec(&["a", "b", "c"]);
        let msg = describe_mismatch(&lines, &pattern, 0);
        assert!(msg.contains("No"), "{msg}");
    }

    #[test]
    fn empty_pattern_or_lines_describe_nothing() {
        assert_eq!(describe_mismatch(&[], &to_vec(&["a"]), 0), "");
        assert_eq!(describe_mismatch(&to_vec(&["a"]), &[], 0), "");
    }
}
