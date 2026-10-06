//! Writing a tolerant match back in the file's own whitespace.
//!
//! [`super::seek_sequence::seek`] can place a chunk only after ignoring
//! trailing whitespace, leading whitespace or typographic punctuation. The
//! chunk's replacement lines then still hold the patch's text, not the file's:
//!
//! - an unchanged (context) line written from the patch replaces the file's
//!   own line with the patch's copy of it, so its indentation, trailing
//!   whitespace and typographic characters silently become the patch's;
//! - an added (`+`) line is in the patch's indentation, which may be shifted
//!   from the file's.
//!
//! In a whitespace-significant file (Python, YAML, a Makefile) either one
//! corrupts the file while the call reports success. Here a context line is
//! always the file's own line (upstream codex keeps context lines the same way
//! in its line-ending-preserving path), and the added lines are moved by the
//! one indentation offset the matched lines show between the patch and the
//! file. When the matched lines do not share one offset, the chunk is refused:
//! which offset an added line was written against cannot be known.
//!
//! **Offset model.** A uniformly shifted copy of a block has, on every line,
//! `file = B + r` and `patch = P + r`, where `r` is the line's own nesting and
//! `B` and `P` are the file's and the patch's base indentation. For one pair
//! the longest common suffix of the two leading-whitespace strings is taken as
//! `r`, which leaves `(P, B)`. Every non-blank matched pair must give the same
//! `(P, B)`; an added line is then re-indented by replacing its leading `P`
//! with `B`. Blank and whitespace-only lines carry no indentation and are left
//! out of the measurement.

/// The leading whitespace of `line`, by the same rule `str::trim` uses.
fn leading(line: &str) -> &str {
    let end = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map_or(line.len(), |(i, _)| i);
    &line[..end]
}

/// The `(patch base, file base)` pair one matched pair of lines shows, or
/// `None` when either line is blank.
fn offset_of(patch: &str, file: &str) -> Option<(String, String)> {
    if patch.trim().is_empty() || file.trim().is_empty() {
        return None;
    }
    let (p, f) = (leading(patch), leading(file));
    let common = p
        .chars()
        .rev()
        .zip(f.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum::<usize>();
    Some((
        p[..p.len() - common].to_owned(),
        f[..f.len() - common].to_owned(),
    ))
}

/// `ws` in words: "1 space", "1 tab and 4 spaces".
fn describe(ws: &str) -> String {
    let count = |n: usize, one: &str, many: &str| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    let tabs = ws.chars().filter(|&c| c == '\t').count();
    let spaces = ws.chars().filter(|&c| c == ' ').count();
    let other = ws.chars().count() - tabs - spaces;
    let parts: Vec<String> = [
        count(tabs, "tab", "tabs"),
        count(spaces, "space", "spaces"),
        count(
            other,
            "other whitespace character",
            "other whitespace characters",
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.join(" and ")
}

/// How the patch's indentation stands to the file's, for a message.
fn describe_offset(patch: &str, file: &str) -> String {
    match (patch.is_empty(), file.is_empty()) {
        (true, true) => "the same indentation as the file".to_owned(),
        (true, false) => format!("{} less indentation than the file", describe(file)),
        (false, true) => format!("{} more indentation than the file", describe(patch)),
        (false, false) => format!(
            "{} of indentation where the file has {}",
            describe(patch),
            describe(file)
        ),
    }
}

/// The first 60 characters of `line`, trimmed, for a message.
fn quote(line: &str) -> String {
    let t = line.trim();
    match t.char_indices().nth(60) {
        Some((cut, _)) => format!("{}...", &t[..cut]),
        None => t.to_owned(),
    }
}

/// A chunk's replacement lines fitted to the file.
#[derive(Debug, PartialEq)]
pub(crate) struct Fitted {
    pub lines: Vec<String>,
    /// The note for the result when added lines were re-indented.
    pub shift_note: Option<String>,
}

/// One matched pair of lines and the offset it shows.
struct Measured {
    /// Index into the chunk's old lines.
    old: usize,
    is_context: bool,
    offset: (String, String),
}

/// `new` (the chunk's replacement lines) rewritten for a match of `old` at
/// `file_start` in `file`: every context line becomes the file's own line, and
/// every added line is moved by the matched lines' one indentation offset.
/// `context` pairs indices of `old` and `new` that are context lines (see
/// [`super::parser::UpdateFileChunk::context_line_indices`]); pairs past the
/// ends of `old` or `new` (the trimmed final empty line) are ignored.
///
/// Refused when the matched lines show more than one offset, or when an added
/// line is indented less than the patch's base indentation that must be
/// removed from it.
pub(crate) fn fit_to_file(
    path: &str,
    file: &[String],
    file_start: usize,
    old: &[String],
    new: &[String],
    context: &[(usize, usize)],
) -> Result<Fitted, String> {
    let matched = &file[file_start..file_start + old.len()];
    let context: Vec<(usize, usize)> = context
        .iter()
        .copied()
        .filter(|&(o, n)| o < old.len() && n < new.len())
        .collect();
    // Sets, not scans: a hunk can hold thousands of lines.
    let context_old: std::collections::HashSet<usize> = context.iter().map(|&(o, _)| o).collect();
    let is_context = |o: usize| context_old.contains(&o);

    let measured: Vec<Measured> = old
        .iter()
        .zip(matched)
        .enumerate()
        .filter_map(|(i, (p, f))| {
            offset_of(p, f).map(|offset| Measured {
                old: i,
                is_context: is_context(i),
                offset,
            })
        })
        .collect();
    let context_at: std::collections::HashMap<usize, usize> =
        context.iter().map(|&(o, n)| (n, o)).collect();
    let (patch_base, file_base) = match measured.first() {
        None => (String::new(), String::new()),
        Some(first) => {
            if let Some(other) = measured.iter().find(|m| m.offset != first.offset) {
                // A tab-indented file (Go, a Makefile) and a patch written
                // with spaces: one offset per level, not one per block.
                return match spaces_per_tab(old, matched) {
                    Some(width) => {
                        tabs_for_spaces(path, file_start, matched, new, &context_at, width)
                    }
                    None => Err(inconsistent(path, file_start, old, &measured, first, other)),
                };
            }
            first.offset.clone()
        }
    };

    let mut shifted = 0usize;
    let mut lines = Vec::with_capacity(new.len());
    for (n, line) in new.iter().enumerate() {
        if let Some(&o) = context_at.get(&n) {
            lines.push(matched[o].clone());
            continue;
        }
        if patch_base == file_base || line.trim().is_empty() {
            lines.push(line.clone());
            continue;
        }
        match line.strip_prefix(patch_base.as_str()) {
            Some(rest) => {
                lines.push(format!("{file_base}{rest}"));
                shifted += 1;
            }
            None => {
                return Err(format!(
                    "In {path}, the patch's lines matched at line {} only after ignoring \
                     indentation: they have {}. The added line '{}' has less indentation than \
                     that, so it cannot be moved to the file's indentation; the patch was not \
                     applied. Copy the unchanged and `-` lines with the file's exact indentation.",
                    file_start + 1,
                    describe_offset(&patch_base, &file_base),
                    quote(line)
                ));
            }
        }
    }
    let shift_note = (shifted > 0).then(|| {
        format!(
            "note: in {path}, the patch's lines at line {} have {}; its {shifted} added line{} \
             re-indented by the same amount to match the file, and its unchanged lines were kept \
             as the file has them",
            file_start + 1,
            describe_offset(&patch_base, &file_base),
            if shifted == 1 { " was" } else { "s were" },
        )
    });
    Ok(Fitted { lines, shift_note })
}

/// The number of spaces the patch wrote for each tab of the file, when every
/// matched non-blank pair is the file's tabs written as that many spaces per
/// tab (2 to 8), and at least one line is indented. A model asked to edit a
/// tab-indented file (gofmt's Go, a Makefile's recipes) writes spaces, so
/// the offset grows with the depth and no single offset fits.
fn spaces_per_tab(old: &[String], matched: &[String]) -> Option<usize> {
    let mut width = None;
    for (p, f) in old.iter().zip(matched) {
        if p.trim().is_empty() || f.trim().is_empty() {
            continue;
        }
        let (lp, lf) = (leading(p), leading(f));
        if !lp.chars().all(|c| c == ' ') || !lf.chars().all(|c| c == '\t') {
            return None;
        }
        let (spaces, tabs) = (lp.len(), lf.len());
        if tabs == 0 {
            if spaces != 0 {
                return None;
            }
            continue;
        }
        if !spaces.is_multiple_of(tabs) {
            return None;
        }
        let w = spaces / tabs;
        if !(2..=8).contains(&w) || width.is_some_and(|known| known != w) {
            return None;
        }
        width = Some(w);
    }
    width
}

/// `new` fitted to a tab-indented file whose matched lines the patch wrote
/// with `width` spaces per tab: context lines are the file's own, and each
/// added line's leading spaces become one tab per `width`. An added line
/// whose leading whitespace is not a whole number of tab widths is refused.
fn tabs_for_spaces(
    path: &str,
    file_start: usize,
    matched: &[String],
    new: &[String],
    context_at: &std::collections::HashMap<usize, usize>,
    width: usize,
) -> Result<Fitted, String> {
    let mut lines = Vec::with_capacity(new.len());
    let mut shifted = 0usize;
    for (n, line) in new.iter().enumerate() {
        if let Some(&o) = context_at.get(&n) {
            lines.push(matched[o].clone());
            continue;
        }
        let lead = leading(line);
        if line.trim().is_empty() || lead.is_empty() {
            lines.push(line.clone());
            continue;
        }
        if !lead.chars().all(|c| c == ' ') || !lead.len().is_multiple_of(width) {
            return Err(format!(
                "In {path}, the patch's lines matched at line {} with {width} spaces for each tab \
                 of the file. The added line '{}' is not indented by a whole number of {width} \
                 spaces, so it cannot be written with tabs; the patch was not applied. Indent the \
                 lines with tabs, as the file is.",
                file_start + 1,
                quote(line)
            ));
        }
        lines.push(format!(
            "{}{}",
            "\t".repeat(lead.len() / width),
            &line[lead.len()..]
        ));
        shifted += 1;
    }
    let shift_note = (shifted > 0).then(|| {
        format!(
            "note: in {path}, the patch's lines at line {} were indented with {width} spaces for \
             each tab of the file; its {shifted} added line{} written with tabs to match the \
             file, and its unchanged lines were kept as the file has them",
            file_start + 1,
            if shifted == 1 { " was" } else { "s were" },
        )
    });
    Ok(Fitted { lines, shift_note })
}

/// The refusal for matched lines that show two different offsets.
fn inconsistent(
    path: &str,
    file_start: usize,
    old: &[String],
    measured: &[Measured],
    first: &Measured,
    other: &Measured,
) -> String {
    let line = |m: &Measured| {
        format!(
            "file line {} ('{}') has {}",
            file_start + m.old + 1,
            quote(&old[m.old]),
            describe_offset(&m.offset.0, &m.offset.1)
        )
    };
    // The common slip: every unchanged line carries extra whitespace after its
    // ` ` prefix while every `-` line is exact. Named when the lines show
    // exactly that.
    let uniform = |want: bool| {
        let mut it = measured.iter().filter(|m| m.is_context == want);
        let head = it.next()?;
        it.all(|m| m.offset == head.offset)
            .then(|| head.offset.clone())
    };
    let hint = match (uniform(true), uniform(false)) {
        (Some((extra, base)), Some((p, f)))
            if base.is_empty() && !extra.is_empty() && p.is_empty() && f.is_empty() =>
        {
            format!(
                " Every unchanged line has {extra_words} more indentation than the file while \
                 every `-` line is exact: an unchanged line is a single ` ` prefix followed by \
                 the file's line as it is, with no extra whitespace after the prefix.",
                extra_words = describe(&extra)
            )
        }
        _ => String::new(),
    };
    format!(
        "In {path}, the patch's unchanged and `-` lines matched at line {} only after ignoring \
         indentation, and they are not shifted from the file by one amount: {}, but {}. The \
         indentation of the `+` lines cannot be known, so the patch was not applied. Copy the \
         lines with the file's exact indentation.{hint}",
        file_start + 1,
        line(first),
        line(other),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_offset_is_the_part_outside_the_common_nesting() {
        assert_eq!(
            offset_of("  a", "    a"),
            Some((String::new(), "  ".into()))
        );
        assert_eq!(
            offset_of("      a", "        a"),
            Some((String::new(), "  ".into()))
        );
        assert_eq!(offset_of(" a", "a"), Some((" ".into(), String::new())));
        assert_eq!(
            offset_of("    a", "\ta"),
            Some(("    ".into(), "\t".into()))
        );
        assert_eq!(
            offset_of("\ta", "\t\ta"),
            Some((String::new(), "\t".into()))
        );
        assert_eq!(offset_of("", "  a"), None);
        assert_eq!(offset_of("  a", "   "), None);
    }

    #[test]
    fn whitespace_is_described_in_words() {
        assert_eq!(describe(" "), "1 space");
        assert_eq!(describe("\t    "), "1 tab and 4 spaces");
        assert_eq!(describe("\t\t"), "2 tabs");
        assert_eq!(describe("\u{00A0}"), "1 other whitespace character");
    }

    #[test]
    fn a_uniform_shift_moves_added_lines_and_keeps_context_from_the_file() {
        let file = v(&["def f():", "    if x:", "        a()", "    b()"]);
        // The patch has the block at 2 less indentation; context at 0, `-` at 1.
        let old = v(&["  if x:", "      a()"]);
        let new = v(&["  if x:", "      c()", "      d()"]);
        let fitted = fit_to_file("m.py", &file, 1, &old, &new, &[(0, 0)]).expect("fits");
        assert_eq!(
            fitted.lines,
            v(&["    if x:", "        c()", "        d()"])
        );
        let note = fitted.shift_note.expect("noted");
        assert!(note.contains("2 spaces less indentation"), "{note}");
        assert!(note.contains("2 added lines were re-indented"), "{note}");
    }

    #[test]
    fn a_trailing_whitespace_or_punctuation_match_writes_the_file_s_context_line() {
        let file = v(&["x = \u{201C}a\u{201D}  ", "y = 1"]);
        let old = v(&["x = \"a\"", "y = 1"]);
        let new = v(&["x = \"a\"", "y = 2"]);
        let fitted = fit_to_file("f", &file, 0, &old, &new, &[(0, 0)]).expect("fits");
        assert_eq!(fitted.lines, v(&["x = \u{201C}a\u{201D}  ", "y = 2"]));
        assert_eq!(fitted.shift_note, None);
    }

    #[test]
    fn two_offsets_in_one_chunk_are_refused_with_both_lines_named() {
        let file = v(&["a:", "  b: 1", "  c: 2"]);
        // `a:` at the same indentation, `b` 2 spaces deeper than the file.
        let old = v(&["a:", "    b: 1"]);
        let new = v(&["a:", "    b: 3"]);
        let err = fit_to_file("x.yaml", &file, 0, &old, &new, &[(0, 0)]).unwrap_err();
        assert!(
            err.contains("not shifted from the file by one amount"),
            "{err}"
        );
        assert!(err.contains("file line 1 ('a:')"), "{err}");
        assert!(err.contains("file line 2 ('b: 1')"), "{err}");
        assert!(err.contains("not applied"), "{err}");
    }

    #[test]
    fn an_added_line_shallower_than_the_patch_base_is_refused() {
        let file = v(&["a", "b"]);
        let old = v(&["  a"]);
        let new = v(&["  a", "c"]);
        let err = fit_to_file("f", &file, 0, &old, &new, &[(0, 0)]).unwrap_err();
        assert!(err.contains("'c' has less indentation"), "{err}");
    }

    #[test]
    fn blank_lines_carry_no_offset_and_stay_as_the_file_has_them() {
        let file = v(&["  a", "", "  b"]);
        let old = v(&["a", "   ", "b"]);
        let new = v(&["a", "   ", "b", "c"]);
        let fitted =
            fit_to_file("f", &file, 0, &old, &new, &[(0, 0), (1, 1), (2, 2)]).expect("fits");
        assert_eq!(fitted.lines, v(&["  a", "", "  b", "  c"]));
    }
}
