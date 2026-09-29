//! A tolerant structural read of Rust source for [`Check::CargoTestUses`]
//! (`crate::brief`): does one function's body call a helper and no longer
//! hold the old logic inline?
//!
//! This is a scan, not a parser. It blanks comments and string and char
//! literals, finds `fn <name>`, matches braces, and looks for the helper's name in
//! the body (or the helper passed by name). That is enough to tell a refactor from a helper nothing calls;
//! it does not prove the call is reached.

/// Blank out comments, string literals and char literals in Rust `src`,
/// keeping every other character and all newlines, so a scan for calls and
/// braces sees code only. An unterminated literal blanks to the end.
pub fn strip_non_code(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = b.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for c in &mut out[from..to.min(b.len())] {
            if *c != '\n' {
                *c = ' ';
            }
        }
    };
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < b.len() {
        let at = |n: usize| b.get(i + n).copied();
        match b[i] {
            '/' if at(1) == Some('/') => {
                let end = b[i..]
                    .iter()
                    .position(|&c| c == '\n')
                    .map_or(b.len(), |p| i + p);
                blank(&mut out, i, end);
                i = end;
            }
            '/' if at(1) == Some('*') => {
                // Block comments nest in Rust.
                let (mut depth, mut j) = (1, i + 2);
                while j < b.len() && depth > 0 {
                    if b[j] == '/' && b.get(j + 1) == Some(&'*') {
                        depth += 1;
                        j += 2;
                    } else if b[j] == '*' && b.get(j + 1) == Some(&'/') {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i, j);
                i = j;
            }
            'r' if matches!(at(1), Some('"' | '#')) && !(i > 0 && is_ident(b[i - 1])) => {
                let hashes = b[i + 1..].iter().take_while(|&&c| c == '#').count();
                if b.get(i + 1 + hashes) != Some(&'"') {
                    i += 1;
                    continue;
                }
                let start = i + 2 + hashes;
                let close: Vec<char> = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let end = (start..b.len())
                    .find(|&j| b[j..].starts_with(&close))
                    .map_or(b.len(), |j| j + close.len());
                blank(&mut out, i, end);
                i = end;
            }
            '"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != '"' {
                    j += if b[j] == '\\' { 2 } else { 1 };
                }
                blank(&mut out, i, j + 1);
                i = j + 1;
            }
            '\'' => {
                // A char literal ('x' or '\n'); anything else is a lifetime.
                let end = if at(1) == Some('\\') {
                    (i + 2..(i + 12).min(b.len()))
                        .find(|&j| b[j] == '\'')
                        .map(|j| j + 1)
                } else if at(2) == Some('\'') {
                    Some(i + 3)
                } else {
                    None
                };
                match end {
                    Some(end) => {
                        blank(&mut out, i, end);
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    out.into_iter().collect()
}

/// The body (between the braces, exclusive) of the first `fn <name>` in
/// already-stripped code, or `None` when there is none or its braces do not
/// balance.
fn fn_body<'a>(code: &'a str, name: &str) -> Option<&'a str> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    for (at, _) in code.match_indices(name) {
        let before = code[..at].trim_end();
        let is_fn_name = before
            .strip_suffix("fn")
            .is_some_and(|p| !p.chars().next_back().is_some_and(is_ident))
            && !code[at + name.len()..].chars().next().is_some_and(is_ident);
        if !is_fn_name {
            continue;
        }
        let open = at + code[at..].find('{')?;
        let mut depth = 0usize;
        for (j, c) in code[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&code[open + 1..open + j]);
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    None
}

/// True when `function`'s body in `source` uses `helper` (a call, a path call
/// such as `Self::helper(..)`, or the name passed on; a comment or string
/// mention does not count) and contains none
/// of `absent`. `absent` fragments are compared with whitespace removed, so
/// formatting cannot hide inlined logic.
pub fn body_uses(source: &str, function: &str, helper: &str, absent: &[&str]) -> bool {
    let code = strip_non_code(source);
    let Some(body) = fn_body(&code, function) else {
        return false;
    };
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    // A whole-identifier occurrence is a use: a call, a path call, or the
    // helper passed by name (`.map(helper)`).
    let called = body.match_indices(helper).any(|(i, _)| {
        !body[..i].chars().next_back().is_some_and(is_ident)
            && !body[i + helper.len()..]
                .chars()
                .next()
                .is_some_and(is_ident)
    });
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let body = squash(body);
    called && absent.iter().all(|a| !body.contains(&squash(a)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brief::{Check, suite};

    fn uses_of(id: &str) -> (&'static str, &'static str, &'static [&'static str]) {
        let brief = suite().into_iter().find(|b| b.id == id).expect("brief");
        match brief.check {
            Check::CargoTestUses {
                function,
                helper,
                absent,
                ..
            } => (function, helper, absent),
            other => panic!("{id} has {other:?}, not a structural check"),
        }
    }

    const SHIPPING_GOOD: &str = "pub fn apply_discount(price: f64, pct_off: f64) -> f64 {\n    price - price * pct_off / 100.0\n}\npub fn total_price(price: f64, qty: u32, pct_off: f64) -> f64 {\n    apply_discount(price, pct_off) * qty as f64\n}\n";
    const PARSER_GOOD: &str = "pub fn parse_int_field(field: &str) -> i64 {\n    field.trim().parse().unwrap_or(0)\n}\npub fn tally_csv_ints(line: &str) -> i64 {\n    line.split(',').map(parse_int_field).sum()\n}\n";

    #[test]
    fn a_refactor_that_calls_the_helper_passes() {
        let (f, h, a) = uses_of("refactor-shipping");
        assert!(body_uses(SHIPPING_GOOD, f, h, a));
        let (f, h, a) = uses_of("refactor-parser");
        // Passing the helper by name and calling it in a closure both count.
        assert!(body_uses(PARSER_GOOD, f, h, a));
        let called = PARSER_GOOD.replace(
            "line.split(',').map(parse_int_field).sum()",
            "line.split(',').map(|x| parse_int_field(x)).sum()",
        );
        assert!(body_uses(&called, f, h, a));
    }

    // The mutation: inline the old logic in place of the call. The fixture
    // as shipped is the same shape (the helper missing, the logic inline).
    #[test]
    fn inlining_the_old_logic_fails_the_check() {
        let (f, h, a) = uses_of("refactor-shipping");
        let inlined = SHIPPING_GOOD.replace(
            "apply_discount(price, pct_off) * qty as f64",
            "(price - price * pct_off / 100.0) * qty as f64",
        );
        assert!(!body_uses(&inlined, f, h, a));
        // Helper defined but the call kept next to the inline copy.
        let both = SHIPPING_GOOD.replace(
            "apply_discount(price, pct_off) * qty as f64",
            "let _ = apply_discount(price, pct_off);\n    (price - price * pct_off / 100.0) * qty as f64",
        );
        assert!(!body_uses(&both, f, h, a));

        let (f, h, a) = uses_of("refactor-parser");
        let inlined = "pub fn parse_int_field(field: &str) -> i64 {\n    field.trim().parse().unwrap_or(0)\n}\npub fn tally_csv_ints(line: &str) -> i64 {\n    let mut t = 0;\n    for x in line.split(',') { t += x.trim().parse::<i64>().unwrap_or(0); }\n    t\n}\n";
        assert!(!body_uses(inlined, f, h, a));
    }

    #[test]
    fn the_shipped_fixtures_fail_the_check() {
        for id in ["refactor-shipping", "refactor-parser"] {
            let (f, h, a) = uses_of(id);
            let brief = suite().into_iter().find(|b| b.id == id).expect("brief");
            let src = std::fs::read_to_string(brief.fixture_dir().join("src/lib.rs"))
                .expect("fixture source");
            assert!(!body_uses(&src, f, h, a), "{id}: the unrefactored fixture");
        }
    }

    #[test]
    fn a_mention_in_a_comment_or_string_is_not_a_call() {
        let (f, h, a) = uses_of("refactor-shipping");
        let src = "pub fn total_price(p: f64, q: u32, o: f64) -> f64 {\n    // apply_discount(p, o) would go here\n    let _m = \"apply_discount(p, o)\";\n    /* apply_discount(p, o) */\n    p * q as f64\n}\n";
        assert!(!body_uses(src, f, h, a));
    }

    #[test]
    fn scanning_is_tolerant_of_layout_and_literals() {
        let (f, h, a) = uses_of("refactor-shipping");
        let src = "pub fn   total_price (p: f64, q: u32, o: f64)\n -> f64\n{\n    let _c = '}';\n    let _s = \"}{\";\n    let _r = r#\"}\"#;\n    Self::apply_discount\n        (p, o) * q as f64\n}\n";
        assert!(body_uses(src, f, h, a));
        // A longer function name is not the function, a missing one is false.
        assert!(!body_uses(
            "fn total_price_x() { apply_discount(1.0, 2.0) }",
            f,
            h,
            a
        ));
        assert!(!body_uses("fn other() {}", f, h, a));
        // Unbalanced braces are not a body.
        assert!(!body_uses(
            "fn total_price() { apply_discount(1.0, 2.0)",
            f,
            h,
            a
        ));
    }
}
