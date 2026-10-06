//! Build-tool recipes are shell commands (decision 0094, amended
//! 2026-10-06): `make TARGET` runs the recipe lines of the target and of its
//! prerequisites, `npm run X` (and `npm test`, `pnpm`, `yarn`, `bun run`)
//! runs the `package.json` script with its `pre` and `post` scripts, `just
//! RECIPE` runs the justfile recipe and its dependencies, and `cargo NAME`
//! runs the alias `.cargo/config.toml` defines, through the runner and
//! wrappers it names. Each recipe line is returned as a script, so the
//! user's rules, the `forbid` rules, the deletion limit and the grants judge
//! it as a command. A missing or unparseable recipe file, and what kGraf
//! cannot place, is unreadable.
//!
//! The bounds (decision 0094):
//!
//! - Files are read from the command's directory only, never from a parent
//!   (npm, just and cargo search parents; a recipe kGraf does not find there
//!   leaves the command unreadable, cargo's built-in commands aside).
//! - make: GNU make's syntax for rules, recipes, variables (`=`, `:=`,
//!   `::=`, `?=`, `+=`, `!=`), `$(shell ...)`, automatic variables, `-C`,
//!   `-f`. Unreadable: `include`, `$(eval ...)`, `.RECIPEPREFIX`, a `SHELL`
//!   that is not a POSIX shell, and a variable kGraf cannot resolve where a
//!   program stands. Both branches of a conditional are read. Pattern rules'
//!   recipes are read for any target without a rule of its own; make's
//!   built-in implicit rules (which run `cc`, `as` and the like) are not.
//! - npm, pnpm, yarn, bun: the named script and its `pre`/`post` scripts.
//!   Unreadable: workspace and directory options (`-w`, `--prefix`,
//!   `--filter`, `-r`), a `script-shell` set in the directory's `.npmrc`.
//!   `npm install` and the lifecycle scripts of dependencies are not read.
//! - just: recipes, dependencies, parameters, `{{ }}` interpolation of
//!   literal variables, backticks. Unreadable: `import`, `mod`, a shebang
//!   or `[script]` recipe, `set shell` that is not a POSIX shell,
//!   `[working-directory]`.
//! - cargo: `[alias]` of `.cargo/config.toml` (or `.cargo/config`) in the
//!   directories from the command's up to the worktree root, `--config
//!   alias.X=...`, `CARGO_ALIAS_X=`; the `runner` of `[target.*]` for `run`,
//!   `test` and `bench`; `build.rustc-wrapper`, `build.rustc-workspace-
//!   wrapper`, `build.rustc`, `build.rustdoc` and `[target.*] linker` for the
//!   commands that compile. What cargo builds and runs (`build.rs`, tests)
//!   is not read, as before.

use super::{ContentCache, Embedded, ReadContext, read_worktree_file};
use std::collections::{BTreeMap, BTreeSet};

fn unreadable(program: &str, why: &str) -> Vec<Embedded> {
    vec![Embedded::Unreadable(format!("`{program}` {why}"))]
}

/// A placeholder for a value kGraf could not resolve. It holds `$`, so a
/// program word holding it is unreadable, a script holding it is not judged
/// command by command, and a deletion naming it is asked about.
const UNRESOLVED: &str = "$__unresolved";

/// Whether `shell` (a `SHELL` or `set shell` value) is a POSIX shell whose
/// `-c` script kGraf reads.
fn posix_shell(shell: &str) -> bool {
    let first = shell.split_whitespace().next().unwrap_or_default();
    matches!(
        super::base_name(first),
        "sh" | "bash" | "dash" | "zsh" | "ksh"
    )
}

// ---------------------------------------------------------------- make

/// One rule of a Makefile.
#[derive(Clone, Debug, Default)]
struct MakeRule {
    prerequisites: Vec<String>,
    recipe: Vec<String>,
}

/// A Makefile read for the permission check.
#[derive(Debug, Default)]
struct Makefile {
    /// Rules by target; a target named by several rules has their
    /// prerequisites and recipes joined.
    rules: BTreeMap<String, MakeRule>,
    /// Pattern rules (`%.o: %.c`), whose recipes run for a target without
    /// a rule of its own.
    patterns: Vec<MakeRule>,
    /// Each variable's definitions, in order (`+=` appends to the last).
    vars: BTreeMap<String, Vec<String>>,
    /// Commands that run while make reads the file (`$(shell ...)`, `!=`).
    at_parse: Vec<String>,
    /// The first ordinary target.
    default_goal: Option<String>,
    /// Why the file cannot be read, when it cannot.
    opaque: Option<String>,
}

static MAKEFILES: ContentCache<Makefile> = ContentCache::new();

/// Where a make variable reference or function ends: the index after its
/// closing bracket in `chars` from `start` (just after `$(` or `${`).
fn closing(chars: &[char], start: usize, open: char) -> Option<usize> {
    let close = if open == '(' { ')' } else { '}' };
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() {
        if chars[i] == open {
            depth += 1;
        } else if chars[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

/// Every `$(shell ...)` in `text`, with its command.
fn shell_calls(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == '$'
            && (chars[i + 1] == '(' || chars[i + 1] == '{')
            && let Some(end) = closing(&chars, i + 2, chars[i + 1])
        {
            let inner: String = chars[i + 2..end - 1].iter().collect();
            if let Some(cmd) = inner.strip_prefix("shell ") {
                out.push(cmd.trim().to_owned());
            }
            out.extend(shell_calls(&inner));
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// The position of the rule colon in a Makefile line, outside variable
/// references; `None` when the line is no rule. A `:` that begins `:=` or
/// `::=` is an assignment's.
fn rule_colon(line: &str) -> Option<usize> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut byte = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$'
            && matches!(chars.get(i + 1), Some('(') | Some('{'))
            && let Some(end) = closing(&chars, i + 2, chars[i + 1])
        {
            byte += chars[i..end].iter().map(|c| c.len_utf8()).sum::<usize>();
            i = end;
            continue;
        }
        if c == '=' {
            return None;
        }
        if c == ':' {
            let next: String = chars[i + 1..].iter().take(2).collect();
            if next.starts_with('=') || next == ":=" {
                return None;
            }
            return Some(byte);
        }
        byte += c.len_utf8();
        i += 1;
    }
    None
}

/// The assignment in a Makefile line: name, operator, value.
fn make_assignment(line: &str) -> Option<(String, &'static str, String)> {
    let line = line
        .trim_start_matches("export ")
        .trim_start_matches("override ")
        .trim_start_matches("private ");
    for op in ["::=", ":::=", ":=", "?=", "+=", "!=", "="] {
        if let Some(at) = line.find(op) {
            let name = line[..at].trim();
            if name.is_empty()
                || name.contains(char::is_whitespace)
                || name.contains(['(', ')', '{', '}', '$', ':'])
            {
                // `x: y = z` (a target-specific assignment) or no name.
                continue;
            }
            // The first operator in the line is the assignment's.
            let first = ["::=", ":::=", ":=", "?=", "+=", "!=", "="]
                .iter()
                .filter_map(|o| line.find(o).map(|p| (p, *o)))
                .min_by_key(|(p, o)| (*p, std::cmp::Reverse(o.len())));
            if let Some((p, o)) = first
                && p == at
                && o == op
            {
                return Some((name.to_owned(), op, line[at + op.len()..].trim().to_owned()));
            }
        }
    }
    None
}

fn parse_makefile(text: &str) -> Makefile {
    let mut mf = Makefile::default();
    // Lines joined at a backslash before the newline.
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for raw in text.lines() {
        if let Some(stripped) = raw.strip_suffix('\\') {
            cur.push_str(stripped);
            cur.push(' ');
            continue;
        }
        cur.push_str(raw);
        lines.push(std::mem::take(&mut cur));
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    let mut current: Vec<String> = Vec::new();
    let mut current_is_pattern = false;
    let mut in_define: Option<String> = None;
    // Whether the variable `define` opened has a line yet.
    let mut define_open = false;
    for line in lines {
        if let Some(name) = &in_define {
            if line.trim() == "endef" {
                in_define = None;
            } else {
                // A multi-line variable (a canned recipe): one definition,
                // its lines joined with newlines.
                let entry = mf.vars.entry(name.clone()).or_default();
                match entry.last_mut() {
                    Some(last) if define_open => {
                        last.push('\n');
                        last.push_str(line.trim_start_matches('\t'));
                    }
                    _ => entry.push(line.trim_start_matches('\t').to_owned()),
                }
                define_open = true;
                mf.at_parse.extend(shell_calls(&line));
            }
            continue;
        }
        if let Some(recipe) = line.strip_prefix('\t') {
            if !current.is_empty() {
                let target = if current_is_pattern {
                    None
                } else {
                    Some(current.clone())
                };
                match target {
                    Some(targets) => {
                        for t in targets {
                            mf.rules
                                .entry(t)
                                .or_default()
                                .recipe
                                .push(recipe.to_owned());
                        }
                    }
                    None => {
                        if let Some(p) = mf.patterns.last_mut() {
                            p.recipe.push(recipe.to_owned());
                        }
                    }
                }
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let word = trimmed.split_whitespace().next().unwrap_or_default();
        match word {
            "include" | "-include" | "sinclude" => {
                mf.opaque.get_or_insert_with(|| {
                    "includes another makefile, which kGraf does not read".into()
                });
                continue;
            }
            "ifeq" | "ifneq" | "ifdef" | "ifndef" | "else" | "endif" => continue,
            "define" => {
                let name = trimmed
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .trim_end_matches(['=', ':', '?', '+', '!'])
                    .to_owned();
                in_define = Some(name);
                define_open = false;
                continue;
            }
            "vpath" | "export" | "unexport" if !trimmed.contains('=') => continue,
            _ => {}
        }
        if trimmed.contains("$(eval") || trimmed.contains("${eval") {
            mf.opaque
                .get_or_insert_with(|| "uses `$(eval ...)`, which kGraf does not read".into());
        }
        mf.at_parse.extend(shell_calls(trimmed));
        if let Some(colon) = rule_colon(trimmed) {
            let targets_text = &trimmed[..colon];
            let mut after = trimmed[colon + 1..].trim_start_matches(':').to_owned();
            // `targets: prerequisites ; recipe`.
            let inline = after
                .split_once(';')
                .map(|(p, r)| (p.to_owned(), r.to_owned()));
            if let Some((p, _)) = &inline {
                after = p.clone();
            }
            // `target: VAR = value` is a target-specific variable.
            if make_assignment(&after).is_some() {
                if let Some((name, _, value)) = make_assignment(&after) {
                    mf.vars.entry(name).or_default().push(value);
                }
                current.clear();
                continue;
            }
            // Order-only prerequisites after `|` are prerequisites too.
            let prerequisites: Vec<String> = after
                .split_whitespace()
                .filter(|w| *w != "|")
                .map(str::to_owned)
                .collect();
            let targets: Vec<String> = targets_text.split_whitespace().map(str::to_owned).collect();
            current_is_pattern = targets.iter().any(|t| t.contains('%'));
            if current_is_pattern {
                mf.patterns.push(MakeRule {
                    prerequisites,
                    recipe: inline.iter().map(|(_, r)| r.trim().to_owned()).collect(),
                });
            } else {
                for t in &targets {
                    if t == ".RECIPEPREFIX" {
                        mf.opaque.get_or_insert_with(|| {
                            "sets `.RECIPEPREFIX`, which kGraf does not read".into()
                        });
                    }
                    let rule = mf.rules.entry(t.clone()).or_default();
                    rule.prerequisites.extend(prerequisites.iter().cloned());
                    if let Some((_, r)) = &inline {
                        rule.recipe.push(r.trim().to_owned());
                    }
                    if mf.default_goal.is_none() && !t.starts_with('.') {
                        mf.default_goal = Some(t.clone());
                    }
                }
            }
            current = targets;
            continue;
        }
        if let Some((name, op, value)) = make_assignment(trimmed) {
            current.clear();
            if name == ".RECIPEPREFIX" {
                mf.opaque.get_or_insert_with(|| {
                    "sets `.RECIPEPREFIX`, which kGraf does not read".into()
                });
            }
            match op {
                "!=" => {
                    mf.at_parse.push(value.clone());
                    mf.vars.entry(name).or_default().push(UNRESOLVED.to_owned());
                }
                "+=" => {
                    let entry = mf.vars.entry(name).or_default();
                    match entry.last_mut() {
                        Some(last) => {
                            last.push(' ');
                            last.push_str(&value);
                        }
                        None => entry.push(value),
                    }
                }
                _ => mf.vars.entry(name).or_default().push(value),
            }
            continue;
        }
        // Anything else (a directive kGraf does not know) ends the rule.
        current.clear();
    }
    if let Some(goal) = mf.vars.get(".DEFAULT_GOAL").and_then(|v| v.last()) {
        mf.default_goal = Some(goal.trim().to_owned());
    }
    if let Some(shell) = mf.vars.get("SHELL")
        && shell.iter().any(|s| !posix_shell(s))
    {
        mf.opaque
            .get_or_insert_with(|| "sets `SHELL` to a shell kGraf does not read".into());
    }
    mf
}

/// The context of one recipe line: its rule's targets and prerequisites,
/// for the automatic variables.
struct RecipeContext<'a> {
    target: &'a str,
    prerequisites: &'a [String],
    pattern: bool,
}

/// Make's built-in variables a recipe commonly names as its program.
fn builtin_var(name: &str) -> Option<&'static str> {
    Some(match name {
        "MAKE" => "make",
        "CC" => "cc",
        "CXX" => "c++",
        "CPP" => "cc -E",
        "RM" => "rm -f",
        "AR" => "ar",
        "AS" => "as",
        "LD" => "ld",
        "LEX" => "lex",
        "YACC" => "yacc",
        "FC" => "f77",
        "SHELL" => "/bin/sh",
        _ => return None,
    })
}

/// Marks an expansion whose text kGraf cannot know and which may hold shell
/// syntax (a variable defined more than once with a `;` in one value, a
/// function call whose arguments hold one): the line it is in may run
/// another command, so it is unreadable.
const CANNOT_READ: char = '\u{1}';

/// Whether `text` holds shell syntax that could start another command.
fn holds_shell_syntax(text: &str) -> bool {
    text.contains([';', '&', '|', '`', '<', '>', '\n', '(', ')'])
}

/// Expand make's references in `text`: variables defined once, make's
/// built-in programs, `$$` and the automatic variables. An undefined
/// variable is empty, as in make (a value given before `make` on its
/// command line is an override). A variable defined more than once, or a
/// function call, becomes [`UNRESOLVED`], or [`CANNOT_READ`] when its text
/// may hold shell syntax.
fn expand(
    text: &str,
    mf: &Makefile,
    overrides: &BTreeMap<String, String>,
    rc: Option<&RecipeContext<'_>>,
    depth: usize,
) -> String {
    if depth > 16 {
        return UNRESOLVED.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let value_of = |name: &str| -> String {
        if let Some(v) = overrides.get(name) {
            return expand(v, mf, overrides, rc, depth + 1);
        }
        if let Some(rc) = rc {
            let dir_file = |s: &str, part: char| -> String {
                let (d, f) = s.rsplit_once('/').unwrap_or((".", s));
                if part == 'D' {
                    d.to_owned()
                } else {
                    f.to_owned()
                }
            };
            let first_prereq = rc.prerequisites.first().cloned().unwrap_or_default();
            match name {
                "@" if !rc.pattern => return rc.target.to_owned(),
                "<" if !rc.pattern => return first_prereq,
                "^" | "+" | "?" if !rc.pattern => return rc.prerequisites.join(" "),
                "@D" | "@F" if !rc.pattern => {
                    return dir_file(rc.target, name.chars().nth(1).unwrap_or('F'));
                }
                "<D" | "<F" if !rc.pattern => {
                    return dir_file(&first_prereq, name.chars().nth(1).unwrap_or('F'));
                }
                _ => {}
            }
        }
        match mf.vars.get(name).map(Vec::as_slice) {
            Some([one]) => expand(one, mf, overrides, rc, depth + 1),
            Some(several)
                if several.iter().any(|v| {
                    let e = expand(v, mf, overrides, rc, depth + 1);
                    e.contains(CANNOT_READ) || holds_shell_syntax(&e)
                }) =>
            {
                CANNOT_READ.to_string()
            }
            Some(_) => UNRESOLVED.to_owned(),
            // An automatic variable kGraf cannot fill (a pattern rule's
            // target or stem).
            None if rc.is_some()
                && matches!(
                    name,
                    "@" | "<"
                        | "^"
                        | "+"
                        | "?"
                        | "*"
                        | "|"
                        | "@D"
                        | "@F"
                        | "<D"
                        | "<F"
                        | "*D"
                        | "*F"
                ) =>
            {
                UNRESOLVED.to_owned()
            }
            None => builtin_var(name).unwrap_or_default().to_owned(),
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('$') => {
                out.push('$');
                i += 2;
            }
            Some(&open @ ('(' | '{')) => match closing(&chars, i + 2, open) {
                Some(end) => {
                    let inner: String = chars[i + 2..end - 1].iter().collect();
                    if inner.contains(char::is_whitespace) || inner.contains(',') {
                        // A function call (`$(wildcard ...)`, `$(shell
                        // ...)`): its value is not resolved. `$(call ...)`
                        // and `$(value ...)` expand a variable's text, and a
                        // call whose arguments hold shell syntax may produce
                        // it.
                        let function = inner.split_whitespace().next().unwrap_or_default();
                        let arguments =
                            expand(&inner[function.len()..], mf, overrides, rc, depth + 1);
                        if matches!(function, "call" | "value" | "eval" | "foreach" | "file")
                            || arguments.contains(CANNOT_READ)
                            || holds_shell_syntax(&arguments)
                        {
                            out.push(CANNOT_READ);
                        } else {
                            out.push_str(UNRESOLVED);
                        }
                    } else {
                        out.push_str(&value_of(&inner));
                    }
                    i = end;
                }
                None => {
                    out.push_str(UNRESOLVED);
                    i = chars.len();
                }
            },
            Some(&single) => {
                out.push_str(&value_of(&single.to_string()));
                i += 2;
            }
            None => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

/// `make [OPTIONS] [VAR=VALUE] [TARGET...]`.
pub(super) fn make(prefix: &[String], args: &[String], ctx: &ReadContext) -> Vec<Embedded> {
    let mut file: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut overrides: BTreeMap<String, String> = BTreeMap::new();
    // `CFLAGS=... make`, `export CFLAGS=...; make`: make reads its
    // environment as variables.
    let _ = prefix;
    for (name, value) in &ctx.env {
        overrides.insert(name.clone(), value.clone());
    }
    let mut goals: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let next = args.get(i + 1).cloned();
        let take = |name: &str, short: &str| -> Option<(Option<String>, usize)> {
            if a == short || a == name {
                return Some((next.clone(), 2));
            }
            if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                return Some((Some(v.to_owned()), 1));
            }
            if short.len() == 2 && a.starts_with(short) && a.len() > 2 && !a.starts_with("--") {
                return Some((Some(a[2..].to_owned()), 1));
            }
            None
        };
        if let Some((v, n)) = take("--file", "-f").or_else(|| take("--makefile", "-f")) {
            file = v;
            i += n;
            continue;
        }
        if let Some((v, n)) = take("--directory", "-C") {
            dir = match (dir, v) {
                (Some(d), Some(v)) => Some(format!("{d}/{v}")),
                (_, v) => v,
            };
            i += n;
            continue;
        }
        if a == "--eval" || a.starts_with("--eval=") || a == "-E" {
            return unreadable("make", "is given `--eval`, which kGraf does not read");
        }
        if let Some((_, n)) = take("--include-dir", "-I")
            .or_else(|| take("--old-file", "-o"))
            .or_else(|| take("--what-if", "-W"))
            .or_else(|| take("--assume-old", "-o"))
            .or_else(|| take("--assume-new", "-W"))
        {
            i += n;
            continue;
        }
        if a == "-j" || a == "-l" {
            // An optional number follows.
            i += if next.as_deref().is_some_and(|n| n.parse::<f64>().is_ok()) {
                2
            } else {
                1
            };
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        if let Some((name, value)) = a.split_once('=')
            && super::is_assignment(a)
        {
            overrides.insert(
                name.trim_end_matches([':', '+', '?', '!']).to_owned(),
                value.to_owned(),
            );
            i += 1;
            continue;
        }
        goals.push(a.clone());
        i += 1;
    }
    let ctx = match &dir {
        Some(d) => ctx.moved_to(d),
        None => ctx.clone(),
    };
    let names = ["GNUmakefile", "makefile", "Makefile"];
    let name = match &file {
        Some(f) => f.clone(),
        None => match names.iter().find(|n| super::worktree_file_exists(&ctx, n)) {
            Some(n) => (*n).to_owned(),
            None if ctx.cwd.is_none() => {
                return unreadable("make", "runs a makefile from a directory kGraf cannot tell");
            }
            None => return unreadable("make", "finds no makefile kGraf can read"),
        },
    };
    let read = match read_worktree_file(&ctx, &name) {
        Ok(r) => r,
        Err(why) => return unreadable("make", &format!("cannot read its makefile: {why}")),
    };
    let mf = MAKEFILES.get("make", &read.text, || parse_makefile(&read.text));
    if let Some(why) = &mf.opaque {
        return unreadable("make", &format!("runs a makefile that {why}"));
    }
    let mut lines: Vec<String> = mf
        .at_parse
        .iter()
        .map(|c| expand(c, &mf, &overrides, None, 0))
        .collect();
    if goals.is_empty() {
        match &mf.default_goal {
            Some(g) => goals.push(g.clone()),
            None => {
                if lines.iter().any(|l| l.contains(CANNOT_READ)) {
                    return unreadable("make", "runs a command kGraf cannot know");
                }
                return wrap(
                    &ctx,
                    dir.is_some(),
                    lines.into_iter().map(Embedded::Script).collect(),
                );
            }
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = goals;
    let mut patterns_read = false;
    while let Some(t) = queue.pop() {
        if !seen.insert(t.clone()) || seen.len() > 512 {
            continue;
        }
        match mf.rules.get(&t) {
            Some(rule) => {
                let rc = RecipeContext {
                    target: &t,
                    prerequisites: &rule.prerequisites,
                    pattern: false,
                };
                for line in &rule.recipe {
                    // `$(shell ...)` in a recipe line runs when make
                    // expands it.
                    for cmd in shell_calls(line) {
                        lines.push(expand(&cmd, &mf, &overrides, Some(&rc), 0));
                    }
                    lines.push(expand(line, &mf, &overrides, Some(&rc), 0));
                }
                for p in &rule.prerequisites {
                    queue.push(expand(p, &mf, &overrides, None, 0));
                }
                if rule.recipe.is_empty() && !mf.patterns.is_empty() && !patterns_read {
                    patterns_read = true;
                    push_patterns(&mf, &overrides, &mut lines, &mut queue);
                }
            }
            None if !patterns_read && !mf.patterns.is_empty() => {
                patterns_read = true;
                push_patterns(&mf, &overrides, &mut lines, &mut queue);
            }
            None => {}
        }
    }
    if lines.iter().any(|l| l.contains(CANNOT_READ)) {
        return unreadable(
            "make",
            "runs a recipe line built from a value kGraf cannot know, which may hold shell \
             syntax",
        );
    }
    // A canned recipe expands to several lines, each a command line.
    let found = lines
        .iter()
        .flat_map(|l| l.lines())
        .map(strip_recipe_prefix)
        .filter(|l| !l.is_empty())
        .map(Embedded::Script)
        .collect();
    wrap(&ctx, dir.is_some(), found)
}

fn push_patterns(
    mf: &Makefile,
    overrides: &BTreeMap<String, String>,
    lines: &mut Vec<String>,
    queue: &mut Vec<String>,
) {
    for p in &mf.patterns {
        let rc = RecipeContext {
            target: "",
            prerequisites: &p.prerequisites,
            pattern: true,
        };
        for line in &p.recipe {
            for cmd in shell_calls(line) {
                lines.push(expand(&cmd, mf, overrides, Some(&rc), 0));
            }
            lines.push(expand(line, mf, overrides, Some(&rc), 0));
        }
        for pre in &p.prerequisites {
            if !pre.contains('%') {
                queue.push(expand(pre, mf, overrides, None, 0));
            }
        }
    }
}

/// A recipe line without make's prefixes: `@` (silent), `-` (ignore
/// errors), `+` (run under `-n` too).
fn strip_recipe_prefix(line: &str) -> String {
    line.trim()
        .trim_start_matches(['@', '-', '+', ' ', '\t'])
        .trim()
        .to_owned()
}

/// `found`, read in `ctx` when the tool moved to another directory.
fn wrap(ctx: &ReadContext, moved: bool, found: Vec<Embedded>) -> Vec<Embedded> {
    if !moved {
        return found;
    }
    found
        .into_iter()
        .map(|f| Embedded::In(ctx.clone(), Box::new(f)))
        .collect()
}

// ------------------------------------------------- package.json scripts

/// A `package.json` read for the permission check: its scripts, or why it
/// cannot be read.
type PackageScripts = Result<BTreeMap<String, String>, String>;

static PACKAGES: ContentCache<PackageScripts> = ContentCache::new();

fn parse_package(text: &str) -> PackageScripts {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("its `package.json` does not parse: {e}"))?;
    let mut out = BTreeMap::new();
    if let Some(scripts) = value.get("scripts").and_then(|s| s.as_object()) {
        for (k, v) in scripts {
            match v.as_str() {
                Some(s) => {
                    out.insert(k.clone(), s.to_owned());
                }
                None => return Err(format!("its script `{k}` is not text")),
            }
        }
    }
    Ok(out)
}

/// Built-in commands of each tool, which run no script of the same name.
fn builtin(tool: &str, sub: &str) -> bool {
    let list: &[&str] = match tool {
        "npm" => &[
            "access",
            "adduser",
            "audit",
            "bugs",
            "cache",
            "ci",
            "completion",
            "config",
            "dedupe",
            "deprecate",
            "diff",
            "dist-tag",
            "docs",
            "doctor",
            "edit",
            "explain",
            "explore",
            "find-dupes",
            "fund",
            "get",
            "help",
            "help-search",
            "hook",
            "init",
            "install",
            "install-ci-test",
            "install-test",
            "i",
            "it",
            "link",
            "ll",
            "login",
            "logout",
            "ls",
            "org",
            "outdated",
            "owner",
            "pack",
            "ping",
            "pkg",
            "prefix",
            "profile",
            "prune",
            "publish",
            "query",
            "rebuild",
            "repo",
            "root",
            "sbom",
            "search",
            "set",
            "shrinkwrap",
            "star",
            "stars",
            "team",
            "token",
            "undeprecate",
            "uninstall",
            "unpublish",
            "unstar",
            "update",
            "version",
            "view",
            "whoami",
            "add",
            "remove",
            "rm",
            "un",
            "up",
            "list",
            "exec",
            "x",
            "create",
            "innit",
            "ln",
            "show",
            "info",
            "v",
        ],
        "pnpm" => &[
            "add",
            "install",
            "i",
            "update",
            "up",
            "remove",
            "rm",
            "un",
            "uninstall",
            "link",
            "ln",
            "unlink",
            "import",
            "rebuild",
            "rb",
            "prune",
            "fetch",
            "patch",
            "patch-commit",
            "patch-remove",
            "audit",
            "list",
            "ls",
            "ll",
            "outdated",
            "why",
            "licenses",
            "exec",
            "dlx",
            "create",
            "publish",
            "pack",
            "store",
            "root",
            "bin",
            "setup",
            "init",
            "deploy",
            "doctor",
            "config",
            "env",
            "server",
            "recursive",
            "dedupe",
            "approve-builds",
            "self-update",
            "cat-file",
            "cat-index",
            "find-hash",
            "help",
            "ignored-builds",
            "sbom",
        ],
        "yarn" => &[
            "add",
            "audit",
            "autoclean",
            "bin",
            "cache",
            "check",
            "config",
            "create",
            "dedupe",
            "dlx",
            "exec",
            "explain",
            "generate-lock-entry",
            "global",
            "help",
            "import",
            "info",
            "init",
            "install",
            "licenses",
            "link",
            "list",
            "login",
            "logout",
            "node",
            "npm",
            "outdated",
            "owner",
            "pack",
            "patch",
            "patch-commit",
            "plugin",
            "policies",
            "publish",
            "rebuild",
            "remove",
            "search",
            "set",
            "stage",
            "tag",
            "team",
            "unlink",
            "unplug",
            "up",
            "upgrade",
            "upgrade-interactive",
            "version",
            "versions",
            "why",
            "workspace",
            "workspaces",
            "constraints",
            "dedupe",
        ],
        "bun" => &[
            "install",
            "i",
            "add",
            "a",
            "remove",
            "rm",
            "update",
            "link",
            "unlink",
            "pm",
            "init",
            "create",
            "c",
            "upgrade",
            "build",
            "test",
            "x",
            "repl",
            "exec",
            "outdated",
            "publish",
            "audit",
            "info",
            "patch",
            "patch-commit",
            "completions",
            "help",
            "why",
            "discord",
        ],
        _ => &[],
    };
    list.contains(&sub)
}

/// The script an npm-family subcommand runs: `run X`, `run-script X`,
/// `test`, `start`, `stop`, `restart`, or (for pnpm, yarn and bun) a name
/// that is no built-in command. `None` when the subcommand runs no script.
pub(super) fn package_scripts(
    tool: &str,
    args: &[String],
    ctx: &ReadContext,
) -> Option<Vec<Embedded>> {
    // Options before the subcommand; a workspace or directory option means
    // another `package.json` runs.
    let mut i = 0;
    let mut elsewhere = false;
    while let Some(a) = args.get(i) {
        if !a.starts_with('-') {
            break;
        }
        let takes = matches!(
            a.as_str(),
            "-C" | "--dir" | "--prefix" | "--cwd" | "-w" | "--workspace" | "--filter" | "-F"
        );
        if takes
            || matches!(
                a.as_str(),
                "-r" | "--recursive" | "-ws" | "--workspaces" | "--bun"
            ) && a != "--bun"
            || a.starts_with("--prefix=")
            || a.starts_with("--dir=")
            || a.starts_with("--cwd=")
            || a.starts_with("--workspace=")
            || a.starts_with("--filter=")
        {
            elsewhere = true;
        }
        i += if takes { 2 } else { 1 };
    }
    let sub = args.get(i)?.as_str();
    let rest = &args[i + 1..];
    // npm takes its options anywhere on the line, before `--`.
    if rest.iter().take_while(|a| *a != "--").any(|a| {
        matches!(
            a.as_str(),
            "-w" | "--workspace"
                | "-ws"
                | "--workspaces"
                | "--prefix"
                | "-C"
                | "--dir"
                | "--filter"
                | "-F"
                | "-r"
                | "--recursive"
                | "--cwd"
        ) || ["--workspace=", "--prefix=", "--filter=", "--dir=", "--cwd="]
            .iter()
            .any(|p| a.starts_with(p))
    }) {
        elsewhere = true;
    }
    let (name, extra): (String, &[String]) = match (tool, sub) {
        (_, "run" | "run-script" | "rum" | "urn") => {
            // `run [OPTIONS] NAME [-- ARGS]`.
            let mut j = 0;
            while let Some(a) = rest.get(j) {
                if !a.starts_with('-') {
                    break;
                }
                if matches!(
                    a.as_str(),
                    "-w" | "--workspace" | "--filter" | "-F" | "--prefix"
                ) || a.starts_with("--workspace")
                    || a.starts_with("--filter")
                    || a == "-ws"
                    || a == "--workspaces"
                    || a == "-r"
                    || a == "--recursive"
                {
                    elsewhere = true;
                }
                j += 1;
            }
            match rest.get(j) {
                Some(n) => (n.clone(), &rest[j + 1..]),
                // `npm run` alone lists the scripts.
                None => return Some(Vec::new()),
            }
        }
        ("npm", "test" | "t" | "tst" | "start" | "stop" | "restart") => {
            let n = match sub {
                "t" | "tst" => "test",
                s => s,
            };
            (n.to_owned(), rest)
        }
        ("pnpm" | "yarn", "test" | "t" | "start" | "stop" | "restart") => {
            (if sub == "t" { "test" } else { sub }.to_owned(), rest)
        }
        ("bun", _) if builtin("bun", sub) => return None,
        ("pnpm" | "yarn" | "bun", _) if !builtin(tool, sub) => (sub.to_owned(), rest),
        _ => return None,
    };
    if elsewhere {
        return Some(unreadable(
            tool,
            "runs scripts of another package or workspace, which kGraf does not read",
        ));
    }
    // `bun run FILE` and `bun FILE` run a file, read as code.
    if tool == "bun"
        && (name.contains('/') || name.contains('.'))
        && super::worktree_file_exists(ctx, &name)
    {
        return Some(super::code::script_file_call("bun", &name, ctx));
    }
    if ["npm_config_script_shell", "NPM_CONFIG_SCRIPT_SHELL"]
        .iter()
        .any(|v| ctx.var(v).is_some())
    {
        return Some(unreadable(
            tool,
            "runs its scripts with the shell its environment names, which kGraf does not read",
        ));
    }
    if tool == "npm"
        && let Ok(npmrc) = read_worktree_file(ctx, ".npmrc")
        && npmrc
            .text
            .lines()
            .any(|l| l.trim_start().starts_with("script-shell"))
    {
        return Some(unreadable(
            tool,
            "runs its scripts with the `script-shell` its `.npmrc` sets, which kGraf does not \
             read",
        ));
    }
    let read = match read_worktree_file(ctx, "package.json") {
        Ok(r) => r,
        Err(why) => {
            return Some(unreadable(
                tool,
                &format!("runs a `package.json` script, and {why}"),
            ));
        }
    };
    let scripts = PACKAGES.get("package", &read.text, || parse_package(&read.text));
    let scripts = match scripts.as_ref() {
        Ok(s) => s,
        Err(why) => return Some(unreadable(tool, why)),
    };
    let extra_text: String = extra
        .iter()
        .filter(|a| *a != "--")
        .map(|a| shlex::try_quote(a).map_or_else(|_| a.clone(), |q| q.into_owned()))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = Vec::new();
    let mut push = |n: &str, with_args: bool| {
        if let Some(s) = scripts.get(n) {
            let mut text = s.clone();
            if with_args && !extra_text.is_empty() {
                text.push(' ');
                text.push_str(&extra_text);
            }
            out.push(Embedded::Script(text));
            true
        } else {
            false
        }
    };
    let names: Vec<(String, bool)> =
        if tool == "npm" && name == "restart" && !scripts.contains_key("restart") {
            // npm runs stop then start when there is no restart script.
            vec![
                ("prestop".into(), false),
                ("stop".into(), true),
                ("poststop".into(), false),
                ("prestart".into(), false),
                ("start".into(), true),
                ("poststart".into(), false),
            ]
        } else {
            vec![
                (format!("pre{name}"), false),
                (name.clone(), true),
                (format!("post{name}"), false),
            ]
        };
    let mut main_found = false;
    for (n, main) in &names {
        let hit = push(n, *main);
        if *main && hit {
            main_found = true;
        }
    }
    if !main_found {
        match (tool, name.as_str()) {
            // npm's default start script.
            ("npm", "start") => {
                out.push(Embedded::Argv(vec!["node".into(), "server.js".into()]));
            }
            // pnpm, yarn and bun run a package's program of that name when
            // no script has it.
            ("pnpm" | "yarn" | "bun", _)
                if !matches!(sub, "run" | "run-script") || tool != "pnpm" =>
            {
                let mut argv = vec![name.clone()];
                argv.extend(extra.iter().filter(|a| *a != "--").cloned());
                out.push(Embedded::Argv(argv));
            }
            _ => {}
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- just

/// One justfile recipe.
#[derive(Clone, Debug, Default)]
struct JustRecipe {
    /// How many parameters it needs at least, and whether its last one takes
    /// every remaining argument.
    required: usize,
    params: usize,
    variadic: bool,
    dependencies: Vec<String>,
    body: Vec<String>,
    /// Why its body cannot be read (a shebang, `[script]`,
    /// `[working-directory]`).
    opaque: Option<String>,
}

#[derive(Debug, Default)]
struct Justfile {
    recipes: BTreeMap<String, JustRecipe>,
    order: Vec<String>,
    default: Option<String>,
    vars: BTreeMap<String, String>,
    aliases: BTreeMap<String, String>,
    /// Backtick commands that run while just reads the file.
    at_parse: Vec<String>,
    opaque: Option<String>,
}

static JUSTFILES: ContentCache<Justfile> = ContentCache::new();

/// Every backtick command in `text`.
fn backticks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        // ```` ``` ```` opens an indented backtick block.
        let (fence, after) = match after.strip_prefix("``") {
            Some(rest) => ("```", rest),
            None => ("`", after),
        };
        match after.find(fence) {
            Some(end) => {
                out.push(after[..end].trim().to_owned());
                rest = &after[end + fence.len()..];
            }
            None => break,
        }
    }
    out
}

fn parse_justfile(text: &str) -> Justfile {
    let mut jf = Justfile::default();
    let mut current: Option<String> = None;
    let mut attributes: Vec<String> = Vec::new();
    for line in text.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if indented && current.is_some() {
            if trimmed.is_empty() {
                continue;
            }
            let name = current.clone().unwrap_or_default();
            let recipe = jf.recipes.entry(name).or_default();
            if recipe.body.is_empty() && trimmed.starts_with("#!") {
                recipe.opaque.get_or_insert_with(|| {
                    "is a shebang recipe, run by another interpreter".into()
                });
            }
            if !trimmed.starts_with('#') {
                recipe.body.push(trimmed.to_owned());
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            if !trimmed.is_empty() {
                continue;
            }
            current = None;
            continue;
        }
        current = None;
        if let Some(attr) = trimmed.strip_prefix('[') {
            attributes.push(attr.trim_end_matches(']').to_ascii_lowercase());
            continue;
        }
        let first = trimmed.split_whitespace().next().unwrap_or_default();
        match first {
            "import" | "import?" | "mod" | "mod?" => {
                jf.opaque.get_or_insert_with(|| {
                    "imports another justfile, which kGraf does not read".into()
                });
                continue;
            }
            "set" => {
                let rest = trimmed["set".len()..].trim();
                let (name, value) = rest.split_once(":=").unwrap_or((rest, "true"));
                let name = name.trim();
                if matches!(name, "shell" | "windows-shell" | "script-interpreter")
                    && !posix_shell(
                        value
                            .trim()
                            .trim_start_matches('[')
                            .split(',')
                            .next()
                            .unwrap_or_default()
                            .trim()
                            .trim_matches(['\'', '"']),
                    )
                {
                    jf.opaque.get_or_insert_with(|| {
                        format!("sets `{name}` to a shell kGraf does not read")
                    });
                }
                if name == "windows-powershell" && value.trim() == "true" {
                    jf.opaque.get_or_insert_with(|| {
                        "runs its recipes with PowerShell on Windows".into()
                    });
                }
                if name == "working-directory" {
                    jf.opaque
                        .get_or_insert_with(|| "sets another working directory".into());
                }
                jf.at_parse.extend(backticks(value));
                continue;
            }
            "alias" => {
                if let Some((a, b)) = trimmed["alias".len()..].split_once(":=") {
                    jf.aliases.insert(a.trim().to_owned(), b.trim().to_owned());
                }
                continue;
            }
            "export" | "unexport" => {
                let rest = trimmed[first.len()..].trim();
                if let Some((n, v)) = rest.split_once(":=") {
                    jf.at_parse.extend(backticks(v));
                    jf.vars.insert(n.trim().to_owned(), v.trim().to_owned());
                }
                continue;
            }
            _ => {}
        }
        if let Some((n, v)) = trimmed.split_once(":=")
            && !n.contains(':')
            && n.trim()
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            jf.at_parse.extend(backticks(v));
            jf.vars.insert(n.trim().to_owned(), v.trim().to_owned());
            continue;
        }
        // A recipe header: `[@]NAME PARAMS: DEPENDENCIES [&& DEPENDENCIES]`.
        let Some((head, deps)) = trimmed.split_once(':') else {
            jf.opaque
                .get_or_insert_with(|| format!("has a line kGraf cannot read: `{trimmed}`"));
            continue;
        };
        let mut head_words = head.split_whitespace();
        let name = head_words
            .next()
            .unwrap_or_default()
            .trim_start_matches('@')
            .to_owned();
        let mut recipe = JustRecipe::default();
        for p in head_words {
            let p = p.trim_start_matches('$');
            if p.starts_with('+') || p.starts_with('*') {
                recipe.variadic = true;
                if p.starts_with('+') && !p.contains('=') {
                    recipe.required += 1;
                }
                recipe.params += 1;
            } else {
                recipe.params += 1;
                if !p.contains('=') {
                    recipe.required += 1;
                }
            }
        }
        // Dependencies: names, or `(name args)`.
        let deps = deps.replace("&&", " ");
        let mut in_paren = false;
        for w in deps.split_whitespace() {
            if let Some(n) = w.strip_prefix('(') {
                in_paren = !w.ends_with(')');
                recipe.dependencies.push(n.trim_end_matches(')').to_owned());
                continue;
            }
            if in_paren {
                if w.ends_with(')') {
                    in_paren = false;
                }
                continue;
            }
            recipe.dependencies.push(w.to_owned());
        }
        for attr in attributes.drain(..) {
            if attr.starts_with("script") {
                recipe.opaque.get_or_insert_with(|| {
                    "is a `[script]` recipe, run by another interpreter".into()
                });
            }
            if attr.starts_with("working-directory") || attr == "no-cd" {
                recipe
                    .opaque
                    .get_or_insert_with(|| "runs in another directory".into());
            }
            if attr == "default" {
                jf.default = Some(name.clone());
            }
        }
        if !jf.recipes.contains_key(&name) {
            jf.order.push(name.clone());
        }
        jf.recipes.insert(name.clone(), recipe);
        current = Some(name);
    }
    jf
}

/// A recipe line with `{{ }}` interpolation resolved: a variable kGraf can
/// read as a literal is substituted, anything else is [`UNRESOLVED`].
fn interpolate(line: &str, jf: &Justfile) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("{{") {
        if rest[start..].starts_with("{{{{") {
            out.push_str(&rest[..start + 2]);
            rest = &rest[start + 4..];
            continue;
        }
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(UNRESOLVED);
            return out;
        };
        let expr = after[..end].trim();
        let literal = jf.vars.get(expr).and_then(|v| {
            let v = v.trim();
            (v.len() >= 2
                && (v.starts_with('\'') && v.ends_with('\'')
                    || v.starts_with('"') && v.ends_with('"')))
            .then(|| v[1..v.len() - 1].to_owned())
        });
        out.push_str(literal.as_deref().unwrap_or(UNRESOLVED));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// `just [OPTIONS] [RECIPE [ARGS]]...`.
pub(super) fn just(args: &[String], ctx: &ReadContext) -> Vec<Embedded> {
    let mut file: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let next = args.get(i + 1).cloned();
        match a.as_str() {
            "-f" | "--justfile" => {
                file = next;
                i += 2;
            }
            "-d" | "--working-directory" => {
                dir = next;
                i += 2;
            }
            "--shell" | "--shell-arg" | "--command" | "-c" | "--choose" | "--chooser" => {
                return unreadable(
                    "just",
                    &format!("is given `{a}`, which kGraf does not read"),
                );
            }
            "--set" => i += 3,
            "-l" | "--list" | "--summary" | "--dump" | "--fmt" | "--help" | "-h" | "--version"
            | "-V" | "--variables" | "--init" | "--edit" | "-e" | "--groups" => return Vec::new(),
            _ if a.starts_with("--justfile=") => {
                file = Some(a["--justfile=".len()..].to_owned());
                i += 1;
            }
            _ if a.starts_with('-') => i += 1,
            _ => {
                words.push(a.clone());
                i += 1;
            }
        }
    }
    let ctx = match &dir {
        Some(d) => ctx.moved_to(d),
        None => ctx.clone(),
    };
    let name = match file {
        Some(f) => f,
        None => {
            let found = ["justfile", "Justfile", ".justfile", "JUSTFILE", ".Justfile"]
                .iter()
                .find(|n| super::worktree_file_exists(&ctx, n));
            match found {
                Some(n) => (*n).to_owned(),
                None => return unreadable("just", "finds no justfile kGraf can read"),
            }
        }
    };
    let read = match read_worktree_file(&ctx, &name) {
        Ok(r) => r,
        Err(why) => return unreadable("just", &format!("cannot read its justfile: {why}")),
    };
    let jf = JUSTFILES.get("just", &read.text, || parse_justfile(&read.text));
    if let Some(why) = &jf.opaque {
        return unreadable("just", &format!("runs a justfile that {why}"));
    }
    // Which recipes run: each word that names a recipe takes its
    // parameters from the words after it.
    let mut goals: Vec<String> = Vec::new();
    let mut w = 0;
    let resolve = |n: &str| -> Option<String> {
        if jf.recipes.contains_key(n) {
            Some(n.to_owned())
        } else {
            jf.aliases.get(n).cloned()
        }
    };
    while w < words.len() {
        // `VAR=VALUE` overrides a variable.
        if words[w].contains('=') && !jf.recipes.contains_key(&words[w]) {
            w += 1;
            continue;
        }
        let Some(r) = resolve(&words[w]) else {
            return unreadable(
                "just",
                &format!(
                    "is asked for `{}`, which its justfile does not define",
                    words[w].escape_debug()
                ),
            );
        };
        let recipe = &jf.recipes[&r];
        w += 1;
        if recipe.variadic {
            w = words.len();
        } else {
            w += recipe.params.min(words.len() - w);
        }
        goals.push(r);
    }
    if goals.is_empty() {
        match jf.default.clone().or_else(|| jf.order.first().cloned()) {
            Some(g) => goals.push(g),
            None => return Vec::new(),
        }
    }
    let mut lines: Vec<String> = jf.at_parse.clone();
    let mut seen = BTreeSet::new();
    let mut queue = goals;
    while let Some(g) = queue.pop() {
        if !seen.insert(g.clone()) || seen.len() > 512 {
            continue;
        }
        let Some(recipe) = resolve(&g).and_then(|r| jf.recipes.get(&r)) else {
            return unreadable(
                "just",
                &format!("depends on `{g}`, which its justfile does not define"),
            );
        };
        if let Some(why) = &recipe.opaque {
            return unreadable("just", &format!("runs the recipe `{g}`, which {why}"));
        }
        for line in &recipe.body {
            lines.extend(backticks(line));
            let line = interpolate(line, &jf);
            lines.push(line.trim_start_matches(['@', '-']).trim().to_owned());
        }
        queue.extend(recipe.dependencies.iter().cloned());
    }
    wrap(
        &ctx,
        dir.is_some(),
        lines
            .into_iter()
            .filter(|l| !l.is_empty())
            .map(Embedded::Script)
            .collect(),
    )
}

// --------------------------------------------------------------- cargo

/// cargo's own commands, which no alias may redefine, and the commands it
/// ships as programs (`clippy`, `fmt`, `miri`).
const CARGO_COMMANDS: &[&str] = &[
    "add",
    "bench",
    "build",
    "check",
    "clean",
    "clippy",
    "config",
    "doc",
    "fetch",
    "fix",
    "fmt",
    "generate-lockfile",
    "git-checkout",
    "help",
    "info",
    "init",
    "install",
    "locate-project",
    "login",
    "logout",
    "metadata",
    "miri",
    "new",
    "owner",
    "package",
    "pkgid",
    "publish",
    "read-manifest",
    "remove",
    "report",
    "run",
    "rustc",
    "rustdoc",
    "search",
    "test",
    "tree",
    "uninstall",
    "update",
    "vendor",
    "verify-project",
    "version",
    "yank",
];

/// cargo's built-in aliases, which a configured alias may override.
const CARGO_BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("b", "build"),
    ("c", "check"),
    ("d", "doc"),
    ("r", "run"),
    ("t", "test"),
    ("rm", "remove"),
];

/// What a `.cargo/config.toml` says about running commands.
#[derive(Debug, Default)]
struct CargoConfig {
    aliases: BTreeMap<String, Vec<String>>,
    /// `[target.*] runner` values.
    runners: Vec<Vec<String>>,
    /// `build.rustc-wrapper` and the like, and `[target.*] linker`.
    wrappers: Vec<Vec<String>>,
    opaque: Option<String>,
}

static CARGO_CONFIGS: ContentCache<CargoConfig> = ContentCache::new();

/// A configuration value that names a program: a string split on
/// whitespace, or an array of strings.
fn program_value(v: &toml::Value) -> Option<Vec<String>> {
    match v {
        toml::Value::String(s) => Some(s.split_whitespace().map(str::to_owned).collect()),
        toml::Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>(),
        toml::Value::Table(t) => t.get("path").and_then(program_value).map(|mut p| {
            if let Some(args) = t.get("args").and_then(program_value) {
                p.extend(args);
            }
            p
        }),
        _ => None,
    }
}

fn parse_cargo_config(text: &str) -> CargoConfig {
    let mut cfg = CargoConfig::default();
    let value: toml::Table = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            cfg.opaque = Some(format!(
                "has a `.cargo/config.toml` that does not parse: {e}"
            ));
            return cfg;
        }
    };
    if value.contains_key("include") {
        cfg.opaque = Some("has a `.cargo/config.toml` that includes another file".into());
    }
    if let Some(aliases) = value.get("alias").and_then(|a| a.as_table()) {
        for (k, v) in aliases {
            match program_value(v) {
                Some(words) => {
                    cfg.aliases.insert(k.clone(), words);
                }
                None => cfg.opaque = Some(format!("has an alias `{k}` kGraf cannot read")),
            }
        }
    }
    if let Some(build) = value.get("build").and_then(|b| b.as_table()) {
        for key in [
            "rustc-wrapper",
            "rustc-workspace-wrapper",
            "rustc",
            "rustdoc",
        ] {
            if let Some(p) = build.get(key).and_then(program_value) {
                cfg.wrappers.push(p);
            }
        }
    }
    if let Some(targets) = value.get("target").and_then(|t| t.as_table()) {
        for (_, t) in targets {
            let Some(t) = t.as_table() else { continue };
            if let Some(p) = t.get("runner").and_then(program_value) {
                cfg.runners.push(p);
            }
            if let Some(p) = t.get("linker").and_then(program_value) {
                cfg.wrappers.push(p);
            }
        }
    }
    cfg
}

/// The configuration files cargo reads for a command run in `ctx`'s
/// directory, from the worktree root down to it (deeper wins), within the
/// worktree.
fn cargo_configs(ctx: &ReadContext) -> Result<Vec<std::sync::Arc<CargoConfig>>, String> {
    let (Some(root), Some(cwd)) = (&ctx.root, &ctx.cwd) else {
        return Err("kGraf cannot tell which directory cargo runs in".into());
    };
    let mut dirs: Vec<std::path::PathBuf> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .map(|d| d.to_path_buf())
        .collect();
    dirs.reverse();
    let mut out = Vec::new();
    for d in dirs {
        let rel = d.strip_prefix(cwd).unwrap_or(&d);
        let _ = rel;
        for name in [".cargo/config.toml", ".cargo/config"] {
            let path = d.join(name);
            if !path.exists() {
                continue;
            }
            let file_ctx = ReadContext {
                cwd: Some(d.clone()),
                ..ctx.clone()
            };
            let read = read_worktree_file(&file_ctx, name)?;
            let cfg = CARGO_CONFIGS.get("cargo", &read.text, || parse_cargo_config(&read.text));
            if let Some(why) = &cfg.opaque {
                return Err(why.clone());
            }
            out.push(cfg);
            break;
        }
    }
    Ok(out)
}

/// `cargo [+TOOLCHAIN] [OPTIONS] COMMAND [ARGS]`: an alias's expansion, and
/// the runner and wrappers the configuration names for the commands that
/// compile or run. `None` when cargo runs nothing of the configuration's.
pub(super) fn cargo(words: &[String], at: usize, ctx: &ReadContext) -> Option<Vec<Embedded>> {
    let args = &words[at + 1..];
    let mut out: Vec<Embedded> = Vec::new();
    let mut given_aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in &ctx.env {
        let (name, value) = (name.as_str(), value.as_str());
        {
            if let Some(alias) = name.strip_prefix("CARGO_ALIAS_") {
                given_aliases.insert(
                    alias.to_ascii_lowercase().replace('_', "-"),
                    value.split_whitespace().map(str::to_owned).collect(),
                );
            }
            if matches!(
                name,
                "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER" | "RUSTC" | "RUSTDOC"
            ) || (name.starts_with("CARGO_TARGET_")
                && (name.ends_with("_RUNNER") || name.ends_with("_LINKER")))
                || matches!(
                    name,
                    "CARGO_BUILD_RUSTC_WRAPPER" | "CARGO_BUILD_RUSTC" | "CARGO_BUILD_RUSTDOC"
                )
            {
                out.push(Embedded::Argv(
                    value.split_whitespace().map(str::to_owned).collect(),
                ));
            }
            if matches!(name, "CARGO_HOME") {
                out.push(Embedded::Unreadable(
                    "`cargo` is given another `CARGO_HOME`, whose configuration kGraf does not read"
                        .into(),
                ));
            }
        }
    }
    let mut i = 0;
    let mut moved = false;
    while let Some(a) = args.get(i) {
        if a.starts_with('+') {
            i += 1;
            continue;
        }
        if a == "--config" || a.starts_with("--config=") {
            let v = if a == "--config" {
                i += 1;
                args.get(i).cloned().unwrap_or_default()
            } else {
                a["--config=".len()..].to_owned()
            };
            match v.split_once('=') {
                Some((k, val)) if k.trim().starts_with("alias.") => {
                    let name = k.trim()["alias.".len()..].to_owned();
                    let parsed: Option<toml::Value> =
                        toml::from_str::<toml::Table>(&format!("x = {val}"))
                            .ok()
                            .and_then(|t| t.get("x").cloned());
                    match parsed.as_ref().and_then(program_value) {
                        Some(w) => {
                            given_aliases.insert(name, w);
                        }
                        None => {
                            return Some(unreadable(
                                "cargo",
                                "is given an alias kGraf cannot read",
                            ));
                        }
                    }
                }
                Some((k, val))
                    if k.contains("runner")
                        || k.contains("wrapper")
                        || k.contains("linker")
                        || k.trim() == "build.rustc"
                        || k.trim() == "build.rustdoc" =>
                {
                    let val = val.trim().trim_matches(['\'', '"']);
                    out.push(Embedded::Argv(
                        val.split_whitespace().map(str::to_owned).collect(),
                    ));
                }
                Some(_) => {}
                None => {
                    return Some(unreadable(
                        "cargo",
                        "is given a configuration file, which kGraf does not read",
                    ));
                }
            }
            i += 1;
            continue;
        }
        if a == "-C" || a == "-Z" || a == "--color" || a == "--manifest-path" {
            if a == "-C" {
                moved = true;
            }
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    let sub = args.get(i)?.as_str();
    let rest = &args[i + 1..];
    let ctx = if moved { ctx.lost() } else { ctx.clone() };
    let builtin_alias = CARGO_BUILTIN_ALIASES
        .iter()
        .find(|(a, _)| *a == sub)
        .map(|(_, c)| *c);
    let configs = if CARGO_COMMANDS.contains(&sub) && given_aliases.is_empty() && !needs_config(sub)
    {
        Ok(Vec::new())
    } else {
        cargo_configs(&ctx)
    };
    // An alias: `-c` and environment first, then the configuration, deeper
    // directories winning.
    if !CARGO_COMMANDS.contains(&sub) {
        let configured = match &configs {
            Ok(c) => c.iter().rev().find_map(|c| c.aliases.get(sub).cloned()),
            Err(_) => None,
        };
        let expansion = given_aliases.get(sub).cloned().or(configured);
        match (expansion, &configs) {
            (Some(exp), _) => {
                let mut argv = vec!["cargo".to_owned()];
                argv.extend(exp);
                argv.extend(rest.iter().cloned());
                out.push(Embedded::Argv(argv));
                return Some(out);
            }
            (None, Err(why)) if builtin_alias.is_none() => {
                out.push(Embedded::Unreadable(format!(
                    "`cargo {sub}` may be an alias, and {why}"
                )));
                return Some(out);
            }
            // An external `cargo-NAME` program, or a built-in alias.
            _ => {}
        }
    }
    let effective = builtin_alias.unwrap_or(sub);
    if needs_config(effective) {
        match &configs {
            Ok(configs) => {
                for c in configs {
                    for w in &c.wrappers {
                        out.push(Embedded::Argv(w.clone()));
                    }
                    if matches!(effective, "run" | "test" | "bench") {
                        for r in &c.runners {
                            out.push(Embedded::Argv(r.clone()));
                        }
                    }
                }
            }
            Err(why) => out.push(Embedded::Unreadable(format!("`cargo {sub}`: {why}"))),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Whether a cargo command compiles or runs, so the configuration's
/// wrappers, linkers and runners take part.
fn needs_config(sub: &str) -> bool {
    matches!(
        sub,
        "build"
            | "check"
            | "test"
            | "run"
            | "bench"
            | "clippy"
            | "doc"
            | "rustc"
            | "rustdoc"
            | "fix"
            | "install"
            | "miri"
            | "b"
            | "c"
            | "t"
            | "r"
            | "d"
    )
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn make_reads_the_recipes_of_the_target_and_its_prerequisites() {
        let (_d, ctx) = tree(&[(
            "Makefile",
            "CC := gcc\nOUT = build\n\nall: app\n\napp: main.o\n\t@$(CC) -o $@ $^\n\nmain.o: main.c\n\t$(CC) -c $<\n\nclean:\n\t-rm -rf $(OUT)\n\ndeploy:\n\tcurl -X POST x\n",
        )]);
        let all = scripts(&make(&[], &argv(&[]), &ctx));
        assert!(all.contains(&"gcc -o app main.o".to_owned()), "{all:?}");
        assert!(all.contains(&"gcc -c main.c".to_owned()), "{all:?}");
        assert!(!all.iter().any(|s| s.contains("curl")), "{all:?}");
        assert_eq!(
            scripts(&make(&[], &argv(&["clean"]), &ctx)),
            vec!["rm -rf build"]
        );
        assert_eq!(
            scripts(&make(&[], &argv(&["deploy"]), &ctx)),
            vec!["curl -X POST x"]
        );
        // A variable given on the command line wins.
        assert_eq!(
            scripts(&make(&[], &argv(&["clean", "OUT=dist"]), &ctx)),
            vec!["rm -rf dist"]
        );
    }

    #[test]
    fn make_reads_shell_calls_sub_makes_and_refuses_what_it_cannot_place() {
        let (_d, ctx) = tree(&[
            (
                "Makefile",
                "V := $(shell curl -s x)\nall:\n\t$(MAKE) -C sub\n",
            ),
            ("sub/Makefile", "all:\n\twget y\n"),
            ("inc/Makefile", "include other.mk\nall:\n\tls\n"),
            ("var/Makefile", "X = a\nX = b\nall:\n\t$(X) run\n"),
            ("env/Makefile", "all:\n\t$(TOOL) run\n"),
            ("shell/Makefile", "SHELL = python3\nall:\n\tprint(1)\n"),
            (
                "semi/Makefile",
                "ifdef A\nX = a\nelse\nX = b; curl x\nendif\nall:\n\tls $(X)\n",
            ),
            ("call/Makefile", "f = $(1)\nall:\n\tls $(call f,; curl x)\n"),
            (
                "canned/Makefile",
                "define run\ncurl a\nwget b\nendef\nall:\n\t$(run)\n",
            ),
        ]);
        assert_eq!(
            scripts(&make(&[], &argv(&["-C", "canned"]), &ctx)),
            vec!["curl a", "wget b"]
        );
        let all = scripts(&make(&[], &argv(&[]), &ctx));
        assert!(all.contains(&"curl -s x".to_owned()), "{all:?}");
        assert!(all.contains(&"make -C sub".to_owned()), "{all:?}");
        let sub = scripts(&make(&[], &argv(&["-C", "sub"]), &ctx));
        assert_eq!(sub, vec!["wget y"]);
        assert!(opaque(&make(&[], &argv(&["-C", "inc"]), &ctx)));
        assert!(opaque(&make(&[], &argv(&["-C", "shell"]), &ctx)));
        // A program defined twice is left for the reader to refuse; one
        // from the environment is empty unless given before `make`.
        assert!(scripts(&make(&[], &argv(&["-C", "var"]), &ctx))[0].starts_with(UNRESOLVED));
        assert_eq!(
            scripts(&make(&[], &argv(&["-C", "env"]), &ctx)),
            vec!["run"]
        );
        assert_eq!(
            scripts(&make(
                &[],
                &argv(&["-C", "env"]),
                &ctx.with_assignments(&argv(&["TOOL=curl"]))
            )),
            vec!["curl run"]
        );
        // A variable the call exports before `make` is make's too.
        let exported = ReadContext {
            env: vec![("TOOL".into(), "wget".into())],
            ..ctx.clone()
        };
        assert_eq!(
            scripts(&make(&[], &argv(&["-C", "env"]), &exported)),
            vec!["wget run"]
        );
        // Text kGraf cannot know that may hold shell syntax is unreadable.
        assert!(opaque(&make(&[], &argv(&["-C", "semi"]), &ctx)));
        assert!(opaque(&make(&[], &argv(&["-C", "call"]), &ctx)));
        // No makefile, or a directory outside the worktree.
        assert!(opaque(&make(&[], &argv(&["-C", "nowhere"]), &ctx)));
        assert!(opaque(&make(&[], &argv(&["-C", "../.."]), &ctx)));
        assert!(opaque(&make(&[], &argv(&[]), &ReadContext::default())));
    }

    #[test]
    fn package_scripts_are_read_with_their_hooks() {
        let (_d, ctx) = tree(&[(
            "package.json",
            r#"{"scripts":{"pretest":"node setup.js","test":"jest","posttest":"curl x","build":"tsc && vite build","start":"node app.js"}}"#,
        )]);
        let s = |words: &[&str]| {
            scripts(&package_scripts(words[0], &argv(&words[1..]), &ctx).unwrap_or_default())
        };
        assert_eq!(s(&["npm", "test"]), vec!["node setup.js", "jest", "curl x"]);
        assert_eq!(s(&["npm", "run", "build"]), vec!["tsc && vite build"]);
        assert_eq!(
            s(&["npm", "run", "build", "--", "--watch"]),
            vec!["tsc && vite build --watch"]
        );
        assert_eq!(s(&["pnpm", "build"]), vec!["tsc && vite build"]);
        assert_eq!(s(&["yarn", "build"]), vec!["tsc && vite build"]);
        assert_eq!(s(&["bun", "run", "build"]), vec!["tsc && vite build"]);
        assert_eq!(s(&["yarn", "eslint", "."]), vec!["eslint ."]);
        assert!(package_scripts("npm", &argv(&["install"]), &ctx).is_none());
        assert!(package_scripts("bun", &argv(&["test"]), &ctx).is_none());
        let missing =
            package_scripts("npm", &argv(&["test"]), &ReadContext::default()).unwrap_or_default();
        assert!(opaque(&missing));
        let ws =
            package_scripts("npm", &argv(&["run", "build", "-w", "a"]), &ctx).unwrap_or_default();
        assert!(opaque(&ws));
    }

    #[test]
    fn just_reads_recipes_dependencies_and_backticks() {
        let (_d, ctx) = tree(&[(
            "justfile",
            "version := `git describe`\nout := 'dist'\n\ndefault: build\n\nbuild: lint\n    cargo build --release\n    @cp target/x {{out}}\n\nlint:\n    cargo clippy\n\ndeploy env:\n    curl -X POST {{env}}\n\nshebang:\n    #!/usr/bin/env python3\n    print(1)\n",
        )]);
        let all = scripts(&just(&argv(&[]), &ctx));
        for want in [
            "git describe",
            "cargo build --release",
            "cp target/x dist",
            "cargo clippy",
        ] {
            assert!(all.contains(&want.to_owned()), "{want}: {all:?}");
        }
        let deploy = scripts(&just(&argv(&["deploy", "prod"]), &ctx));
        assert!(
            deploy.contains(&format!("curl -X POST {UNRESOLVED}")),
            "{deploy:?}"
        );
        assert!(opaque(&just(&argv(&["shebang"]), &ctx)));
        assert!(opaque(&just(&argv(&["nope"]), &ctx)));
    }

    #[test]
    fn cargo_aliases_runners_and_wrappers_are_read() {
        let (_d, ctx) = tree(&[(
            ".cargo/config.toml",
            "[alias]\nxt = \"run --package xtask --\"\npub2 = [\"publish\", \"--dry-run\"]\n\n[target.x86_64-unknown-linux-gnu]\nrunner = \"curl x\"\n\n[build]\nrustc-wrapper = \"sccache\"\n",
        )]);
        let c = |words: &[&str]| scripts(&cargo(&argv(words), 0, &ctx).unwrap_or_default());
        assert_eq!(
            c(&["cargo", "xt", "dist"]),
            vec!["cargo run --package xtask -- dist"]
        );
        assert_eq!(c(&["cargo", "pub2"]), vec!["cargo publish --dry-run"]);
        assert_eq!(c(&["cargo", "test"]), vec!["sccache", "curl x"]);
        assert_eq!(c(&["cargo", "build"]), vec!["sccache"]);
        assert!(c(&["cargo", "fmt"]).is_empty());
        assert_eq!(
            c(&["cargo", "--config", "alias.zz=\"publish\"", "zz"]),
            vec!["cargo publish"]
        );
        // An external program is no alias.
        assert!(c(&["cargo", "nextest", "run"]).is_empty());
        // Without a directory, a name that may be an alias is unreadable;
        // a built-in command runs as before.
        assert!(opaque(
            &cargo(&argv(&["cargo", "xt"]), 0, &ReadContext::default()).unwrap_or_default()
        ));
        assert!(cargo(&argv(&["cargo", "fmt"]), 0, &ReadContext::default()).is_none());
    }
}
