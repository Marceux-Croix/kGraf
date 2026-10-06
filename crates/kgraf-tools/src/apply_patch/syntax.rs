//! A parse check for whitespace-significant files edited by a tolerant match.
//!
//! When a chunk was placed only after ignoring whitespace, [`super::reindent`]
//! writes it in the file's indentation. This is the second line of defense: a
//! Python or YAML file that parsed before the edit and does not parse after it
//! is refused before it is written, with the line of the first error.
//!
//! The check is comparative (refuse only when the original parsed), so a file
//! that already failed, or syntax the grammar does not know, never blocks an
//! edit. It runs only for a tolerant match: a patch whose lines match exactly
//! is the model's own deliberate edit, and a multi-step change may pass through
//! a state that does not parse.
//!
//! Python uses the tree-sitter grammar the graph indexer already builds (its
//! external scanner tracks INDENT and DEDENT, so an unexpected indent is an
//! ERROR node). YAML uses `serde_yaml_ng`, already in the workspace. A Makefile
//! has no cheap parser here; [`super::reindent`] alone covers it.

use std::path::Path;

/// A file kind with a parse check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Python,
    Yaml,
}

fn kind_of(path: &str) -> Option<Kind> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "py" | "pyi" | "pyw" => Some(Kind::Python),
        "yaml" | "yml" => Some(Kind::Yaml),
        _ => None,
    }
}

/// `Ok(None)` when `source` parses, `Ok(Some(line))` with the 1-based line of
/// the first error when it does not.
fn first_error(kind: Kind, source: &str) -> Result<Option<usize>, String> {
    match kind {
        Kind::Python => {
            if let Some(line) = python_indentation_error(source) {
                return Ok(Some(line));
            }
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_python::LANGUAGE.into())
                .map_err(|e| format!("the Python grammar could not be loaded: {e}"))?;
            let tree = parser
                .parse(source, None)
                .ok_or("the Python parser returned no tree")?;
            let root = tree.root_node();
            if !root.has_error() {
                return Ok(None);
            }
            Ok(Some(first_error_row(root) + 1))
        }
        Kind::Yaml => {
            use serde::Deserialize;
            for doc in serde_yaml_ng::Deserializer::from_str(source) {
                if let Err(e) = serde_yaml_ng::Value::deserialize(doc) {
                    return Ok(Some(e.location().map_or(1, |l| l.line())));
                }
            }
            Ok(None)
        }
    }
}

/// The 1-based line of the first indentation error CPython would raise
/// (`unexpected indent`, `expected an indented block`, `unindent does not
/// match any outer indentation level`), or `None`.
///
/// Needed beside the tree-sitter grammar because that grammar's scanner
/// accepts an indented first line, the exact corruption a whitespace-tolerant
/// write-back produced. This follows the tokenizer's rules: only the start of
/// a logical line counts (not a line inside brackets, a string, or after a
/// backslash), blank and comment-only lines are skipped, and a tab advances to
/// the next multiple of 8 columns.
pub(crate) fn python_indentation_error(source: &str) -> Option<usize> {
    let mut stack: Vec<usize> = vec![0];
    let mut opens_block = false;
    // Open brackets and an unfinished string carry a logical line on.
    let mut depth = 0usize;
    let mut string: Option<(char, bool)> = None; // (quote, triple)
    let mut continued = false;
    for (row, line) in source.lines().enumerate() {
        let at_start = depth == 0 && string.is_none() && !continued;
        continued = false;
        let mut chars = line.chars().peekable();
        if at_start {
            let mut col = 0usize;
            while let Some(&c) = chars.peek() {
                match c {
                    ' ' => col += 1,
                    '\t' => col = (col / 8 + 1) * 8,
                    '\x0c' => col = 0,
                    _ => break,
                }
                chars.next();
            }
            let rest: String = chars.clone().collect();
            let rest = rest.trim_end();
            if rest.is_empty() || rest.starts_with('#') {
                continue;
            }
            let top = *stack.last().unwrap_or(&0);
            if col > top {
                if !opens_block {
                    return Some(row + 1);
                }
                stack.push(col);
            } else {
                if opens_block {
                    return Some(row + 1);
                }
                while stack.last().is_some_and(|&t| t > col) {
                    stack.pop();
                }
                if stack.last() != Some(&col) {
                    return Some(row + 1);
                }
            }
        }
        // Scan the rest of the line for brackets, strings, comments and the
        // last significant character.
        let mut last = None;
        while let Some(c) = chars.next() {
            if let Some((q, triple)) = string {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    if !triple {
                        string = None;
                    } else if chars.peek() == Some(&q) {
                        chars.next();
                        if chars.peek() == Some(&q) {
                            chars.next();
                            string = None;
                        }
                    }
                }
                last = Some('"');
                continue;
            }
            match c {
                '#' => break,
                '\'' | '"' => {
                    let triple = chars.peek() == Some(&c) && {
                        let mut look = chars.clone();
                        look.next();
                        look.peek() == Some(&c)
                    };
                    if triple {
                        chars.next();
                        chars.next();
                    } else if chars.peek() == Some(&c) {
                        // An empty string `''`.
                        chars.next();
                        last = Some('"');
                        continue;
                    }
                    string = Some((c, triple));
                    last = Some('"');
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    last = Some(c);
                }
                ')' | ']' | '}' => {
                    depth = depth.saturating_sub(1);
                    last = Some(c);
                }
                '\\' if chars.peek().is_none() => continued = true,
                c if c.is_whitespace() => {}
                c => last = Some(c),
            }
        }
        // A single-quoted string does not span lines (unless the line ends in
        // a backslash, which the escape above already consumed).
        if let Some((_, false)) = string
            && !line.ends_with('\\')
        {
            string = None;
        }
        if depth == 0 && string.is_none() && !continued {
            opens_block = last == Some(':');
        }
    }
    None
}

/// The row of the first ERROR or MISSING node under `node`, in document order.
fn first_error_row(node: tree_sitter::Node<'_>) -> usize {
    let mut cursor = node.walk();
    loop {
        let n = cursor.node();
        if n.is_error() || n.is_missing() {
            return n.start_position().row;
        }
        // Descend only into a subtree that holds an error.
        if n.has_error() && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return node.start_position().row;
            }
        }
    }
}

/// `Err` with a refusal when `path` is a file kind with a parse check, `before`
/// parses and `after` does not.
pub(crate) fn refuse_if_newly_broken(path: &str, before: &str, after: &str) -> Result<(), String> {
    let Some(kind) = kind_of(path) else {
        return Ok(());
    };
    let name = match kind {
        Kind::Python => "Python",
        Kind::Yaml => "YAML",
    };
    if first_error(kind, before)?.is_some() {
        return Ok(());
    }
    match first_error(kind, after)? {
        None => Ok(()),
        Some(line) => Err(format!(
            "In {path}, the patch matched only after ignoring whitespace, and the result is no \
             longer valid {name} (first error at line {line}), so it was not written. Copy the \
             unchanged and `-` lines with the file's exact indentation and send the patch again."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unexpected_indent_in_python_is_an_error() {
        assert_eq!(
            first_error(Kind::Python, "def f():\n    return 1\n").expect("runs"),
            None
        );
        assert_eq!(
            first_error(Kind::Python, " \"\"\"doc\"\"\"\n\nclass A:\n    pass\n").expect("runs"),
            Some(1)
        );
        assert!(
            first_error(Kind::Python, "def f():\n    a = 1\n      b = 2\n")
                .expect("runs")
                .is_some()
        );
    }

    /// Lines that are not the start of a logical line carry no indentation:
    /// inside brackets, inside a triple-quoted string, after a backslash; a
    /// comment-only or blank line is skipped too.
    #[test]
    fn the_indentation_check_follows_logical_lines() {
        // One element per line: a `\`-continued literal would eat the
        // leading whitespace this test is about.
        let ok = [
            "import os",
            "",
            "X = {",
            "'a': 1,",
            "        'b': [",
            "2],",
            "}",
            "",
            "def f(a,",
            "      b):",
            "    '''doc",
            "unindented doc line",
            "    '''",
            "       # odd comment",
            "    s = \"x # not a comment:\"",
            "    t = 1 + \\",
            "2",
            "    if a: return b",
            "    if b:  # trailing comment",
            "        pass",
            "    return ''",
            "",
            "class A:",
            "    x: int = 0",
            "",
        ]
        .join("\n");
        assert_eq!(python_indentation_error(&ok), None);
        assert_eq!(first_error(Kind::Python, &ok).expect("runs"), None);
        // A missing block after a colon.
        assert_eq!(python_indentation_error("if x:\ny = 1\n"), Some(2));
        // An indent after a line that opens no block.
        assert_eq!(python_indentation_error("x = 1\n    y = 2\n"), Some(2));
        // An unindent to a level that was never opened.
        assert_eq!(
            python_indentation_error("def f():\n    if x:\n        a()\n  b()\n"),
            Some(4)
        );
        // The recorded corruption: the module's first line indented.
        assert_eq!(
            python_indentation_error(" \"\"\"Module.\"\"\"\n\n\nclass A:\n    pass\n"),
            Some(1)
        );
        // The real stub module parses, so the check is armed for it.
        assert_eq!(
            first_error(Kind::Python, include_str!("testdata/containers.py")).expect("runs"),
            None
        );
    }

    #[test]
    fn a_yaml_indentation_break_is_an_error() {
        assert_eq!(
            first_error(Kind::Yaml, "a:\n  b: 1\n  c: 2\n").expect("runs"),
            None
        );
        assert!(
            first_error(Kind::Yaml, "a:\n  b: 1\n    c: 2\n")
                .expect("runs")
                .is_some()
        );
        // Several documents in one file parse.
        assert_eq!(
            first_error(Kind::Yaml, "a: 1\n---\nb: 2\n").expect("runs"),
            None
        );
    }

    #[test]
    fn only_a_newly_broken_file_of_a_checked_kind_is_refused() {
        let bad = " x = 1\n";
        let good = "x = 1\n";
        let err = refuse_if_newly_broken("m.py", good, bad).unwrap_err();
        assert!(err.contains("no longer valid Python"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        // Already broken before: not this edit's doing.
        assert!(refuse_if_newly_broken("m.py", bad, bad).is_ok());
        // Not a checked kind.
        assert!(refuse_if_newly_broken("m.txt", good, bad).is_ok());
    }
}
