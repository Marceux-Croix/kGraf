//! Placeholder bodies in source: code that stands for work not done yet
//! (`todo!()`, `unimplemented!()`, `raise NotImplementedError`,
//! `throw new Error("not implemented")`).
//!
//! A placeholder is found only in code. The same words inside a string
//! literal, a raw string, a character literal, a doc comment or a comment are
//! text about a placeholder, not one: a file whose prose says "`todo!()`
//! panics" holds no unfinished body. Every check that asks "does this file
//! hold a placeholder" reads [`has_placeholder_body`], and every check that
//! needs source with its text removed reads [`code_only`], so the rule is one
//! rule.
//!
//! The lexing is tree-sitter's, with the grammars `kgraf-graph` already
//! compiles for indexing (Rust, Python, TypeScript, JavaScript, Go, Java, C,
//! C++, Ruby, C#, PHP). A grammar knows raw strings (`r#"..."#`), byte
//! strings, nested block comments, lifetimes against character literals and
//! Python's triple-quoted and prefixed strings, which a hand-written scan
//! gets wrong at the edges. tree-sitter recovers from a syntax error with an
//! error node and goes on, so a file in the middle of an edit is still lexed.
//!
//! Cost, measured in a release build on an Apple-silicon laptop: one lex of a
//! 34 KB Rust file takes about 6 ms, of a 397 KB file about 50 ms (a
//! hand-written byte scan took well under 1 ms). The callers lex a file only
//! when a plan names it, when a producer's write is read for a derived
//! interface, or when a declared write was left untouched by an attempt; a
//! planner call takes tens of seconds, so the lex is not the bottleneck.
//!
//! A file in a language with no grammar is read line by line: a line that
//! starts as a comment does not count. That reader cannot tell a string from
//! code. A prose file (Markdown, plain text) holds no code body at all.

use kgraf_graph::Language;
use std::ops::Range;

/// The macros whose call is a placeholder body. `unreachable!` is not one: it
/// states an invariant, and a mock of a method nobody calls is made of it.
const PLACEHOLDER_MACROS: [&str; 2] = ["todo", "unimplemented"];

/// The words that raise a message, in the languages kGraf reads: a "not
/// implemented" message after one of them is a placeholder body.
const RAISE_WORDS: [&str; 4] = ["raise", "throw", "panic", "fail"];

/// The exception whose raise is a placeholder body (Python, Ruby).
const NOT_IMPLEMENTED_ERROR: &str = "NotImplementedError";

/// Extensions of prose files: no code body, so no placeholder.
const PROSE_EXTENSIONS: [&str; 5] = ["md", "markdown", "txt", "rst", "adoc"];

/// Source with its text removed, and where the string literals were.
struct Lexed {
    /// The source, same byte length, with every comment and literal replaced
    /// by spaces (newlines kept).
    code: String,
    /// The byte ranges of the string and character literals (not comments).
    literals: Vec<Range<usize>>,
}

/// `text` with the inside of every comment and every string, raw string,
/// byte string, character, template and regex literal replaced by spaces,
/// newlines kept, so offsets and line numbers match the source. `None` when
/// kGraf has no grammar for `path` (or it is Markdown, which has no code).
pub fn code_only(path: &str, text: &str) -> Option<String> {
    lex(path, text).map(|l| l.code)
}

/// Whether `text`, the content of `path`, holds a placeholder body in code:
///
/// - a call of `todo!` or `unimplemented!` with any delimiter, the name
///   standing alone or at the end of a path (`std::todo!`), not part of a
///   longer name (`my_todo!` is another macro);
/// - `raise NotImplementedError` (or `fail NotImplementedError`), with or
///   without arguments;
/// - a string literal that says "not implemented" (any case) as the message
///   of a `raise`, `throw`, `panic` or `fail` on the same line, or on the line
///   before when the literal opens its own line.
///
/// None of these counts inside a comment or a literal. A file with no
/// grammar is read line by line ([`line_has_placeholder`]).
pub fn has_placeholder_body(path: &str, text: &str) -> bool {
    if is_prose(path) {
        return false;
    }
    match lex(path, text) {
        Some(l) => {
            code_has_placeholder(&l.code)
                || l.literals
                    .iter()
                    .any(|r| is_raised_not_implemented(text, &l.code, r))
        }
        None => text.lines().any(line_has_placeholder),
    }
}

fn is_prose(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| PROSE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Lexes `text` by the grammar for `path`. A grammar that does not load or a
/// parse that returns nothing gives `None`: the caller falls back to the line
/// reader, and `every_grammar_kgraf_reads_lexes_its_text_out` keeps the
/// grammars loading.
fn lex(path: &str, text: &str) -> Option<Lexed> {
    let language = Language::from_path(path).filter(|l| *l != Language::Markdown)?;
    let grammar = language.grammar().ok()?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(text, None)?;

    // Local mutation: one byte buffer blanked in place in a single pre-order
    // walk, rather than a string rebuilt per node.
    let mut code = text.as_bytes().to_vec();
    let mut literals = Vec::new();
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        let text_node = node.is_named() && is_text_kind(node.kind());
        if text_node {
            let range = node.byte_range();
            if !node.kind().contains("comment") {
                literals.push(range.clone());
            }
            for b in &mut code[range] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
        // A text node is blanked whole; its children are not visited.
        if !text_node && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    // Whole nodes start and end on character boundaries, so blanking them
    // byte by byte keeps the buffer UTF-8.
    let code = String::from_utf8(code).ok()?;
    Some(Lexed { code, literals })
}

/// Whether a named node of this kind is text rather than code, across the
/// grammars: every comment kind (`line_comment`, `block_comment`, `comment`),
/// every string kind (`string_literal`, `raw_string_literal`, `string`,
/// `template_string`, `interpreted_string_literal`, `encapsed_string`,
/// `verbatim_string_literal`, `interpolated_string_expression`), character
/// literals and the heredoc, text-block and regex literals.
fn is_text_kind(kind: &str) -> bool {
    kind.contains("comment")
        || kind.contains("string")
        || matches!(
            kind,
            "char_literal"
                | "character_literal"
                | "rune_literal"
                | "heredoc"
                | "heredoc_body"
                | "nowdoc"
                | "text_block"
                | "regex"
        )
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The offsets where `word` stands alone in `code` (no identifier byte on
/// either side).
fn word_at(code: &str, word: &str) -> impl Iterator<Item = usize> {
    let bytes = code.as_bytes();
    code.match_indices(word)
        .map(|(at, _)| at)
        .filter(move |&at| {
            let end = at + word.len();
            (at == 0 || !is_ident_byte(bytes[at - 1]))
                && (end >= bytes.len() || !is_ident_byte(bytes[end]))
        })
}

/// Whether `code` (text already removed) holds a placeholder macro call or a
/// raise of `NotImplementedError`.
fn code_has_placeholder(code: &str) -> bool {
    let macro_call = PLACEHOLDER_MACROS.iter().any(|name| {
        word_at(code, name).any(|at| {
            let rest = code[at + name.len()..].trim_start();
            rest.strip_prefix('!')
                .map(str::trim_start)
                .is_some_and(|rest| rest.starts_with(['(', '[', '{']))
        })
    });
    macro_call
        || word_at(code, NOT_IMPLEMENTED_ERROR).any(|at| {
            let before = code[..at].trim_end();
            ["raise", "fail"].iter().any(|kw| {
                before
                    .strip_suffix(kw)
                    .is_some_and(|head| head.as_bytes().last().is_none_or(|b| !is_ident_byte(*b)))
            })
        })
}

/// Whether the literal at `range` says "not implemented" and is the message
/// of a raise in the code before it: on its own line, or on the line before
/// when only openers (`(`, `,`, whitespace) precede it on its own line.
fn is_raised_not_implemented(text: &str, code: &str, range: &Range<usize>) -> bool {
    let says = text
        .get(range.clone())
        .is_some_and(|t| t.to_ascii_lowercase().contains("not implemented"));
    if !says {
        return false;
    }
    let line_start = code[..range.start].rfind('\n').map_or(0, |n| n + 1);
    let mut before = &code[line_start..range.start];
    if before
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '(' | ','))
        .is_empty()
        && line_start > 0
    {
        let prev_start = code[..line_start - 1].rfind('\n').map_or(0, |n| n + 1);
        before = &code[prev_start..range.start];
    }
    RAISE_WORDS
        .iter()
        .any(|w| word_at(before, w).next().is_some())
}

/// The reader for a language with no grammar: a line that starts as a comment
/// (`//`, `#`, `/*`, `*`, `--`) does not count; on any other line the code
/// rules apply to the whole line, and a "not implemented" message counts when
/// a raise word is on the same line.
fn line_has_placeholder(line: &str) -> bool {
    let t = line.trim_start();
    if ["//", "#", "/*", "*", "--"]
        .iter()
        .any(|c| t.starts_with(c))
    {
        return false;
    }
    code_has_placeholder(t)
        || (t.to_ascii_lowercase().contains("not implemented")
            && RAISE_WORDS.iter().any(|w| word_at(t, w).next().is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like `crates/kgraf-host/src/scoped_check.rs`, which a plan was refused for: the
    /// macro is named in a string, a raw string, a byte string, a character
    /// context, a doc comment, a line comment and a nested block comment, and
    /// no body is a placeholder.
    const PROSE_ONLY: &str = r###"//! A check. `todo!()` in a module doc is prose.

/// Doc: `unimplemented!()` panics.
pub fn brief() -> &'static str {
    "Acceptance line of your contract is advisory. `todo!()` panics or failing tests \
     in other contracts' unfinished code are not yours to fix."
}

pub fn raw() -> &'static str {
    r#"a raw string: todo!() and "quoted" unimplemented!{}"#
}

pub fn bytes() -> &'static [u8] {
    b"todo!()"
}

pub fn chars<'a>(x: &'a str) -> (char, &'a str) {
    ('t', x) // todo!() in a trailing comment
}

/* outer /* nested todo!() */ still a comment: unimplemented!() */
pub fn message() -> String {
    format!("raise NotImplementedError is {} not implemented", 1)
}
"###;

    #[test]
    fn a_placeholder_named_only_in_text_is_not_a_placeholder() {
        assert!(!has_placeholder_body("src/scoped_check.rs", PROSE_ONLY));
    }

    #[test]
    fn a_real_todo_body_next_to_the_same_text_is_still_found() {
        let with_body = format!("{PROSE_ONLY}\npub fn run() -> u8 {{\n    todo!()\n}}\n");
        assert!(has_placeholder_body("src/scoped_check.rs", &with_body));
    }

    #[test]
    fn every_rust_form_of_a_placeholder_call_is_found() {
        for body in [
            "todo!()",
            "todo!(\"discount contract: implement\")",
            "todo![]",
            "todo!{}",
            "unimplemented!()",
            "unimplemented!(\"x\")",
            "std::todo!()",
            "::core::unimplemented!()",
            "todo ! ()",
            "let x = todo!();",
            "vec![todo!()]",
        ] {
            let src = format!("pub fn f() -> u8 {{ {body} }}\n");
            assert!(has_placeholder_body("a/src/lib.rs", &src), "{src}");
        }
        // Another macro whose name ends in the word, or `unreachable!`, is not one.
        for body in [
            "my_todo!()",
            "unreachable!()",
            "todo()",
            "let todo = 1; todo",
        ] {
            let src = format!("pub fn f() -> u8 {{ {body} }}\n");
            assert!(!has_placeholder_body("a/src/lib.rs", &src), "{src}");
        }
    }

    #[test]
    fn a_rust_file_in_the_middle_of_an_edit_is_still_lexed() {
        // Unbalanced braces: tree-sitter recovers, the body is still found and
        // the string is still text.
        assert!(has_placeholder_body(
            "a/src/lib.rs",
            "pub fn f() -> u8 {\n    todo!()\n"
        ));
        assert!(!has_placeholder_body(
            "a/src/lib.rs",
            "pub fn f() -> &'static str {\n    \"todo!()\"\n"
        ));
    }

    #[test]
    fn python_placeholders_are_found_in_code_and_not_in_text() {
        for src in [
            "def f():\n    raise NotImplementedError\n",
            "def f():\n    raise NotImplementedError(\"later\")\n",
            "class A:\n    def f(self):\n        raise NotImplementedError()\n",
            "def f():\n    raise ValueError(\"Not implemented yet\")\n",
        ] {
            assert!(has_placeholder_body("pkg/mod.py", src), "{src}");
        }
        for src in [
            "def f():\n    \"\"\"Docstring: raise NotImplementedError until done.\"\"\"\n    return 1\n",
            "def f():\n    return '''\nraise NotImplementedError\n'''\n",
            "def f():\n    return r\"raise NotImplementedError\"\n",
            "def f():\n    return f\"raise NotImplementedError {1}\"\n",
            "def f():\n    return b'raise NotImplementedError'\n",
            "def f():  # raise NotImplementedError\n    return 1\n",
            "def f():\n    print(\"not implemented\")\n    return 1\n",
            // Catching the error is not raising it.
            "try:\n    g()\nexcept NotImplementedError:\n    pass\n",
        ] {
            assert!(!has_placeholder_body("pkg/mod.py", src), "{src}");
        }
    }

    #[test]
    fn a_not_implemented_message_counts_only_when_raised_in_code() {
        for (path, src) in [
            (
                "web/a.ts",
                "export function f(): number {\n  throw new Error(\"not implemented\");\n}\n",
            ),
            (
                "web/a.js",
                "function f() {\n  throw new Error(\n    'Not implemented'\n  );\n}\n",
            ),
            (
                "svc/a.go",
                "package a\n\nfunc F() int {\n\tpanic(\"not implemented\")\n}\n",
            ),
            (
                "src/A.java",
                "class A { int f() { throw new UnsupportedOperationException(\"Not implemented\"); } }\n",
            ),
            ("lib/a.rb", "def f\n  raise NotImplementedError\nend\n"),
            ("src/lib.rs", "fn f() { panic!(\"not implemented\") }\n"),
        ] {
            assert!(has_placeholder_body(path, src), "{path}: {src}");
        }
        for (path, src) in [
            (
                "web/a.ts",
                "// throw new Error(\"not implemented\")\nexport const x = 1;\n",
            ),
            (
                "web/a.ts",
                "export const help = \"throw: not implemented\";\n",
            ),
            (
                "svc/a.go",
                "package a\n\n// panic(\"not implemented\")\nfunc F() int { return 1 }\n",
            ),
            ("lib/a.rb", "# raise NotImplementedError\ndef f\n  1\nend\n"),
        ] {
            assert!(!has_placeholder_body(path, src), "{path}: {src}");
        }
    }

    #[test]
    fn a_file_without_a_grammar_is_read_line_by_line() {
        assert!(has_placeholder_body("a/b.kt", "fun f(): Int = todo!()\n"));
        assert!(has_placeholder_body(
            "a/b.swift",
            "fatalError(); throw E(\"not implemented\")\n"
        ));
        assert!(!has_placeholder_body("a/b.kt", "// todo!()\nfun f() = 1\n"));
        assert!(!has_placeholder_body("a/b.sql", "-- todo!()\nselect 1;\n"));
    }

    #[test]
    fn a_prose_file_holds_no_placeholder() {
        for path in ["README.md", "docs/notes.txt", "docs/a.rst", "docs/a.MD"] {
            assert!(
                !has_placeholder_body(path, "fn f() { todo!() }\n"),
                "{path}"
            );
        }
    }

    /// Every grammar loads and removes its own comments and strings: a grammar
    /// that failed to load would fall back to the line reader unseen.
    #[test]
    fn every_grammar_kgraf_reads_lexes_its_text_out() {
        for (path, src, text) in [
            ("a.rs", "let s = r#\"SECRET\"#; // SECRET\n", "SECRET"),
            ("a.py", "s = '''SECRET'''  # SECRET\n", "SECRET"),
            ("a.ts", "const s = `SECRET`; // SECRET\n", "SECRET"),
            ("a.tsx", "const s = 'SECRET'; /* SECRET */\n", "SECRET"),
            ("a.js", "const s = /SECRET/; // SECRET\n", "SECRET"),
            ("a.go", "package a\nvar s = `SECRET` // SECRET\n", "SECRET"),
            (
                "A.java",
                "class A { String s = \"SECRET\"; } // SECRET\n",
                "SECRET",
            ),
            ("a.c", "char *s = \"SECRET\"; /* SECRET */\n", "SECRET"),
            ("a.cpp", "auto s = R\"(SECRET)\"; // SECRET\n", "SECRET"),
            ("a.rb", "s = \"SECRET\" # SECRET\n", "SECRET"),
            (
                "a.cs",
                "class A { string s = @\"SECRET\"; } // SECRET\n",
                "SECRET",
            ),
            ("a.php", "<?php $s = 'SECRET'; // SECRET\n", "SECRET"),
        ] {
            let code = code_only(path, src).unwrap_or_else(|| panic!("{path}: no grammar"));
            assert_eq!(code.len(), src.len(), "{path}");
            assert!(!code.contains(text), "{path}: {code:?}");
        }
        assert!(code_only("README.md", "x").is_none());
        assert!(code_only("a.kt", "x").is_none());
    }

    #[test]
    fn rust_lifetimes_stay_code_and_char_literals_are_text() {
        let code = code_only("a.rs", "fn f<'a>(x: &'a str) -> char { '{' }\n").expect("rust");
        assert!(code.contains("<'a>"), "{code}");
        assert!(code.contains("&'a str"), "{code}");
        assert!(!code.contains("'{'"), "{code}");
    }
}
