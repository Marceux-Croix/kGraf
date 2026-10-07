//! Tests of the recipe readers that pin each branch to its effect: the
//! Makefile parser and expander, the `package.json` script reader, the
//! justfile reader and the cargo configuration reader.

use super::*;
use std::path::Path;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn scripts(found: &[Embedded]) -> Vec<String> {
    let mut out = Vec::new();
    for e in found {
        match e {
            Embedded::Script(s) => out.push(s.clone()),
            Embedded::Argv(a) => out.push(a.join(" ")),
            Embedded::In(_, inner) => out.extend(scripts(std::slice::from_ref(inner))),
            Embedded::Unreadable(_) => {}
        }
    }
    out
}

fn opaque(found: &[Embedded]) -> bool {
    found.iter().any(|e| match e {
        Embedded::Unreadable(_) => true,
        Embedded::In(_, inner) => opaque(std::slice::from_ref(inner)),
        _ => false,
    })
}

fn why(found: &[Embedded]) -> String {
    found
        .iter()
        .filter_map(|e| match e {
            Embedded::Unreadable(w) => Some(w.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, ReadContext) {
    let dir = tempfile::tempdir().expect("dir");
    for (name, text) in files {
        let p = dir.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap_or(Path::new("."))).expect("mkdir");
        std::fs::write(p, text).expect("write");
    }
    let ctx = ReadContext::in_worktree(dir.path(), None);
    (dir, ctx)
}

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn expanded(mf: &Makefile, text: &str) -> String {
    expand(text, mf, &BTreeMap::new(), None, 0)
}

// ------------------------------------------------------------ make: text

#[test]
fn the_closing_bracket_is_found_through_nested_pairs_and_none_when_unclosed() {
    assert_eq!(closing(&chars("x(y)z)"), 0, '('), Some(6));
    assert_eq!(closing(&chars("a{b}c}"), 0, '{'), Some(6));
    assert_eq!(closing(&chars("(a)"), 1, '('), Some(3));
    assert_eq!(closing(&chars("abc"), 0, '('), None);
    assert_eq!(closing(&chars("(()"), 0, '('), None);
    assert_eq!(closing(&chars(""), 0, '('), None);
}

#[test]
fn shell_calls_finds_each_call_in_either_bracket_and_nested() {
    assert_eq!(shell_calls("a $(shell echo hi) b"), vec!["echo hi"]);
    assert_eq!(shell_calls("${shell echo x}"), vec!["echo x"]);
    assert_eq!(
        shell_calls("$(shell echo $(shell date))"),
        vec!["echo $(shell date)", "date"]
    );
    assert_eq!(shell_calls("$(shell a) $(shell b)"), vec!["a", "b"]);
    assert!(shell_calls("x$").is_empty());
    assert!(shell_calls("$").is_empty());
    assert!(shell_calls("$(shell unclosed").is_empty());
    assert!(shell_calls("$(wildcard x)").is_empty());
    // The scan for the closing bracket starts right after the opening one.
    assert_eq!(shell_calls("0123456789$(shell $(x) y)"), vec!["$(x) y"]);
}

#[test]
fn the_rule_colon_is_found_outside_references_and_assignments() {
    assert_eq!(rule_colon("all: dep"), Some(3));
    assert_eq!(rule_colon("a:"), Some(1));
    assert_eq!(rule_colon("x$(A:b): c"), Some(7));
    assert_eq!(rule_colon("x${A:b}: c"), Some(7));
    assert_eq!(rule_colon("\u{e9}: x"), Some(2));
    assert_eq!(rule_colon("$(\u{e9}): x"), Some(5));
    assert_eq!(rule_colon("foo bar"), None);
    assert_eq!(rule_colon(""), None);
    assert_eq!(rule_colon("X = a:b"), None);
    assert_eq!(rule_colon("X := 1"), None);
    assert_eq!(rule_colon("X ::= 1"), None);
    assert_eq!(rule_colon("a::= b"), None);
    assert_eq!(rule_colon("a:= b"), None);
}

#[test]
fn make_assignment_reads_the_name_the_operator_and_the_value() {
    let a = |l: &str| make_assignment(l).map(|(n, o, v)| (n, o.to_owned(), v));
    let t = |n: &str, o: &str, v: &str| Some((n.to_owned(), o.to_owned(), v.to_owned()));
    assert_eq!(a("X = 1"), t("X", "=", "1"));
    assert_eq!(a("X=1"), t("X", "=", "1"));
    assert_eq!(a("X := 1"), t("X", ":=", "1"));
    assert_eq!(a("X ::= 1"), t("X", "::=", "1"));
    assert_eq!(a("X :::= 1"), t("X", ":::=", "1"));
    assert_eq!(a("X ?= 1"), t("X", "?=", "1"));
    assert_eq!(a("X += 1"), t("X", "+=", "1"));
    assert_eq!(a("X != cmd a=b"), t("X", "!=", "cmd a=b"));
    assert_eq!(a("export X = 1"), t("X", "=", "1"));
    assert_eq!(a("override X := 1"), t("X", ":=", "1"));
    assert_eq!(a("private X = 1"), t("X", "=", "1"));
    // A name with a space, a bracket or a colon, or none, is no assignment.
    assert_eq!(a("a b = c"), None);
    assert_eq!(a("x: y = z"), None);
    assert_eq!(a("= v"), None);
    assert_eq!(a("$(A) = v"), None);
    assert_eq!(a("{A} = v"), None);
    assert_eq!(a("plain words"), None);
}

#[test]
fn the_makefile_parser_keeps_each_construct_in_its_place() {
    // A last line ending in a backslash still counts.
    let mf = parse_makefile("all:\n\techo hi\\");
    assert_eq!(mf.rules["all"].recipe, vec!["echo hi "]);
    // A blank line or a comment between a rule and its recipe line keeps
    // the recipe with the rule.
    let mf = parse_makefile("all:\n\n\techo a\n# note\n\techo b\n");
    assert_eq!(mf.rules["all"].recipe, vec!["echo a", "echo b"]);
    // Conditionals do not end a rule.
    let mf = parse_makefile("all:\nifdef X\n\techo a\nelse\n\techo b\nendif\n");
    assert_eq!(mf.rules["all"].recipe, vec!["echo a", "echo b"]);
    for word in ["ifeq (a,b)", "ifneq (a,b)", "ifndef X", "endif"] {
        let mf = parse_makefile(&format!("all:\n{word}\n\techo a\n"));
        assert_eq!(mf.rules["all"].recipe, vec!["echo a"], "{word}");
    }
    // `export NAME` and `vpath` lines end nothing; `export X = v` defines.
    let mf = parse_makefile("all:\nexport FOO\n\techo a\nvpath %.c src\n\techo b\n");
    assert_eq!(mf.rules["all"].recipe, vec!["echo a", "echo b"]);
    let mf = parse_makefile("export X = curl\nall:\n\t$(X) y\n");
    assert_eq!(mf.vars["X"], vec!["curl"]);
    // `$(eval` and `${eval` each make the file unreadable.
    assert!(parse_makefile("X = $(eval A)\n").opaque.is_some());
    assert!(parse_makefile("X = ${eval A}\n").opaque.is_some());
    assert!(parse_makefile("X = $(shell a)\n").opaque.is_none());
    // `!=` runs a command at parse time and leaves the value unknown.
    let mf = parse_makefile("X != curl x\nall:\n\techo $(X)\n");
    assert_eq!(mf.at_parse, vec!["curl x"]);
    // The value is stored so that expanding it gives UNRESOLVED.
    assert_eq!(expanded(&mf, "$(X)"), UNRESOLVED);
    assert_eq!(expanded(&mf, "$(X) y"), format!("{UNRESOLVED} y"));
    // `+=` appends to the last definition, or defines.
    let mf = parse_makefile("X = a\nX += b\nY += c\n");
    assert_eq!(mf.vars["X"], vec!["a b"]);
    assert_eq!(mf.vars["Y"], vec!["c"]);
    // A second `define` of a name is a second definition.
    let mf = parse_makefile("define X\ncurl a\nendef\ndefine X\nwget b\nendef\n");
    assert_eq!(mf.vars["X"], vec!["curl a", "wget b"]);
    // Lines of one `define` are one definition.
    let mf = parse_makefile("define X\ncurl a\nwget b\nendef\n");
    assert_eq!(mf.vars["X"], vec!["curl a\nwget b"]);
    // Unclassified text ends the rule.
    let mf = parse_makefile("all:\nwhat is this\n\techo a\n");
    assert!(mf.rules["all"].recipe.is_empty());
    // The first target that is not a dot target is the default goal.
    let mf = parse_makefile(".PHONY: x\nfirst:\n\techo a\nsecond:\n");
    assert_eq!(mf.default_goal.as_deref(), Some("first"));
    let mf = parse_makefile("first:\n.DEFAULT_GOAL := other\nother:\n");
    assert_eq!(mf.default_goal.as_deref(), Some("other"));
    // `.RECIPEPREFIX` as a target or as a variable is refused.
    assert!(parse_makefile(".RECIPEPREFIX: >\n").opaque.is_some());
    assert!(parse_makefile(".RECIPEPREFIX = >\n").opaque.is_some());
    // `SHELL` that is a POSIX shell is read; another is not.
    assert!(parse_makefile("SHELL = /bin/bash\n").opaque.is_none());
    assert!(parse_makefile("SHELL = python3\n").opaque.is_some());
    // `target: VAR = value` is a variable, not a rule.
    let mf = parse_makefile("t: V = x\n");
    assert!(mf.rules.is_empty());
    assert_eq!(mf.vars["V"], vec!["x"]);
    // An inline recipe after `;`, and order-only prerequisites.
    let mf = parse_makefile("a: b | c ; echo hi\n");
    assert_eq!(mf.rules["a"].recipe, vec!["echo hi"]);
    assert_eq!(mf.rules["a"].prerequisites, vec!["b", "c"]);
}

#[test]
fn posix_shells_are_the_five_and_nothing_else() {
    for ok in [
        "sh",
        "bash",
        "dash",
        "zsh",
        "ksh",
        "/bin/sh -c",
        "/usr/bin/zsh",
    ] {
        assert!(posix_shell(ok), "{ok}");
    }
    for bad in ["", "python3", "fish", "pwsh", "csh", "/bin/fish -c"] {
        assert!(!posix_shell(bad), "{bad}");
    }
}

#[test]
fn make_built_in_programs_have_their_make_values() {
    for (name, value) in [
        ("MAKE", "make"),
        ("CC", "cc"),
        ("CXX", "c++"),
        ("CPP", "cc -E"),
        ("RM", "rm -f"),
        ("AR", "ar"),
        ("AS", "as"),
        ("LD", "ld"),
        ("LEX", "lex"),
        ("YACC", "yacc"),
        ("FC", "f77"),
        ("SHELL", "/bin/sh"),
    ] {
        assert_eq!(builtin_var(name), Some(value), "{name}");
        let mf = parse_makefile("");
        assert_eq!(expanded(&mf, &format!("$({name})")), value, "{name}");
    }
    assert_eq!(builtin_var("TOOL"), None);
}

// ----------------------------------------------------------- make: expand

#[test]
fn expansion_stops_at_depth_sixteen_and_not_before() {
    fn chain(n: usize, last: &str) -> String {
        let mut t = String::new();
        for k in 1..n {
            t.push_str(&format!("V{k} = $(V{})\n", k + 1));
        }
        t.push_str(&format!("V{n} = {last}\n"));
        t
    }
    let at16 = parse_makefile(&chain(16, "ok"));
    assert_eq!(expanded(&at16, "$(V1)"), "ok");
    let at17 = parse_makefile(&chain(17, "ok"));
    assert_eq!(expanded(&at17, "$(V1)"), UNRESOLVED);
    // A function call's arguments are one level deeper than the call.
    let mut text = String::new();
    for k in 1..15 {
        text.push_str(&format!("V{k} = $(V{})\n", k + 1));
    }
    text.push_str("V15 = $(wildcard $(W))\nW = a;b\n");
    let deep = parse_makefile(&text);
    assert_eq!(expanded(&deep, "$(V1)"), UNRESOLVED);
    // One level up the same arguments are read, and the `;` shows.
    let mut text = String::new();
    for k in 1..14 {
        text.push_str(&format!("V{k} = $(V{})\n", k + 1));
    }
    text.push_str("V14 = $(wildcard $(W))\nW = a;b\n");
    let shallow = parse_makefile(&text);
    assert_eq!(expanded(&shallow, "$(V1)"), CANNOT_READ.to_string());
}

#[test]
fn a_variable_that_names_itself_ends_unresolved() {
    let once = parse_makefile("X = $(X)\n");
    assert!(expanded(&once, "$(X)").contains(UNRESOLVED));
    let twice = parse_makefile("X = $(X)\nX = $(X)\n");
    assert!(expanded(&twice, "$(X)").contains(UNRESOLVED));
    let mut overrides = BTreeMap::new();
    overrides.insert("X".to_owned(), "$(X)".to_owned());
    let mf = parse_makefile("");
    assert!(expand("$(X)", &mf, &overrides, None, 0).contains(UNRESOLVED));
}

#[test]
fn a_dollar_and_unclosed_references_expand_as_make_reads_them() {
    let mf = parse_makefile("X = v\n");
    assert_eq!(expanded(&mf, "x$$y"), "x$y");
    assert_eq!(expanded(&mf, "a$(b"), format!("a{UNRESOLVED}"));
    assert_eq!(expanded(&mf, "x$"), "x$");
    assert_eq!(expanded(&mf, "a$Xb"), "avb");
    assert_eq!(expanded(&mf, "a${X}b"), "avb");
    assert_eq!(expanded(&mf, "a$@b"), "ab");
    // A variable never defined is empty.
    assert_eq!(expanded(&mf, "[$(NONE)]"), "[]");
}

#[test]
fn variables_defined_twice_are_unknown_and_may_hold_shell_syntax() {
    let mf = parse_makefile("X = a\nX = b\nY = a\nY = b; c\n");
    assert_eq!(expanded(&mf, "$(X)"), UNRESOLVED);
    assert_eq!(expanded(&mf, "$(Y)"), CANNOT_READ.to_string());
    let mf = parse_makefile("Z = a\nZ = $(Y)\nY = p\nY = q|r\n");
    assert_eq!(expanded(&mf, "$(Z)"), CANNOT_READ.to_string());
}

#[test]
fn function_calls_are_unknown_and_some_may_hold_shell_syntax() {
    let mf = parse_makefile("");
    let u = UNRESOLVED;
    let c = CANNOT_READ.to_string();
    assert_eq!(expanded(&mf, "$(wildcard *.c)"), u);
    assert_eq!(expanded(&mf, "${wildcard *.c}"), u);
    assert_eq!(expanded(&mf, "$(a,b)"), u);
    assert_eq!(expanded(&mf, "$(subst a,b,c)"), u);
    for f in [
        "$(call f,x)",
        "$(value V)",
        "$(eval x)",
        "$(foreach a,b,c)",
        "$(file >x,y)",
    ] {
        assert_eq!(expanded(&mf, f), c, "{f}");
    }
    assert_eq!(expanded(&mf, "$(wildcard a;b)"), c);
    assert_eq!(expanded(&mf, "$(wildcard a b|c)"), c);
    assert_eq!(expanded(&mf, "$(wildcard $(call f))"), c);
    assert_eq!(expanded(&mf, "$(wildcard a&b)"), c);
    assert_eq!(expanded(&mf, "$(wildcard a`b`)"), c);
    assert_eq!(expanded(&mf, "$(wildcard a<b)"), c);
    assert_eq!(expanded(&mf, "$(wildcard a>b)"), c);
    assert_eq!(expanded(&mf, "$(wildcard a(b))"), c);
}

#[test]
fn automatic_variables_have_values_in_a_plain_rule_and_none_in_a_pattern_rule() {
    let mf = parse_makefile("");
    let text = "$@ $(@D) $(@F) $< $(<D) $(<F) $^ $+ $? $* $| $(*D) $(*F)";
    let prerequisites = argv(&["y/x.c", "h.h"]);
    let plain = RecipeContext {
        target: "out/a.o",
        prerequisites: &prerequisites,
        pattern: false,
    };
    let u = UNRESOLVED;
    assert_eq!(
        expand(text, &mf, &BTreeMap::new(), Some(&plain), 0),
        format!("out/a.o out a.o y/x.c y x.c y/x.c h.h y/x.c h.h y/x.c h.h {u} {u} {u} {u}")
    );
    let pattern = RecipeContext {
        target: "",
        prerequisites: &prerequisites,
        pattern: true,
    };
    let all_unknown = vec![u; 13].join(" ");
    assert_eq!(
        expand(text, &mf, &BTreeMap::new(), Some(&pattern), 0),
        all_unknown
    );
    // A target and a prerequisite without a folder.
    let bare_pre = argv(&["x.c"]);
    let bare = RecipeContext {
        target: "a.o",
        prerequisites: &bare_pre,
        pattern: false,
    };
    assert_eq!(
        expand(
            "$(@D) $(@F) $(<D) $(<F)",
            &mf,
            &BTreeMap::new(),
            Some(&bare),
            0
        ),
        ". a.o . x.c"
    );
    // No prerequisite: `$<` is empty.
    let none: Vec<String> = Vec::new();
    let lone = RecipeContext {
        target: "t",
        prerequisites: &none,
        pattern: false,
    };
    assert_eq!(
        expand("[$<][$^]", &mf, &BTreeMap::new(), Some(&lone), 0),
        "[][]"
    );
    // An override of an automatic name wins over the rule's value.
    let mut overrides = BTreeMap::new();
    overrides.insert("@".to_owned(), "forced".to_owned());
    assert_eq!(expand("$@", &mf, &overrides, Some(&plain), 0), "forced");
}

// ------------------------------------------------------------- make: call

const MAKEFILE: &str =
    "all:\n\techo all\nsub:\n\techo sub\n2:\n\techo two\n4:\n\techo four\nclean:\n\techo clean\n";

fn tree_for_make() -> (tempfile::TempDir, ReadContext) {
    tree(&[
        ("Makefile", MAKEFILE),
        ("alt.mk", "all:\n\techo alt\nclean:\n\techo alt-clean\n"),
        ("d/Makefile", "all:\n\techo d\n"),
        ("d/e/Makefile", "all:\n\techo de\n"),
    ])
}

#[test]
fn make_takes_its_file_and_directory_options_in_every_spelling() {
    let (_d, ctx) = tree_for_make();
    let m = |args: &[&str]| scripts(&make(&[], &argv(args), &ctx));
    for args in [
        &["-f", "alt.mk"][..],
        &["-falt.mk"],
        &["--file=alt.mk"],
        &["--file", "alt.mk"],
        &["--makefile=alt.mk"],
        &["--makefile", "alt.mk"],
    ] {
        assert_eq!(m(args), vec!["echo alt"], "{args:?}");
    }
    // The option after a goal.
    assert_eq!(m(&["clean", "-f", "alt.mk"]), vec!["echo alt-clean"]);
    assert_eq!(m(&["clean", "--file=alt.mk"]), vec!["echo alt-clean"]);
    for args in [
        &["-C", "d"][..],
        &["-Cd"],
        &["--directory=d"],
        &["--directory", "d"],
        &["all", "-C", "d"],
    ] {
        assert_eq!(m(args), vec!["echo d"], "{args:?}");
    }
    assert_eq!(m(&["-C", "d", "-C", "e"]), vec!["echo de"]);
    assert_eq!(m(&["-C", "d", "--directory=e"]), vec!["echo de"]);
    // A `-C` run is read in the directory it names.
    assert!(matches!(
        make(&[], &argv(&["-C", "d"]), &ctx)[0],
        Embedded::In(..)
    ));
    assert!(matches!(
        make(&[], &argv(&["clean"]), &ctx)[0],
        Embedded::Script(_)
    ));
}

#[test]
fn make_skips_the_options_that_take_a_value_and_a_job_count() {
    let (_d, ctx) = tree_for_make();
    let m = |args: &[&str]| scripts(&make(&[], &argv(args), &ctx));
    assert_eq!(m(&["-j", "4", "clean"]), vec!["echo clean"]);
    assert_eq!(m(&["-l", "2", "clean"]), vec!["echo clean"]);
    assert_eq!(m(&["clean", "-j", "2"]), vec!["echo clean"]);
    assert_eq!(m(&["clean", "-l", "4"]), vec!["echo clean"]);
    assert_eq!(m(&["-j", "clean"]), vec!["echo clean"]);
    assert_eq!(m(&["-l", "clean"]), vec!["echo clean"]);
    assert_eq!(m(&["-j", "1.5", "clean"]), vec!["echo clean"]);
    assert_eq!(m(&["-k", "clean"]), vec!["echo clean"]);
    assert_eq!(m(&["clean", "-k", "-s"]), vec!["echo clean"]);
    for args in [
        &["-I", "x", "clean"][..],
        &["-Ix", "clean"],
        &["--include-dir", "x", "clean"],
        &["--include-dir=x", "clean"],
        &["-o", "x", "clean"],
        &["--old-file", "x", "clean"],
        &["-W", "x", "clean"],
        &["--what-if=x", "clean"],
        &["--assume-old", "x", "clean"],
        &["--assume-new=x", "clean"],
        &["clean", "-W", "x"],
        &["clean", "--old-file=x"],
    ] {
        assert_eq!(m(args), vec!["echo clean"], "{args:?}");
    }
    // A variable assignment is no goal, after a goal or before.
    assert_eq!(m(&["clean", "OUT=x", "OTHER:=y"]), vec!["echo clean"]);
    assert_eq!(m(&["OUT:=x", "clean"]), vec!["echo clean"]);
}

#[test]
fn make_refuses_eval_in_each_spelling_and_says_why_it_cannot_read() {
    let (_d, ctx) = tree_for_make();
    for flag in ["--eval", "--eval=X=1", "-E"] {
        let found = make(&[], &argv(&[flag, "clean"]), &ctx);
        assert!(opaque(&found), "{flag}");
        assert!(why(&found).contains("`--eval`"), "{flag}");
    }
    // Without a directory the message says so; with one and no makefile,
    // it says that.
    let found = make(&[], &argv(&[]), &ReadContext::default());
    assert!(why(&found).contains("from a directory kGraf cannot tell"));
    let (_e, empty) = tree(&[("x", "")]);
    let found = make(&[], &argv(&[]), &empty);
    assert!(why(&found).contains("finds no makefile kGraf can read"));
}

#[test]
fn make_stops_at_a_cycle_and_at_five_hundred_twelve_targets() {
    let (_d, ctx) = tree(&[("Makefile", "a: b\n\techo a\nb: a\n\techo b\n")]);
    assert_eq!(
        scripts(&make(&[], &argv(&[]), &ctx)),
        vec!["echo a", "echo b"]
    );
    let mut text = String::new();
    for k in 0..600 {
        text.push_str(&format!("t{k}: t{}\n\techo {k}\n", k + 1));
    }
    text.push_str("t600:\n\techo 600\n");
    let (_e, long) = tree(&[("Makefile", &text)]);
    let found = scripts(&make(&[], &argv(&["t0"]), &long));
    assert_eq!(found.len(), 512);
    assert_eq!(found[0], "echo 0");
    assert_eq!(found[511], "echo 511");
}

#[test]
fn pattern_rules_run_for_a_target_without_a_rule_once() {
    let (_d, ctx) = tree(&[
        (
            "Makefile",
            "%.o: %.c common.h\n\tcc -c $<\ncommon.h:\n\tgen\n",
        ),
        ("two/Makefile", "a.o: x\nb.o: y\n%.o: %.c\n\tcc -c $<\n"),
        ("recipe/Makefile", "a.o:\n\techo a\n%.o: %.c\n\tcc -c $<\n"),
        ("none/Makefile", "a.o: dep\nb.o: dep\ndep:\n\techo dep\n"),
        ("bare/Makefile", "a.o:\n%.o: %.c\n\tcc -c $<\n"),
    ]);
    let cc = format!("cc -c {UNRESOLVED}");
    let one = scripts(&make(&[], &argv(&["a.o"]), &ctx));
    assert_eq!(one, vec![cc.clone(), "gen".to_owned()]);
    // A second target without a rule does not read them again.
    let two = scripts(&make(&[], &argv(&["a.o", "b.o"]), &ctx));
    assert_eq!(two, vec![cc.clone(), "gen".to_owned()]);
    // Rules without a recipe use them, once.
    let found = scripts(&make(&[], &argv(&["-C", "two", "a.o", "b.o"]), &ctx));
    assert_eq!(found, vec![cc.clone()]);
    // A rule with a recipe does not.
    let found = scripts(&make(&[], &argv(&["-C", "recipe", "a.o"]), &ctx));
    assert_eq!(found, vec!["echo a"]);
    // A rule with no recipe and no prerequisite uses them too.
    let found = scripts(&make(&[], &argv(&["-C", "bare", "a.o"]), &ctx));
    assert_eq!(found, vec![cc.clone()]);
    // Without pattern rules, an unknown target runs nothing.
    let found = scripts(&make(&[], &argv(&["-C", "none", "zzz"]), &ctx));
    assert!(found.is_empty(), "{found:?}");
    let found = scripts(&make(&[], &argv(&["-C", "none", "a.o"]), &ctx));
    assert_eq!(found, vec!["echo dep"]);
}

/// A program named by a `!=` variable is a command kGraf cannot know: the
/// word holds `$`, so it is unreadable and never judged as a program called
/// `_unresolved`, which a deny rule on the real program would miss.
#[test]
fn a_program_named_by_a_shell_assignment_stays_unresolved() {
    let (_d, ctx) = tree(&[(
        "Makefile",
        "PROG != echo curl\nALSO != echo wget\nALSO += -q\nall:\n\t$(PROG) x\n\t$(ALSO) y\n",
    )]);
    let found = scripts(&make(&[], &argv(&[]), &ctx));
    assert_eq!(
        found,
        vec![
            "echo curl".to_owned(),
            "echo wget".to_owned(),
            format!("{UNRESOLVED} x"),
            format!("{UNRESOLVED} -q y"),
        ]
    );
    assert!(found[2].starts_with('$') && found[3].starts_with('$'));
}

#[test]
fn make_reads_commands_that_run_while_it_reads_the_file() {
    let (_d, ctx) = tree(&[
        ("Makefile", "X != curl x\nall:\n\techo $(X)\n"),
        ("none/Makefile", "V := $(shell wget y)\n"),
        ("risky/Makefile", "V := $(call f,x)\nW := $(shell $(V))\n"),
    ]);
    let found = scripts(&make(&[], &argv(&[]), &ctx));
    assert_eq!(found, vec!["curl x", &format!("echo {UNRESOLVED}")]);
    // No goal and no rule: only what runs while reading.
    let found = scripts(&make(&[], &argv(&["-C", "none"]), &ctx));
    assert_eq!(found, vec!["wget y"]);
    let found = make(&[], &argv(&["-C", "risky"]), &ctx);
    assert!(opaque(&found));
    assert!(why(&found).contains("cannot know"), "{}", why(&found));
}

// ---------------------------------------------------------------- package

const PACKAGE: &str = r#"{"scripts":{"build":"tsc","test":"jest","start":"node app.js","stop":"echo stop","audit":"curl audit","why":"curl why","lint":"eslint","restart":"echo restart","build.js":"curl dotted","bin/b":"curl slash"}}"#;

fn package(tool: &str, words: &[&str], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    package_scripts(tool, &argv(words), ctx)
}

#[test]
fn a_workspace_or_directory_option_before_the_command_makes_it_unreadable() {
    let (_d, ctx) = tree(&[("package.json", PACKAGE)]);
    let values = [
        "-w",
        "--workspace",
        "--prefix",
        "-C",
        "--dir",
        "--cwd",
        "--filter",
        "-F",
    ];
    for opt in values {
        let found = package("npm", &[opt, "x", "run", "build"], &ctx).expect("read");
        assert!(opaque(&found), "{opt}");
        let found = package("npm", &[opt, "x", "install"], &ctx).expect("read");
        assert!(opaque(&found), "{opt} install");
    }
    for opt in [
        "-r",
        "--recursive",
        "-ws",
        "--workspaces",
        "--prefix=x",
        "--dir=x",
        "--cwd=x",
        "--workspace=x",
        "--filter=x",
    ] {
        let found = package("npm", &[opt, "run", "build"], &ctx).expect("read");
        assert!(opaque(&found), "{opt}");
        let found = package("npm", &[opt, "install"], &ctx).expect("read");
        assert!(opaque(&found), "{opt} install");
    }
    // Other options, and `--bun`, change nothing.
    for opt in ["--silent", "--bun", "-q"] {
        let found = package("npm", &[opt, "run", "build"], &ctx).expect("read");
        assert_eq!(scripts(&found), vec!["tsc"], "{opt}");
    }
    // The value of an option that takes one is not the command.
    let found = package("npm", &["--silent", "-C", "x", "-q", "run", "build"], &ctx).expect("r");
    assert!(opaque(&found));
}

#[test]
fn a_workspace_option_after_the_command_makes_it_unreadable_until_the_double_dash() {
    let (_d, ctx) = tree(&[("package.json", PACKAGE)]);
    for opt in [
        "-w",
        "--workspace",
        "-ws",
        "--workspaces",
        "--prefix",
        "-C",
        "--dir",
        "--filter",
        "-F",
        "-r",
        "--recursive",
        "--cwd",
        "--workspace=a",
        "--prefix=a",
        "--filter=a",
        "--dir=a",
        "--cwd=a",
    ] {
        let found = package("npm", &["run", "build", opt], &ctx).expect("read");
        assert!(opaque(&found), "{opt}");
        let found = package("npm", &["test", opt], &ctx).expect("read");
        assert!(opaque(&found), "{opt} test");
    }
    // After `--` the words belong to the script.
    let found = package("npm", &["run", "build", "--", "-w"], &ctx).expect("read");
    assert_eq!(scripts(&found), vec!["tsc -w"]);
    // Options of `run` before the script name.
    for opt in [
        "--workspace-root",
        "--filter-prod",
        "--workspace=a",
        "--workspaces",
    ] {
        let found = package("npm", &["run", opt, "build"], &ctx).expect("read");
        assert!(opaque(&found), "{opt}");
    }
    for opt in ["--silent", "--if-present", "-s"] {
        let found = package("npm", &["run", opt, "build"], &ctx).expect("read");
        assert_eq!(scripts(&found), vec!["tsc"], "{opt}");
    }
    let found = package("npm", &["run", "--silent", "--if-present", "build"], &ctx).expect("r");
    assert_eq!(scripts(&found), vec!["tsc"]);
}

#[test]
fn each_manager_runs_its_scripts_and_leaves_its_built_in_commands() {
    let (_d, ctx) = tree(&[("package.json", PACKAGE)]);
    let s = |tool: &str, words: &[&str]| package(tool, words, &ctx).map(|f| scripts(&f));
    // pnpm and yarn take `t` for `test`; `start` and `stop` are scripts.
    assert_eq!(s("pnpm", &["t"]), Some(vec!["jest".to_owned()]));
    assert_eq!(s("yarn", &["t"]), Some(vec!["jest".to_owned()]));
    assert_eq!(s("pnpm", &["test"]), Some(vec!["jest".to_owned()]));
    assert_eq!(s("yarn", &["start"]), Some(vec!["node app.js".to_owned()]));
    assert_eq!(s("pnpm", &["stop"]), Some(vec!["echo stop".to_owned()]));
    assert_eq!(
        s("yarn", &["restart"]),
        Some(vec!["echo restart".to_owned()])
    );
    assert_eq!(s("npm", &["t"]), Some(vec!["jest".to_owned()]));
    assert_eq!(s("npm", &["tst"]), Some(vec!["jest".to_owned()]));
    // A built-in command runs no script of its name.
    assert_eq!(s("pnpm", &["audit"]), None);
    assert_eq!(s("yarn", &["why"]), None);
    assert_eq!(s("bun", &["pm"]), None);
    assert_eq!(s("bun", &["upgrade"]), None);
    assert_eq!(s("npm", &["audit"]), None);
    // A name that is no built-in runs the script of that name.
    assert_eq!(s("pnpm", &["lint"]), Some(vec!["eslint".to_owned()]));
    assert_eq!(s("yarn", &["lint"]), Some(vec!["eslint".to_owned()]));
    assert_eq!(s("bun", &["lint"]), Some(vec!["eslint".to_owned()]));
    // With no script, pnpm, yarn and bun run the program of the name; pnpm
    // does not for `run`.
    assert_eq!(
        s("pnpm", &["eslint", "."]),
        Some(vec!["eslint .".to_owned()])
    );
    assert_eq!(
        s("yarn", &["run", "nothing"]),
        Some(vec!["nothing".to_owned()])
    );
    assert_eq!(
        s("bun", &["run", "nothing"]),
        Some(vec!["nothing".to_owned()])
    );
    assert_eq!(
        s("yarn", &["run-script", "nothing"]),
        Some(vec!["nothing".to_owned()])
    );
    assert_eq!(s("pnpm", &["run", "nothing"]), Some(Vec::new()));
    assert_eq!(s("pnpm", &["run-script", "nothing"]), Some(Vec::new()));
    assert_eq!(s("npm", &["run", "nothing"]), Some(Vec::new()));
    assert_eq!(s("npm", &["run"]), Some(Vec::new()));
    assert_eq!(s("npm", &["run", "rum"]), Some(Vec::new()));
    // npm's default start runs `node server.js`.
    let (_e, bare) = tree(&[("package.json", "{}")]);
    let found = package("npm", &["start"], &bare).expect("read");
    assert_eq!(found, vec![Embedded::Argv(argv(&["node", "server.js"]))]);
    let found = package("npm", &["stop"], &bare).expect("read");
    assert!(found.is_empty());
    // `rum` and `urn` are npm's spellings of `run`.
    assert_eq!(s("npm", &["rum", "build"]), Some(vec!["tsc".to_owned()]));
    assert_eq!(s("npm", &["urn", "build"]), Some(vec!["tsc".to_owned()]));
    assert_eq!(
        s("npm", &["run-script", "build"]),
        Some(vec!["tsc".to_owned()])
    );
    assert_eq!(s("npm", &["frobnicate"]), None);
    assert_eq!(s("npm", &[]), None);
}

#[test]
fn the_built_in_lists_name_their_commands() {
    for (tool, names) in [
        (
            "npm",
            &["install", "ci", "audit", "publish", "view", "exec"][..],
        ),
        ("pnpm", &["add", "install", "audit", "dlx", "store", "sbom"]),
        ("yarn", &["add", "audit", "dlx", "workspace", "why"]),
        ("bun", &["install", "pm", "create", "discord"]),
    ] {
        for name in names {
            assert!(builtin(tool, name), "{tool} {name}");
        }
        assert!(!builtin(tool, "my-own-script"), "{tool}");
    }
    assert!(!builtin("deno", "install"));
}

#[test]
fn an_install_family_command_runs_its_lifecycle_scripts_in_order() {
    let every = r#"{"scripts":{"preinstall":"1","install":"2","postinstall":"3","preprepare":"4","prepare":"5","postprepare":"6","prepublishOnly":"7","prepack":"8","postpack":"9","publish":"10","postpublish":"11"}}"#;
    let (_d, ctx) = tree(&[("package.json", every)]);
    let s = |tool: &str, words: &[&str]| scripts(&package(tool, words, &ctx).unwrap_or_default());
    let install = ["1", "2", "3", "4", "5", "6"];
    for words in [
        &["install"][..],
        &["i"],
        &["add"],
        &["ci"],
        &["it"],
        &["install-ci-test"],
    ] {
        assert_eq!(s("npm", words), install, "npm {words:?}");
    }
    assert_eq!(s("npm", &["rebuild"]), install);
    assert_eq!(s("npm", &["rb"]), install);
    assert_eq!(s("pnpm", &["install"]), install);
    assert_eq!(s("yarn", &["add", "x"]), install);
    assert_eq!(s("bun", &["i"]), install);
    // `rebuild` of the other managers installs no project script.
    assert_eq!(package("pnpm", &["rebuild"], &ctx), None);
    assert_eq!(s("npm", &["publish"]), ["7", "8", "5", "9", "10", "11"]);
    assert_eq!(s("pnpm", &["publish"]), ["7", "8", "5", "9", "10", "11"]);
    assert_eq!(s("npm", &["pack"]), ["8", "5", "9"]);
    assert_eq!(s("yarn", &["pack"]), ["8", "5", "9"]);
    // Without a `package.json`, nothing of this project runs; one that does
    // not parse is unreadable.
    let (_e, none) = tree(&[("x", "")]);
    assert_eq!(package("npm", &["install"], &none), Some(Vec::new()));
    let (_f, bad) = tree(&[("package.json", "{")]);
    assert!(opaque(&package("npm", &["install"], &bad).expect("read")));
    assert!(opaque(&package("npm", &["run", "x"], &bad).expect("read")));
    let (_g, text) = tree(&[("package.json", r#"{"scripts":{"a":1}}"#)]);
    let found = package("npm", &["run", "a"], &text).expect("read");
    assert!(why(&found).contains("is not text"), "{}", why(&found));
}

#[test]
fn restart_runs_stop_and_start_when_there_is_no_restart_script() {
    let both = r#"{"scripts":{"prestop":"a","stop":"b","poststop":"c","prestart":"d","start":"e","poststart":"f"}}"#;
    let (_d, ctx) = tree(&[("package.json", both)]);
    let found = package("npm", &["restart"], &ctx).expect("read");
    assert_eq!(scripts(&found), vec!["a", "b", "c", "d", "e", "f"]);
    // A restart script replaces that sequence, with its own hooks.
    let own = r#"{"scripts":{"prerestart":"p","restart":"r","postrestart":"q","stop":"b"}}"#;
    let (_e, ctx) = tree(&[("package.json", own)]);
    let found = package("npm", &["restart"], &ctx).expect("read");
    assert_eq!(scripts(&found), vec!["p", "r", "q"]);
    // Another manager's `restart` is a script of that name, or nothing.
    let (_f, ctx) = tree(&[("package.json", both)]);
    let found = package("yarn", &["restart"], &ctx).expect("read");
    assert_eq!(scripts(&found), vec!["restart"]);
    // The words after the script go to the main script only.
    let found = package("npm", &["run", "stop", "--", "now"], &ctx).expect("read");
    assert_eq!(scripts(&found), vec!["a", "b now", "c"]);
}

#[test]
fn bun_runs_a_file_as_code_and_the_others_run_the_script_of_that_name() {
    let (_d, ctx) = tree(&[
        ("package.json", PACKAGE),
        ("build.js", "console.log(1)\n"),
        ("bin/b", "console.log(2)\n"),
        ("build", "x"),
    ]);
    let s = |tool: &str, words: &[&str]| scripts(&package(tool, words, &ctx).unwrap_or_default());
    assert!(!s("bun", &["run", "build.js"]).contains(&"curl dotted".to_owned()));
    assert!(!s("bun", &["run", "bin/b"]).contains(&"curl slash".to_owned()));
    assert_eq!(s("npm", &["run", "build.js"]), vec!["curl dotted"]);
    assert_eq!(s("npm", &["run", "bin/b"]), vec!["curl slash"]);
    assert_eq!(s("npm", &["run", "build"]), vec!["tsc"]);
    assert_eq!(s("bun", &["run", "build"]), vec!["tsc"]);
    // A name that is no file is a script of that name.
    assert_eq!(s("bun", &["run", "nofile.js"]), vec!["nofile.js"]);
}

#[test]
fn the_shell_a_package_script_runs_in_can_be_named_by_the_environment_or_npmrc() {
    let (_d, ctx) = tree(&[
        ("package.json", PACKAGE),
        (".npmrc", "registry=x\n  script-shell=/bin/fish\n"),
    ]);
    let found = package("npm", &["run", "build"], &ctx).expect("read");
    assert!(why(&found).contains("script-shell"), "{}", why(&found));
    // Only npm reads `.npmrc` this way.
    let found = package("pnpm", &["build"], &ctx).expect("read");
    assert_eq!(scripts(&found), vec!["tsc"]);
    let (_e, plain) = tree(&[("package.json", PACKAGE), (".npmrc", "registry=x\n")]);
    let found = package("npm", &["run", "build"], &plain).expect("read");
    assert_eq!(scripts(&found), vec!["tsc"]);
    for name in ["npm_config_script_shell", "NPM_CONFIG_SCRIPT_SHELL"] {
        let with = ReadContext {
            env: vec![(name.to_owned(), "/bin/fish".to_owned())],
            ..plain.clone()
        };
        let found = package("npm", &["run", "build"], &with).expect("read");
        assert!(
            why(&found).contains("shell its environment names"),
            "{name}"
        );
    }
}

// ------------------------------------------------------------------- just

fn just_in(files: &[(&str, &str)], args: &[&str]) -> Vec<Embedded> {
    let (_d, ctx) = tree(files);
    just(&argv(args), &ctx)
}

#[test]
fn backticks_are_found_one_by_one_and_in_blocks() {
    assert_eq!(backticks("`a` and `b`"), vec!["a", "b"]);
    assert_eq!(backticks("x `a b` y `c`"), vec!["a b", "c"]);
    assert_eq!(backticks("```one two```"), vec!["one two"]);
    assert_eq!(backticks("```a``` `b`"), vec!["a", "b"]);
    assert_eq!(backticks("`open"), Vec::<String>::new());
    assert_eq!(backticks("none"), Vec::<String>::new());
}

#[test]
fn just_reads_the_set_settings_and_refuses_another_shell() {
    let r = |setting: &str| {
        just_in(
            &[("justfile", &format!("{setting}\nr:\n    echo hi\n"))],
            &["r"],
        )
    };
    assert_eq!(scripts(&r("set export")), vec!["echo hi"]);
    assert_eq!(scripts(&r("set positional-arguments")), vec!["echo hi"]);
    assert_eq!(
        scripts(&r("set shell := [\"bash\", \"-c\"]")),
        vec!["echo hi"]
    );
    assert_eq!(scripts(&r("set shell := ['sh', '-cu']")), vec!["echo hi"]);
    assert_eq!(
        scripts(&r("set windows-powershell := false")),
        vec!["echo hi"]
    );
    for bad in [
        "set shell := [\"fish\", \"-c\"]",
        "set windows-shell := [\"powershell.exe\"]",
        "set script-interpreter := ['python3']",
        "set windows-powershell",
        "set windows-powershell := true",
        "set working-directory := 'sub'",
    ] {
        let found = r(bad);
        assert!(opaque(&found), "{bad}");
    }
    let found = r("set shell := [\"fish\", \"-c\"]");
    assert!(
        why(&found).contains("sets `shell` to a shell"),
        "{}",
        why(&found)
    );
    // A backtick in a setting runs while just reads the file.
    let found = scripts(&r("set dotenv-filename := `curl x`"));
    assert!(found.contains(&"curl x".to_owned()), "{found:?}");
}

#[test]
fn just_reads_aliases_exports_variables_and_refuses_imports() {
    let f = "alias b := build\nexport X := 'val'\nunexport Y := 'other'\nmy-var_2 := 'dash'\nbuild:\n    echo {{X}} {{Y}} {{my-var_2}}\n";
    let found = just_in(&[("justfile", f)], &["b"]);
    assert_eq!(scripts(&found), vec!["echo val other dash"]);
    // The alias also names a dependency.
    let g = "alias b := build\nbuild:\n    echo built\nall: b\n    echo all\n";
    let found = just_in(&[("justfile", g)], &["all"]);
    assert_eq!(scripts(&found), vec!["echo all", "echo built"]);
    // Backticks of an exported variable run while just reads the file.
    let h = "export X := `curl x`\nr:\n    echo hi\n";
    let found = scripts(&just_in(&[("justfile", h)], &["r"]));
    assert!(found.contains(&"curl x".to_owned()), "{found:?}");
    for text in ["import 'a.just'", "import? 'a.just'", "mod a", "mod? a"] {
        let found = just_in(
            &[("justfile", &format!("{text}\nr:\n    echo hi\n"))],
            &["r"],
        );
        assert!(why(&found).contains("imports another justfile"), "{text}");
    }
    let found = just_in(&[("justfile", "what is this\nr:\n    echo hi\n")], &["r"]);
    assert!(why(&found).contains("has a line kGraf cannot read"));
}

#[test]
fn just_comments_keep_a_recipe_open_and_blank_lines_close_it() {
    let found = just_in(
        &[("justfile", "r:\n    echo a\n# note\n    echo b\n")],
        &["r"],
    );
    assert_eq!(scripts(&found), vec!["echo a", "echo b"]);
    let found = just_in(
        &[("justfile", "r:\n    echo a\n    # inner\n\n    echo b\n")],
        &["r"],
    );
    assert!(opaque(&found));
    // A shebang line only opens a shebang recipe when it is the first.
    let found = just_in(&[("justfile", "r:\n    echo a\n    #!x\n")], &["r"]);
    assert_eq!(scripts(&found), vec!["echo a"]);
}

#[test]
fn just_parameters_dependencies_and_attributes() {
    let f = "default: x\nx $a b='1' *c:\n    echo {{a}}\ny +d:\n    echo y\nz: x && y\n    echo z\nw: (x 'q') y\n    echo w\n";
    let (_d, ctx) = tree(&[("justfile", f)]);
    let jf = parse_justfile(f);
    assert_eq!(jf.recipes["x"].params, 3);
    assert_eq!(jf.recipes["x"].required, 1);
    assert!(jf.recipes["x"].variadic);
    let g = parse_justfile("z a b='1' c='2':\n    echo z\n");
    assert_eq!(g.recipes["z"].params, 3);
    assert_eq!(g.recipes["z"].required, 1);
    assert!(!g.recipes["z"].variadic);
    let h = parse_justfile("z a +b='1' *c:\n    echo z\n");
    assert_eq!(h.recipes["z"].params, 3);
    assert_eq!(h.recipes["z"].required, 1);
    assert!(h.recipes["z"].variadic);
    assert_eq!(jf.recipes["y"].params, 1);
    assert_eq!(jf.recipes["y"].required, 1);
    assert!(jf.recipes["y"].variadic);
    assert_eq!(jf.recipes["z"].dependencies, vec!["x", "y"]);
    assert_eq!(jf.recipes["w"].dependencies, vec!["x", "y"]);
    // A variadic recipe takes every word after it.
    let found = just(&argv(&["y", "a", "b", "c"]), &ctx);
    assert_eq!(scripts(&found), vec!["echo y"]);
    // A recipe takes its parameters and the next word is the next recipe.
    let found = just(&argv(&["y", "x", "z"]), &ctx);
    assert_eq!(scripts(&found), vec!["echo y"]);
    let g = "a p q:\n    echo a\nb:\n    echo b\nc:\n    echo c\n";
    let (_e, ctx) = tree(&[("justfile", g)]);
    let found = scripts(&just(&argv(&["a", "1", "2", "b"]), &ctx));
    assert_eq!(found, vec!["echo b", "echo a"]);
    let found = scripts(&just(&argv(&["c", "b", "a", "1", "2"]), &ctx));
    assert_eq!(found.len(), 3);
    // Fewer words than parameters.
    let found = scripts(&just(&argv(&["b", "a", "1"]), &ctx));
    assert_eq!(found.len(), 2);
    // Many words after a recipe of two parameters.
    let (_f, ctx) = tree(&[(
        "justfile",
        "l:\n    echo l\nm:\n    echo m\na p q:\n    echo a\n",
    )]);
    let found = scripts(&just(&argv(&["l", "m", "a", "1", "2"]), &ctx));
    assert_eq!(found.len(), 3, "{found:?}");
    // Attributes make a recipe unreadable; `default` picks the default.
    for attr in ["[script]", "[working-directory: 'x']", "[no-cd]"] {
        let h = format!("{attr}\nr:\n    echo hi\n");
        let found = just_in(&[("justfile", &h)], &["r"]);
        assert!(opaque(&found), "{attr}");
    }
    let found = just_in(
        &[(
            "justfile",
            "first:\n    echo f\n[default]\nsecond:\n    echo s\n",
        )],
        &[],
    );
    assert_eq!(scripts(&found), vec!["echo s"]);
    // A recipe not marked default: the first in the file is.
    let found = just_in(
        &[("justfile", "first:\n    echo f\nsecond:\n    echo s\n")],
        &[],
    );
    assert_eq!(scripts(&found), vec!["echo f"]);
    // The attribute does not carry to the recipe after the next.
    let found = just_in(
        &[("justfile", "[no-cd]\na:\n    echo a\nb:\n    echo b\n")],
        &["b"],
    );
    assert_eq!(scripts(&found), vec!["echo b"]);
}

#[test]
fn just_interpolation_substitutes_literals_only() {
    let jf =
        parse_justfile("a := 'x y'\nb := \"z\"\nc := `cmd`\nd := 'unclosed\ne := ''\nf := 'q\"\n");
    assert_eq!(interpolate("echo {{a}}", &jf), "echo x y");
    assert_eq!(interpolate("echo {{ b }}!", &jf), "echo z!");
    assert_eq!(interpolate("{{a}}{{b}}", &jf), "x yz");
    assert_eq!(interpolate("echo {{c}}", &jf), format!("echo {UNRESOLVED}"));
    assert_eq!(interpolate("echo {{d}}", &jf), format!("echo {UNRESOLVED}"));
    assert_eq!(interpolate("echo {{e}}", &jf), "echo ");
    assert_eq!(interpolate("echo {{f}}", &jf), format!("echo {UNRESOLVED}"));
    assert_eq!(
        interpolate("echo {{nope}}", &jf),
        format!("echo {UNRESOLVED}")
    );
    assert_eq!(interpolate("echo {{a", &jf), format!("echo {UNRESOLVED}"));
    // `{{{{` is a literal `{{`.
    assert_eq!(interpolate("echo {{{{ x", &jf), "echo {{ x");
    assert_eq!(interpolate("ab {{{{ {{a}} cd", &jf), "ab {{ x y cd");
    assert_eq!(interpolate("no braces", &jf), "no braces");
    // A one-character value is no literal.
    let one = parse_justfile("a := \"\n");
    assert_eq!(interpolate("{{a}}", &one), UNRESOLVED);
    let mixed = parse_justfile("a := 'x\"\nb := \"x'\n");
    assert_eq!(interpolate("{{a}}", &mixed), UNRESOLVED);
    assert_eq!(interpolate("{{b}}", &mixed), UNRESOLVED);
}

#[test]
fn just_options_in_every_spelling() {
    let files = [
        ("justfile", "r:\n    echo r\ns:\n    echo s\n"),
        ("alt.just", "r:\n    echo alt\ns:\n    echo alt-s\n"),
        ("d/justfile", "r:\n    echo d\n"),
    ];
    let (_d, ctx) = tree(&files);
    let j = |args: &[&str]| scripts(&just(&argv(args), &ctx));
    assert_eq!(j(&["-f", "alt.just", "r"]), vec!["echo alt"]);
    assert_eq!(j(&["--justfile", "alt.just", "r"]), vec!["echo alt"]);
    assert_eq!(j(&["--justfile=alt.just", "r"]), vec!["echo alt"]);
    assert_eq!(j(&["s", "-f", "alt.just"]), vec!["echo alt-s"]);
    assert_eq!(j(&["s", "--justfile=alt.just"]), vec!["echo alt-s"]);
    assert_eq!(j(&["-d", "d", "r"]), vec!["echo d"]);
    assert_eq!(j(&["--working-directory", "d", "r"]), vec!["echo d"]);
    assert_eq!(j(&["r", "-d", "d"]), vec!["echo d"]);
    // A moved directory reads the recipes in it.
    assert!(matches!(
        just(&argv(&["-d", "d", "r"]), &ctx)[0],
        Embedded::In(..)
    ));
    assert!(matches!(just(&argv(&["r"]), &ctx)[0], Embedded::Script(_)));
    // `--set NAME VALUE` takes two words.
    assert_eq!(j(&["--set", "a", "b", "r"]), vec!["echo r"]);
    assert_eq!(j(&["s", "--set", "a", "b"]), vec!["echo s"]);
    assert_eq!(j(&["s", "--set", "a", "b", "r"]), vec!["echo r", "echo s"]);
    // Any other option is skipped.
    assert_eq!(j(&["--quiet", "r"]), vec!["echo r"]);
    assert_eq!(j(&["s", "-q", "--verbose", "r"]), vec!["echo r", "echo s"]);
    // A variable setting is no recipe.
    assert_eq!(j(&["r", "X=1"]), vec!["echo r"]);
    assert_eq!(j(&["X=1", "r", "Y=2", "s"]), vec!["echo s", "echo r"]);
    // Listings run nothing.
    for flag in [
        "-l",
        "--list",
        "--summary",
        "--dump",
        "--fmt",
        "--help",
        "-h",
        "--version",
        "-V",
        "--variables",
        "--init",
        "--edit",
        "-e",
        "--groups",
    ] {
        assert!(j(&[flag]).is_empty(), "{flag}");
        assert!(j(&["r", flag]).is_empty(), "r {flag}");
    }
    // Options that run another program are refused.
    for flag in [
        "--shell",
        "--shell-arg",
        "--command",
        "-c",
        "--choose",
        "--chooser",
    ] {
        let found = just(&argv(&[flag, "x"]), &ctx);
        assert!(why(&found).contains(&format!("`{flag}`")), "{flag}");
    }
    // A recipe the file does not define.
    let found = just(&argv(&["nope"]), &ctx);
    assert!(why(&found).contains("which its justfile does not define"));
    let found = just_in(&[("justfile", "r: ghost\n    echo r\n")], &["r"]);
    assert!(
        why(&found).contains("depends on `ghost`"),
        "{}",
        why(&found)
    );
    // No justfile.
    let found = just_in(&[("x", "")], &["r"]);
    assert!(why(&found).contains("finds no justfile"));
    // An empty justfile with no recipe runs nothing.
    let found = just_in(&[("justfile", "x := 'y'\n")], &[]);
    assert!(found.is_empty());
}

#[test]
fn just_stops_at_a_cycle_and_at_five_hundred_twelve_recipes() {
    let found = just_in(
        &[("justfile", "a: b\n    echo a\nb: a\n    echo b\n")],
        &["a"],
    );
    assert_eq!(scripts(&found), vec!["echo a", "echo b"]);
    let mut text = String::new();
    for k in 0..600 {
        text.push_str(&format!("t{k}: t{}\n    echo {k}\n", k + 1));
    }
    text.push_str("t600:\n    echo 600\n");
    let found = scripts(&just_in(&[("justfile", &text)], &["t0"]));
    assert_eq!(found.len(), 512);
    assert_eq!(found[511], "echo 511");
}

#[test]
fn just_recipe_lines_lose_their_silent_and_ignore_prefixes_and_run_their_backticks() {
    let f = "r:\n    @echo `date`\n    -rm x\n    @-ls\n";
    let found = scripts(&just_in(&[("justfile", f)], &["r"]));
    assert_eq!(found, vec!["date", "echo `date`", "rm x", "ls"]);
}

// ------------------------------------------------------------------ cargo

fn cargo_in(config: &[(&str, &str)], words: &[&str]) -> Option<Vec<Embedded>> {
    let (_d, ctx) = tree(config);
    cargo(&argv(words), 0, &ctx)
}

fn with_env(ctx: &ReadContext, name: &str, value: &str) -> ReadContext {
    ReadContext {
        env: vec![(name.to_owned(), value.to_owned())],
        ..ctx.clone()
    }
}

#[test]
fn cargo_reads_wrappers_runners_and_linkers_from_the_environment() {
    let (_d, ctx) = tree(&[("x", "")]);
    let build = argv(&["cargo", "build"]);
    for name in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC",
        "RUSTDOC",
        "CARGO_TARGET_X86_RUNNER",
        "CARGO_TARGET_X86_LINKER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTDOC",
    ] {
        let found = cargo(&build, 0, &with_env(&ctx, name, "w --a")).expect(name);
        assert_eq!(scripts(&found), vec!["w --a"], "{name}");
    }
    for name in [
        "CARGO_TARGET_X86_FLAGS",
        "X86_RUNNER",
        "X86_LINKER",
        "MY_RUNNER",
        "CARGO_BUILD_JOBS",
        "RUSTFLAGS",
        "RUSTC_FOO",
        "CARGO_TARGET_X86",
    ] {
        assert_eq!(cargo(&build, 0, &with_env(&ctx, name, "w")), None, "{name}");
    }
    let found = cargo(&build, 0, &with_env(&ctx, "CARGO_HOME", "/x")).expect("home");
    assert!(why(&found).contains("another `CARGO_HOME`"));
    // An alias from the environment.
    let alias = with_env(&ctx, "CARGO_ALIAS_MY_XT", "run -p x");
    let found = cargo(&argv(&["cargo", "my-xt", "a"]), 0, &alias).expect("alias");
    assert_eq!(scripts(&found), vec!["cargo run -p x a"]);
}

#[test]
fn cargo_config_flags_name_aliases_wrappers_and_runners() {
    let (_d, ctx) = tree(&[("x", "")]);
    let c = |config: &str| {
        let words = ["cargo", "--config", config, "build"];
        cargo(&argv(&words), 0, &ctx)
    };
    for (config, want) in [
        ("build.rustc-wrapper='sccache'", "sccache"),
        ("build.rustc-workspace-wrapper=w", "w"),
        ("target.x.runner=\"curl x\"", "curl x"),
        ("target.x.linker=cc", "cc"),
        ("build.rustc=r", "r"),
        ("build.rustdoc=d", "d"),
        (" build.rustc = r2", "r2"),
    ] {
        let found = c(config).expect(config);
        assert_eq!(scripts(&found), vec![want], "{config}");
    }
    for config in ["net.retry=2", "term.color='never'", "build.jobs=4"] {
        assert_eq!(c(config), None, "{config}");
    }
    // A file is not read.
    let found = c("file.toml").expect("file");
    assert!(why(&found).contains("configuration file"));
    let found = cargo(&argv(&["cargo", "--config=file.toml", "build"]), 0, &ctx).expect("f");
    assert!(why(&found).contains("configuration file"));
    // An alias given by `--config`, in both spellings, and one that is no
    // list or text.
    let found = cargo(
        &argv(&["cargo", "--config=alias.q=\"publish\"", "q"]),
        0,
        &ctx,
    );
    assert_eq!(scripts(&found.expect("alias")), vec!["cargo publish"]);
    let words = ["cargo", "--config", "alias.q = ['publish', '-n']", "q", "x"];
    assert_eq!(
        scripts(&cargo(&argv(&words), 0, &ctx).expect("alias")),
        vec!["cargo publish -n x"]
    );
    let found = c("alias.q=[1]").expect("bad alias");
    assert!(why(&found).contains("alias kGraf cannot read"));
    let found = c("alias.q=@@").expect("bad alias");
    assert!(why(&found).contains("alias kGraf cannot read"));
}

#[test]
fn cargo_skips_the_toolchain_and_options_before_the_command() {
    let config = [(
        ".cargo/config.toml",
        "[build]\nrustc-wrapper = \"sccache\"\n[alias]\nalways = \"publish\"\nunstable = \"publish\"\nm = \"publish\"\n",
    )];
    let (_d, ctx) = tree(&config);
    let c = |words: &[&str]| cargo(&argv(words), 0, &ctx).map(|f| scripts(&f));
    let sccache = Some(vec!["sccache".to_owned()]);
    assert_eq!(c(&["cargo", "+nightly", "build"]), sccache);
    assert_eq!(
        c(&["cargo", "--locked", "--color", "always", "build"]),
        sccache
    );
    assert_eq!(
        c(&["cargo", "--locked", "-Z", "unstable", "build"]),
        sccache
    );
    assert_eq!(
        c(&["cargo", "--locked", "--manifest-path", "m", "build"]),
        sccache
    );
    assert_eq!(
        c(&["cargo", "+n", "--locked", "--offline", "-q", "build"]),
        sccache
    );
    assert_eq!(
        c(&[
            "cargo",
            "--color",
            "always",
            "-Z",
            "x",
            "--manifest-path",
            "m",
            "build"
        ]),
        sccache
    );
    // `-C DIR` moves cargo somewhere kGraf cannot follow.
    let found = cargo(&argv(&["cargo", "-C", "sub", "build"]), 0, &ctx).expect("moved");
    assert!(
        why(&found).contains("kGraf cannot tell which directory cargo runs in"),
        "{}",
        why(&found)
    );
    // No sub-command at all.
    assert_eq!(c(&["cargo"]), None);
    assert_eq!(c(&["cargo", "--version"]), None);
    // `cargo fmt` is cargo's own and reads no configuration.
    assert_eq!(c(&["cargo", "fmt"]), None);
}

#[test]
fn cargo_aliases_come_from_the_nearest_configuration_and_builtin_aliases_run_as_the_command() {
    let (_d, ctx) = tree(&[
        (
            ".cargo/config.toml",
            "[alias]\nxt = \"run -p root\"\n[build]\nrustc-wrapper = \"sccache\"\n[target.x]\nrunner = { path = \"curl\", args = [\"x\"] }\n[target.y]\nlinker = [\"cc\", \"-v\"]\n",
        ),
        ("sub/.cargo/config", "[alias]\nxt = \"run -p sub\"\n"),
        ("other/x", ""),
    ]);
    let sub = ctx.moved_to("sub");
    let found = cargo(&argv(&["cargo", "xt"]), 0, &sub).expect("sub");
    assert_eq!(scripts(&found), vec!["cargo run -p sub"]);
    let other = ctx.moved_to("other");
    let found = cargo(&argv(&["cargo", "xt"]), 0, &other).expect("other");
    assert_eq!(scripts(&found), vec!["cargo run -p root"]);
    // Built-in aliases compile or run like their command.
    let t = scripts(&cargo(&argv(&["cargo", "t"]), 0, &ctx).expect("t"));
    assert_eq!(t, vec!["sccache", "cc -v", "curl x"]);
    let r = scripts(&cargo(&argv(&["cargo", "r"]), 0, &ctx).expect("r"));
    assert_eq!(r, vec!["sccache", "cc -v", "curl x"]);
    let b = scripts(&cargo(&argv(&["cargo", "b"]), 0, &ctx).expect("b"));
    assert_eq!(b, vec!["sccache", "cc -v"]);
    let bench = scripts(&cargo(&argv(&["cargo", "bench"]), 0, &ctx).expect("bench"));
    assert_eq!(bench, vec!["sccache", "cc -v", "curl x"]);
    assert_eq!(cargo(&argv(&["cargo", "fetch"]), 0, &ctx), None);
    // A configured alias may override a built-in alias.
    let (_d, own) = tree(&[(".cargo/config.toml", "[alias]\nb = \"publish\"\n")]);
    let found = cargo(&argv(&["cargo", "b"]), 0, &own).expect("own");
    assert_eq!(scripts(&found), vec!["cargo publish"]);
}

#[test]
fn cargo_without_a_directory_says_what_it_cannot_read() {
    let none = ReadContext::default();
    let found = cargo(&argv(&["cargo", "xt"]), 0, &none).expect("xt");
    assert!(why(&found).contains("may be an alias"), "{}", why(&found));
    // A built-in alias is not an unknown name.
    let found = cargo(&argv(&["cargo", "b"]), 0, &none).expect("b");
    let text = why(&found);
    assert!(text.starts_with("`cargo b`:"), "{text}");
    assert!(!text.contains("may be an alias"), "{text}");
    let found = cargo(&argv(&["cargo", "build"]), 0, &none).expect("build");
    assert!(why(&found).starts_with("`cargo build`:"), "{}", why(&found));
}

#[test]
fn a_bad_cargo_configuration_is_unreadable_and_a_table_program_is_read() {
    for (text, part) in [
        ("[alias\n", "does not parse"),
        ("include = 'x'\n", "includes another file"),
        ("[alias]\nxt = 5\n", "has an alias `xt` kGraf cannot read"),
    ] {
        let found = cargo_in(&[(".cargo/config.toml", text)], &["cargo", "build"]).expect(part);
        assert!(why(&found).contains(part), "{part}: {}", why(&found));
    }
    assert_eq!(
        program_value(&toml::Value::Table(
            toml::from_str("path = 'p'\nargs = ['a', 'b']").expect("table")
        )),
        Some(argv(&["p", "a", "b"]))
    );
    assert_eq!(
        program_value(&toml::Value::Table(
            toml::from_str("path = 'p'").expect("table")
        )),
        Some(argv(&["p"]))
    );
    assert_eq!(program_value(&toml::Value::Integer(1)), None);
    assert_eq!(
        program_value(&toml::Value::String("a  b".to_owned())),
        Some(argv(&["a", "b"]))
    );
}
