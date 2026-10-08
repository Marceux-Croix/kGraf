//! The programs interpreter code starts (decision 0094, amended
//! 2026-10-06): inline code (`python3 -c`, `node -e`, `deno eval`, `bun -e`,
//! `perl -e`, `ruby -e`, `php -r`, an `awk` program, `osascript -e`) and
//! script files (`python3 x.py`, `node x.js`, `perl x.pl`, `ruby x.rb`, `php
//! x.php`, `awk -f x.awk`).
//!
//! Arbitrary code cannot be read in general. kGraf finds the calls that
//! start a program and reads their literal arguments as commands: a string
//! is a script (`os.system("curl x")`), a list of strings is an argv
//! (`subprocess.run(["curl", "x"])`). A spawning call whose arguments are
//! not literal, a spawning function used other than as a call, and the
//! features that run text as code or load native code make the command
//! unreadable, as does code that does not parse and a file that is missing.
//!
//! Python, JavaScript and TypeScript, Ruby and PHP are parsed with the
//! tree-sitter grammars kGraf already builds. Perl, awk and AppleScript have
//! no grammar here; their text is scanned for the spawning words, and a
//! spawning word kGraf cannot read as a call with literal arguments makes
//! the command unreadable (a word in a comment or a string included: the
//! scan errs toward unreadable).
//!
//! Local modules: the modules a script imports from the worktree are
//! scanned too, one level deep; a local module that imports another local
//! module makes the command unreadable. Installed packages are not read,
//! apart from a list of packages known to start programs or run code from
//! text, whose import makes the command unreadable.
//!
//! Interpreters with no reader here (Lua, R, Julia, Tcl, `expect`, csh,
//! nushell and the like) are unreadable when given code or a script.
//!
//! Every list here (interpreters, spawning calls, modules, the import
//! bound) is recorded in decision 0094. Parsed results are cached by the hash of the code ([`super::ContentCache`]), so
//! a repeated check of the same file parses it once.

use super::{ContentCache, Embedded, ReadContext, read_worktree_file};
use std::collections::HashMap;
use std::sync::Arc;
use tree_sitter::{Language, Node, Parser, Tree};

/// The languages read here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lang {
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Ruby,
    Php,
    /// PHP code with no `<?php` tag (`php -r`).
    PhpOnly,
    Perl,
    Awk,
    AppleScript,
}

impl Lang {
    fn tag(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::PhpOnly => "php-only",
            Self::Perl => "perl",
            Self::Awk => "awk",
            Self::AppleScript => "applescript",
        }
    }

    /// The language of a JavaScript-family file by its extension.
    fn of_js_file(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".tsx") {
            Self::Tsx
        } else if lower.ends_with(".ts") || lower.ends_with(".mts") || lower.ends_with(".cts") {
            Self::TypeScript
        } else {
            Self::JavaScript
        }
    }
}

/// An interpreter family, by its program name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Interp {
    Python,
    Node,
    /// Runners of TypeScript files (`tsx`, `ts-node`).
    TsRunner,
    Perl,
    Ruby,
    Php,
    Awk,
    Osascript,
    /// An interpreter with no reader: unreadable when given code or a file.
    Unread,
}

/// The interpreter a program key names, if any.
pub(super) fn interpreter(key: &str) -> Option<Interp> {
    let versioned = |stem: &str| {
        key.strip_prefix(stem).is_some_and(|rest| {
            rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
    };
    Some(if versioned("python") || versioned("pypy") || key == "py" {
        Interp::Python
    } else if key == "node" || key == "nodejs" {
        Interp::Node
    } else if matches!(
        key,
        "tsx" | "ts-node" | "esno" | "vite-node" | "ts-node-esm"
    ) {
        Interp::TsRunner
    } else if versioned("perl") {
        Interp::Perl
    } else if versioned("ruby") {
        Interp::Ruby
    } else if versioned("php") {
        Interp::Php
    } else if matches!(key, "awk" | "gawk" | "mawk" | "nawk" | "original-awk") {
        Interp::Awk
    } else if key == "osascript" {
        Interp::Osascript
    } else if matches!(
        key,
        "lua"
            | "luajit"
            | "rscript"
            | "r"
            | "julia"
            | "tclsh"
            | "wish"
            | "expect"
            | "guile"
            | "racket"
            | "elixir"
            | "erl"
            | "escript"
            | "scala"
            | "groovy"
            | "swift"
            | "csh"
            | "tcsh"
            | "nu"
            | "xonsh"
            | "elvish"
            | "jshell"
    ) || versioned("lua")
    {
        Interp::Unread
    } else {
        return None;
    })
}

fn unreadable(program: &str, why: &str) -> Vec<Embedded> {
    vec![Embedded::Unreadable(format!("`{program}` {why}"))]
}

/// What a scan of one piece of code found: the commands it starts, and
/// the local modules it imports (as written).
#[derive(Clone, Debug, Default)]
struct Scan {
    found: Vec<Embedded>,
    imports: Vec<String>,
}

type ScanResult = Result<Scan, String>;

static SCANS: ContentCache<ScanResult> = ContentCache::new();

/// The scan of `code` in `lang`, cached by its text.
fn scan(lang: Lang, code: &str) -> Arc<ScanResult> {
    SCANS.get(lang.tag(), code, || match lang {
        Lang::Python => python::scan(code),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => js::scan(lang, code),
        Lang::Ruby => ruby::scan(code),
        Lang::Php | Lang::PhpOnly => php::scan(lang, code),
        Lang::Perl => lexical::perl(code),
        Lang::Awk => lexical::awk(code),
        Lang::AppleScript => lexical::applescript(code),
    })
}

/// How code is located for its local imports: the directory a relative
/// import is taken from.
struct Origin {
    /// The context imports are read in (the script's directory, or the
    /// command's for inline code).
    ctx: ReadContext,
}

/// The commands `code` starts, its local imports scanned one level deep.
fn read_code(program: &str, lang: Lang, code: &str, origin: &Origin) -> Vec<Embedded> {
    let scanned = scan(lang, code);
    let scan = match scanned.as_ref() {
        Ok(s) => s,
        Err(why) => return unreadable(program, &format!("runs code that {why}")),
    };
    let mut out = scan.found.clone();
    for import in &scan.imports {
        match local_module(lang, import, &origin.ctx) {
            Ok(None) => {}
            Ok(Some(file)) => {
                let inner = self::scan(lang, &file.text);
                match inner.as_ref() {
                    Ok(s)
                        if s.imports.iter().any(|i| {
                            let dir = module_context(&origin.ctx, &file.path);
                            matches!(local_module(lang, i, &dir), Ok(Some(_)) | Err(_))
                        }) =>
                    {
                        return unreadable(
                            program,
                            &format!(
                                "runs code whose local module `{}` imports another local module, \
                                 past the one level kGraf reads",
                                import.escape_debug()
                            ),
                        );
                    }
                    Ok(s) => out.extend(s.found.iter().cloned()),
                    Err(why) => {
                        return unreadable(
                            program,
                            &format!(
                                "imports the local module `{}`, which {why}",
                                import.escape_debug()
                            ),
                        );
                    }
                }
            }
            Err(why) => return unreadable(program, &why),
        }
    }
    out
}

/// The context of a file's directory, for the imports it makes.
fn module_context(ctx: &ReadContext, file: &std::path::Path) -> ReadContext {
    ReadContext {
        cwd: file.parent().map(|p| p.to_path_buf()),
        ..ctx.clone()
    }
}

/// The file of a local module `import` (as the code wrote it), read from
/// the worktree: `Ok(None)` when it is not a local module (an installed
/// package), `Err` when it should be local and cannot be read.
fn local_module(
    lang: Lang,
    import: &str,
    ctx: &ReadContext,
) -> Result<Option<super::ReadFile>, String> {
    let candidates: Vec<String> = match lang {
        Lang::Python => {
            let dots = import.chars().take_while(|c| *c == '.').count();
            let rest = &import[dots..];
            let mut base = String::new();
            for _ in 1..dots {
                base.push_str("../");
            }
            let path = rest.replace('.', "/");
            if path.is_empty() {
                vec![format!("{base}__init__.py")]
            } else {
                vec![
                    format!("{base}{path}.py"),
                    format!("{base}{path}/__init__.py"),
                ]
            }
        }
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            let mut c = vec![import.to_owned()];
            for ext in [
                ".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".tsx", ".jsx", ".json",
            ] {
                c.push(format!("{import}{ext}"));
            }
            for index in ["/index.js", "/index.ts", "/index.mjs"] {
                c.push(format!("{import}{index}"));
            }
            c
        }
        Lang::Ruby => {
            if import.ends_with(".rb") {
                vec![import.to_owned()]
            } else {
                vec![format!("{import}.rb"), import.to_owned()]
            }
        }
        Lang::Php | Lang::PhpOnly => vec![import.to_owned()],
        _ => return Ok(None),
    };
    for c in &candidates {
        if super::worktree_file_exists(ctx, c) {
            if c.ends_with(".json") {
                return Ok(None);
            }
            return read_worktree_file(ctx, c).map(Some);
        }
    }
    // A relative import (`./x`, `.x`) names a local module that must be
    // there; a bare name is an installed package.
    let relative = names_a_file(import);
    if relative || matches!(lang, Lang::Php | Lang::PhpOnly) {
        return Err(format!(
            "imports the local module `{}`, which kGraf cannot find",
            import.escape_debug()
        ));
    }
    if ctx.cwd.is_none() {
        return Err(format!(
            "imports `{}`, and kGraf cannot tell whether it is a local module",
            import.escape_debug()
        ));
    }
    Ok(None)
}

/// The commands a script file run by an interpreter starts.
fn read_script_file(program: &str, lang: Lang, file: &str, ctx: &ReadContext) -> Vec<Embedded> {
    let read = match read_worktree_file(ctx, file) {
        Ok(r) => r,
        Err(why) => {
            return unreadable(program, &format!("runs a script kGraf cannot read: {why}"));
        }
    };
    // A file starts with a `#!` line the interpreter skips.
    let origin = Origin {
        ctx: module_context(ctx, &read.path),
    };
    read_code(program, lang, &read.text, &origin)
}

/// `bun run FILE` and `bun FILE`: the file read as code of its kind.
pub(super) fn script_file_call(program: &str, file: &str, ctx: &ReadContext) -> Vec<Embedded> {
    read_script_file(program, Lang::of_js_file(file), file, ctx)
}

/// Whether a module or library an interpreter is told to load names a file
/// (`./x`, `/x`, and on Windows `.\x`, `C:\x`, `\x`) rather than an installed
/// package, under the host's path rules.
fn names_a_file(spec: &str) -> bool {
    super::paths::names_a_file(super::paths::Flavor::HOST, spec)
}

fn inline(program: &str, lang: Lang, code: &str, ctx: &ReadContext) -> Vec<Embedded> {
    read_code(program, lang, code, &Origin { ctx: ctx.clone() })
}

/// The commands an interpreter call starts: its inline code or its script
/// file. `None` when the interpreter starts nothing kGraf reads (a version
/// question).
pub(super) fn interpreter_call(
    interp: Interp,
    program: &str,
    args: &[String],
    ctx: &ReadContext,
) -> Option<Vec<Embedded>> {
    Some(match interp {
        Interp::Python => python_call(program, args, ctx)?,
        Interp::Node => node_call(program, args, ctx)?,
        Interp::TsRunner => {
            let file = args.iter().find(|a| !a.starts_with('-'))?;
            read_script_file(program, Lang::of_js_file(file), file, ctx)
        }
        Interp::Perl => perl_call(program, args, ctx)?,
        Interp::Ruby => ruby_call(program, args, ctx)?,
        Interp::Php => php_call(program, args, ctx)?,
        Interp::Awk => awk_call(program, args, ctx)?,
        Interp::Osascript => osascript_call(program, args, ctx)?,
        Interp::Unread => {
            let asks_version = args.iter().all(|a| {
                matches!(
                    a.as_str(),
                    "--version" | "-v" | "-V" | "--help" | "-h" | "-version"
                )
            });
            if args.is_empty() || !asks_version {
                unreadable(program, "runs code kGraf does not read")
            } else {
                return None;
            }
        }
    })
}

/// Whether `args` only ask an interpreter for its version or help.
fn only_version(args: &[String]) -> bool {
    !args.is_empty()
        && args.iter().all(|a| {
            matches!(
                a.as_str(),
                "--version" | "-V" | "-VV" | "-v" | "--help" | "-h" | "-?" | "--help-all"
            )
        })
}

fn python_call(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    if only_version(args) {
        return None;
    }
    let mut interactive = false;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if a == "-" {
            return Some(unreadable(program, "reads its code from its input"));
        }
        if a.starts_with("--") {
            // `--check-hash-based-pycs MODE` takes a value.
            i += if a == "--check-hash-based-pycs" { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) {
            let mut consumed = 1;
            for (k, c) in cluster.char_indices() {
                match c {
                    'c' | 'm' => {
                        let attached = &cluster[k + 1..];
                        let value = if attached.is_empty() {
                            args.get(i + 1).cloned()
                        } else {
                            Some(attached.to_owned())
                        };
                        let Some(value) = value else {
                            return Some(unreadable(program, "names no code"));
                        };
                        if interactive {
                            return Some(unreadable(program, "reads more code from its input"));
                        }
                        return Some(if c == 'c' {
                            inline(program, Lang::Python, &value, ctx)
                        } else {
                            python_module(program, &value, ctx)
                        });
                    }
                    'W' | 'X' | 'Q' => {
                        if cluster[k + 1..].is_empty() {
                            consumed = 2;
                        }
                        break;
                    }
                    'i' => interactive = true,
                    _ => {}
                }
            }
            i += consumed;
            continue;
        }
        break;
    }
    if interactive {
        return Some(unreadable(program, "reads more code from its input"));
    }
    match args.get(i) {
        Some(file) => Some(read_script_file(program, Lang::Python, file, ctx)),
        None => Some(unreadable(program, "reads its code from its input")),
    }
}

/// `python3 -m MODULE`: a local module (`MODULE.py`, `MODULE/__main__.py`
/// in the command's directory) is read; an installed one (`pytest`, `pip`)
/// is a program like any other, as it was.
fn python_module(program: &str, module: &str, ctx: &ReadContext) -> Vec<Embedded> {
    let path = module.replace('.', "/");
    for candidate in [format!("{path}.py"), format!("{path}/__main__.py")] {
        if super::worktree_file_exists(ctx, &candidate) {
            return read_script_file(program, Lang::Python, &candidate, ctx);
        }
    }
    if ctx.cwd.is_none() {
        return unreadable(
            program,
            &format!(
                "runs the module `{}`, and kGraf cannot tell whether it is a local one",
                module.escape_debug()
            ),
        );
    }
    Vec::new()
}

fn node_call(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    if only_version(args) {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let next = args.get(i + 1);
        match a.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "-" => return Some(unreadable(program, "reads its code from its input")),
            "-e" | "--eval" | "-p" | "--print" => {
                let Some(code) = next else {
                    return Some(unreadable(program, "names no code"));
                };
                out.extend(inline(program, Lang::JavaScript, code, ctx));
                return Some(out);
            }
            "-c" | "--check" => return None,
            "-r" | "--require" | "--import" => {
                if let Some(m) = next
                    && names_a_file(m)
                {
                    out.extend(read_script_file(program, Lang::of_js_file(m), m, ctx));
                }
                i += 2;
                continue;
            }
            "--loader" | "--experimental-loader" => {
                return Some(unreadable(
                    program,
                    "loads a module loader kGraf does not read",
                ));
            }
            "--input-type"
            | "--env-file"
            | "--title"
            | "--conditions"
            | "-C"
            | "--stack-size"
            | "--inspect-port"
            | "--experimental-default-type"
            | "--watch-path"
            | "--test-reporter"
            | "--test-reporter-destination"
            | "--test-name-pattern" => {
                i += 2;
                continue;
            }
            _ if a.starts_with("--eval=") || a.starts_with("--print=") => {
                let code = a.split_once('=').map(|(_, c)| c).unwrap_or_default();
                out.extend(inline(program, Lang::JavaScript, code, ctx));
                return Some(out);
            }
            _ if a.starts_with("--require=") || a.starts_with("--import=") => {
                let m = a.split_once('=').map(|(_, m)| m).unwrap_or_default();
                if names_a_file(m) {
                    out.extend(read_script_file(program, Lang::of_js_file(m), m, ctx));
                }
                i += 1;
                continue;
            }
            _ if a.starts_with("--loader=") || a.starts_with("--experimental-loader=") => {
                return Some(unreadable(
                    program,
                    "loads a module loader kGraf does not read",
                ));
            }
            "--test" => {
                // `node --test` runs the test files it finds, as a test
                // runner does; the files named after it are read.
                let files: Vec<&String> = args[i + 1..]
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .collect();
                for f in files {
                    out.extend(read_script_file(program, Lang::of_js_file(f), f, ctx));
                }
                return Some(out);
            }
            _ if a.starts_with('-') => {
                i += 1;
                continue;
            }
            _ => break,
        }
    }
    match args.get(i) {
        Some(file) => {
            out.extend(read_script_file(program, Lang::of_js_file(file), file, ctx));
            Some(out)
        }
        None => Some(unreadable(program, "reads its code from its input")),
    }
}

/// `deno eval CODE`, `deno run FILE`, `deno FILE`, `deno task NAME`.
pub(super) fn deno(args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    let (sub, rest) = args.split_first()?;
    match sub.as_str() {
        "eval" => {
            let code = rest.iter().find(|a| !a.starts_with('-'))?;
            Some(inline("deno", Lang::TypeScript, code, ctx))
        }
        "run" | "serve" => {
            const VALUED: &[&str] = &[
                "-c",
                "--config",
                "--import-map",
                "--lock",
                "--cert",
                "--location",
                "--seed",
                "--env-file",
                "--node-modules-dir",
                "--watch-exclude",
            ];
            let mut i = 0;
            while let Some(a) = rest.get(i) {
                if !a.starts_with('-') {
                    break;
                }
                i += if VALUED.contains(&a.as_str()) { 2 } else { 1 };
            }
            let file = rest.get(i)?;
            Some(deno_file(file, ctx))
        }
        "task" => Some(unreadable(
            "deno",
            "runs a task of `deno.json`, which kGraf does not read",
        )),
        _ if sub.contains('.') && !sub.starts_with('-') => Some(deno_file(sub, ctx)),
        _ => None,
    }
}

fn deno_file(file: &str, ctx: &ReadContext) -> Vec<Embedded> {
    if file.contains("://") {
        return unreadable("deno", "runs code from a URL, which kGraf does not read");
    }
    read_script_file("deno", Lang::of_js_file(file), file, ctx)
}

/// `bun -e CODE`, `bun --print CODE`: `Some` when bun is given inline code.
pub(super) fn bun_inline(args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-e" | "--eval" | "-p" | "--print" => {
                let Some(code) = args.get(i + 1) else {
                    return Some(unreadable("bun", "names no code"));
                };
                return Some(inline("bun", Lang::TypeScript, code, ctx));
            }
            _ if a.starts_with('-') => i += 1,
            _ => return None,
        }
    }
    None
}

/// Perl modules that start programs or load code from text.
const PERL_OPAQUE_MODULES: &[&str] = &[
    "IPC::Open2",
    "IPC::Open3",
    "IPC::Run",
    "IPC::Run3",
    "IPC::Cmd",
    "IPC::System::Simple",
    "Capture::Tiny",
    "Expect",
    "Shell",
    "Proc::Background",
    "Proc::Simple",
    "Inline",
    "FFI::Platypus",
];

fn perl_call(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    if only_version(args) {
        return None;
    }
    let mut code: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if a == "-" {
            return Some(unreadable(program, "reads its code from its input"));
        }
        let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) else {
            break;
        };
        let mut consumed = 1;
        let mut skip_digits = false;
        for (k, c) in cluster.char_indices() {
            // `-l` and `-0` take octal digits, then the cluster goes on
            // (`-le CODE`).
            if skip_digits && c.is_ascii_digit() {
                continue;
            }
            skip_digits = false;
            match c {
                'l' | '0' => skip_digits = true,
                'e' | 'E' => {
                    let attached = &cluster[k + 1..];
                    if attached.is_empty() {
                        consumed = 2;
                        match args.get(i + 1) {
                            Some(v) => code.push(v.clone()),
                            None => return Some(unreadable(program, "names no code")),
                        }
                    } else {
                        code.push(attached.to_owned());
                    }
                    break;
                }
                'M' | 'm' => {
                    let attached = &cluster[k + 1..];
                    let module = if attached.is_empty() {
                        consumed = 2;
                        args.get(i + 1).cloned().unwrap_or_default()
                    } else {
                        attached.to_owned()
                    };
                    let name = module.trim_start_matches('-');
                    let name = name.split(['=', ' ']).next().unwrap_or_default();
                    if PERL_OPAQUE_MODULES.contains(&name) {
                        return Some(unreadable(
                            program,
                            &format!("loads `{name}`, which starts programs kGraf cannot read"),
                        ));
                    }
                    break;
                }
                'I' | 'F' | 'i' | 'C' => {
                    if c == 'I' && cluster[k + 1..].is_empty() {
                        consumed = 2;
                    }
                    break;
                }
                'x' | 'd' | 'S' => {
                    return Some(unreadable(
                        program,
                        &format!("is given `-{c}`, whose effect kGraf does not read"),
                    ));
                }
                _ => {}
            }
        }
        i += consumed;
    }
    if !code.is_empty() {
        return Some(inline(program, Lang::Perl, &code.join("\n"), ctx));
    }
    match args.get(i) {
        Some(file) => Some(read_script_file(program, Lang::Perl, file, ctx)),
        None => Some(unreadable(program, "reads its code from its input")),
    }
}

fn ruby_call(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    if only_version(args) {
        return None;
    }
    let mut code: Vec<String> = Vec::new();
    let mut out: Vec<Embedded> = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if a == "-" {
            return Some(unreadable(program, "reads its code from its input"));
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) else {
            break;
        };
        let mut consumed = 1;
        for (k, c) in cluster.char_indices() {
            match c {
                'e' | 'r' | 'I' | 'C' | 'E' | 'F' | 'K' | 'W' | '0' | 'T' | 'x' => {
                    let attached = &cluster[k + 1..];
                    let value = if attached.is_empty() && matches!(c, 'e' | 'r' | 'I' | 'C' | 'E') {
                        consumed = 2;
                        args.get(i + 1).cloned().unwrap_or_default()
                    } else {
                        attached.to_owned()
                    };
                    match c {
                        'e' => code.push(value),
                        'r' if names_a_file(&value) => {
                            out.extend(read_script_file(program, Lang::Ruby, &value, ctx));
                        }
                        'C' | 'x' => {
                            return Some(unreadable(
                                program,
                                &format!("is given `-{c}`, whose effect kGraf does not read"),
                            ));
                        }
                        _ => {}
                    }
                    break;
                }
                'S' => return Some(unreadable(program, "searches PATH for its script")),
                _ => {}
            }
        }
        i += consumed;
    }
    if !code.is_empty() {
        out.extend(inline(program, Lang::Ruby, &code.join("\n"), ctx));
        return Some(out);
    }
    match args.get(i) {
        Some(file) => {
            out.extend(read_script_file(program, Lang::Ruby, file, ctx));
            Some(out)
        }
        None => Some(unreadable(program, "reads its code from its input")),
    }
}

fn php_call(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    // Questions that run no code: a version, a syntax check, the build
    // information and the reflection options, which print and exit without
    // reading a script from input.
    const NO_CODE: &[&str] = &[
        "-l",
        "--syntax-check",
        "-i",
        "--info",
        "-m",
        "--modules",
        "--ini",
        "--rf",
        "--rc",
        "--re",
        "--rz",
        "--ri",
        "--rextension",
        "--rfunction",
        "--rclass",
        "--rzendextension",
        "--rextinfo",
    ];
    if only_version(args) || args.iter().any(|a| NO_CODE.contains(&a.as_str())) {
        return None;
    }
    let mut out = Vec::new();
    // Whether `-r` or a like option gave code: with no code and no script,
    // php reads its code from its input, as with no options at all.
    let mut code_given = false;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let next = args.get(i + 1);
        match a.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "-r" | "--run" | "-B" | "--process-begin" | "-R" | "--process-code" | "-E"
            | "--process-end" => {
                let Some(code) = next else {
                    return Some(unreadable(program, "names no code"));
                };
                out.extend(inline(program, Lang::PhpOnly, code, ctx));
                code_given = true;
                i += 2;
                continue;
            }
            "-f" | "--file" | "-F" | "--process-file" => {
                let Some(file) = next else {
                    return Some(unreadable(program, "names no file"));
                };
                out.extend(read_script_file(program, Lang::Php, file, ctx));
                return Some(out);
            }
            "-a" | "--interactive" => {
                return Some(unreadable(program, "reads its code from its input"));
            }
            "-S" | "--server" | "-z" | "--zend-extension" => {
                return Some(unreadable(
                    program,
                    &format!("is given `{a}`, which runs code kGraf does not read"),
                ));
            }
            "-d" | "--define" => {
                let setting = next.map(String::as_str).unwrap_or_default();
                if setting.starts_with("auto_prepend_file")
                    || setting.starts_with("auto_append_file")
                {
                    return Some(unreadable(
                        program,
                        "is told to run another file first, which kGraf does not read",
                    ));
                }
                i += 2;
                continue;
            }
            "-c" | "--php-ini" | "-t" | "--docroot" => {
                i += 2;
                continue;
            }
            _ if a.starts_with('-') => {
                i += 1;
                continue;
            }
            _ => break,
        }
    }
    if code_given {
        return Some(out);
    }
    match args.get(i) {
        Some(file) => {
            out.extend(read_script_file(program, Lang::Php, file, ctx));
            Some(out)
        }
        None => Some(unreadable(program, "reads its code from its input")),
    }
}

fn awk_call(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    if only_version(args) {
        return None;
    }
    let mut out = Vec::new();
    let mut programs_given = false;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let next = args.get(i + 1);
        match a.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "-f" | "--file" | "-E" | "--exec" => {
                let Some(file) = next else {
                    return Some(unreadable(program, "names no program file"));
                };
                out.extend(read_script_file(program, Lang::Awk, file, ctx));
                programs_given = true;
                i += 2;
            }
            "-e" | "--source" => {
                let Some(code) = next else {
                    return Some(unreadable(program, "names no program"));
                };
                out.extend(inline(program, Lang::Awk, code, ctx));
                programs_given = true;
                i += 2;
            }
            "-i" | "--include" | "-l" | "--load" => {
                return Some(unreadable(
                    program,
                    "loads a library or an extension kGraf does not read",
                ));
            }
            "-v" | "-F" | "--assign" | "--field-separator" | "-W" => i += 2,
            _ if a.starts_with("-f") && a.len() > 2 => {
                out.extend(read_script_file(program, Lang::Awk, &a[2..], ctx));
                programs_given = true;
                i += 1;
            }
            _ if a.starts_with("--source=") || a.starts_with("--file=") => {
                let v = a.split_once('=').map(|(_, v)| v).unwrap_or_default();
                if a.starts_with("--file=") {
                    out.extend(read_script_file(program, Lang::Awk, v, ctx));
                } else {
                    out.extend(inline(program, Lang::Awk, v, ctx));
                }
                programs_given = true;
                i += 1;
            }
            _ if a.starts_with('-') && a.len() > 1 => i += 1,
            _ => break,
        }
    }
    if !programs_given {
        match args.get(i) {
            Some(code) => out.extend(inline(program, Lang::Awk, code, ctx)),
            None => return Some(unreadable(program, "names no program")),
        }
    }
    Some(out)
}

fn osascript_call(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    let mut code: Vec<String> = Vec::new();
    let mut javascript = false;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-e" => {
                code.push(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "-l" => {
                javascript = args
                    .get(i + 1)
                    .is_some_and(|l| l.eq_ignore_ascii_case("javascript"));
                i += 2;
            }
            "-s" => i += 2,
            "-i" => return Some(unreadable(program, "reads its code from its input")),
            _ if a.starts_with('-') => i += 1,
            _ => break,
        }
    }
    // JavaScript for Automation reaches `doShellScript` and the Objective-C
    // bridge through any name; it has no reader.
    if javascript {
        return Some(unreadable(
            program,
            "runs JavaScript for Automation, which kGraf does not read",
        ));
    }
    let lang = Lang::AppleScript;
    if !code.is_empty() {
        return Some(inline(program, lang, &code.join("\n"), ctx));
    }
    match args.get(i) {
        Some(file) => Some(read_script_file(program, lang, file, ctx)),
        None => Some(unreadable(program, "reads its code from its input")),
    }
}

// ------------------------------------------------------- tree-sitter help

fn parse(lang: &Language, code: &str) -> Result<Tree, String> {
    let mut parser = Parser::new();
    parser
        .set_language(lang)
        .map_err(|e| format!("cannot be parsed here: {e}"))?;
    let tree = parser
        .parse(code, None)
        .ok_or_else(|| "does not parse".to_owned())?;
    if tree.root_node().has_error() {
        return Err("does not parse".into());
    }
    Ok(tree)
}

fn text<'a>(node: Node<'_>, code: &'a str) -> &'a str {
    code.get(node.byte_range()).unwrap_or_default()
}

/// Every node of the tree, depth first.
fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        out.push(n);
        let mut cursor = n.walk();
        let children: Vec<Node<'_>> = n.children(&mut cursor).collect();
        for c in children.into_iter().rev() {
            stack.push(c);
        }
    }
    out
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// Resolve the backslash escapes of a quoted literal, the numeric ones
/// included (`\x72`, `r`, `\u{72}`, `\U00000072`, `\162`): a command
/// string spelled with them names the program they decode to.
fn unescape(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    // Up to `max` digits of `radix` from `chars`, as a character.
    let number = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
                  radix: u32,
                  max: usize,
                  first: Option<char>|
     -> Option<char> {
        let mut digits: String = first.into_iter().collect();
        while digits.len() < max {
            match chars.peek() {
                Some(d) if d.is_digit(radix) => {
                    digits.push(*d);
                    chars.next();
                }
                _ => break,
            }
        }
        u32::from_str_radix(&digits, radix)
            .ok()
            .and_then(char::from_u32)
    };
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('f') => out.push('\x0c'),
            Some('v') => out.push('\x0b'),
            Some('e') => out.push('\x1b'),
            Some('\n') => {}
            Some(d @ '0'..='7') => out.push(number(&mut chars, 8, 3, Some(d)).unwrap_or(d)),
            Some('x') => match number(&mut chars, 16, 2, None) {
                Some(ch) => out.push(ch),
                None => out.push('x'),
            },
            Some('u') if chars.peek() == Some(&'{') => {
                chars.next();
                let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => out.push('u'),
                }
            }
            Some('u') => match number(&mut chars, 16, 4, None) {
                Some(ch) => out.push(ch),
                None => out.push('u'),
            },
            Some('U') => match number(&mut chars, 16, 8, None) {
                Some(ch) => out.push(ch),
                None => out.push('U'),
            },
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// A literal argument a spawning call was given.
#[derive(Clone, Debug)]
enum Lit {
    Str(String),
    List(Vec<String>),
}

/// What a spawning call's literal arguments start: a string is a script, a
/// list an argv (the first word the program).
fn spawned(first: Lit, rest: &[String], shell: bool) -> Vec<Embedded> {
    match first {
        Lit::Str(s) if rest.is_empty() => vec![Embedded::Script(s)],
        Lit::Str(s) if shell => {
            let mut script = s;
            for r in rest {
                script.push(' ');
                script.push_str(r);
            }
            vec![Embedded::Script(script)]
        }
        Lit::Str(s) => {
            let mut argv = vec![s];
            argv.extend(rest.iter().cloned());
            vec![Embedded::Argv(argv)]
        }
        Lit::List(list) if list.is_empty() => Vec::new(),
        Lit::List(list) if shell => vec![Embedded::Script(list.join(" "))],
        Lit::List(list) => vec![Embedded::Argv(list)],
    }
}

fn dynamic(what: &str) -> String {
    format!("starts a program with {what}, so kGraf cannot tell what it runs")
}

// ------------------------------------------------------------------ Python

mod python {
    use super::*;

    /// Modules whose functions start programs, read call by call.
    const SPAWN_MODULES: &[&str] = &[
        "os",
        "subprocess",
        "pty",
        "asyncio",
        "platform",
        "commands",
        "popen2",
        "posix",
        "nt",
    ];

    /// Modules whose import makes code unreadable: they start programs
    /// through an interface kGraf does not read, load native code, or run
    /// text as code.
    const OPAQUE_MODULES: &[&str] = &[
        "ctypes",
        "cffi",
        "sh",
        "plumbum",
        "pexpect",
        "invoke",
        "fabric",
        "delegator",
        "envoy",
        "sarge",
        "importlib",
        "runpy",
        "code",
        "codeop",
        "pickle",
        "cPickle",
        "dill",
        "cloudpickle",
        "marshal",
        "shelve",
        "_posixsubprocess",
        "_winapi",
        "multiprocessing",
        "concurrent",
        "pyautogui",
        "webbrowser",
        "builtins",
        "__builtin__",
    ];

    /// Names only a spawning function has: a string holding one, in code
    /// that imports a spawning module, may reach it by name
    /// (`operator.methodcaller("system", ...)`, `os.__dict__["system"]`).
    const SPAWN_NAMES: &[&str] = &[
        "system",
        "popen",
        "Popen",
        "check_output",
        "check_call",
        "getoutput",
        "getstatusoutput",
        "execl",
        "execle",
        "execlp",
        "execlpe",
        "execv",
        "execve",
        "execvp",
        "execvpe",
        "spawnl",
        "spawnle",
        "spawnlp",
        "spawnlpe",
        "spawnv",
        "spawnve",
        "spawnvp",
        "spawnvpe",
        "posix_spawn",
        "posix_spawnp",
        "create_subprocess_exec",
        "create_subprocess_shell",
        "startfile",
    ];

    /// How a spawning function takes its command.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Form {
        /// A string run by a shell.
        Shell,
        /// A string (a script, with `shell=True`) or a list (an argv).
        Popen,
        /// `prog, *args`.
        ExecArgs,
        /// `file, arg0, args...` (`os.execl`).
        ExecL,
        /// `file, args_list` (`os.execv`).
        ExecV,
        /// `mode, file, arg0, args...` (`os.spawnl`).
        SpawnL,
        /// `mode, file, args_list` (`os.spawnv`).
        SpawnV,
        /// `path, argv_list, env` (`os.posix_spawn`).
        PosixSpawn,
        /// A list (an argv) or a string (a program).
        Pty,
        /// A program path.
        Start,
    }

    /// How the function `q` names takes its command. A spawning module
    /// reached as another module's attribute (`shutil.os.system`,
    /// `tempfile._os.system`, `asyncio.subprocess`) is that module.
    fn form(q: &str) -> Option<Form> {
        let (module, name) = q.rsplit_once('.')?;
        std::iter::once(module)
            .chain(module.split('.').map(|s| s.trim_start_matches('_')))
            .find_map(|m| form_of(m, name))
    }

    fn form_of(module: &str, name: &str) -> Option<Form> {
        let module = match module {
            "posix" | "nt" => "os",
            m => m,
        };
        Some(match (module, name) {
            ("os", "system" | "popen" | "popen2" | "popen3" | "popen4") => Form::Shell,
            ("subprocess", "getoutput" | "getstatusoutput") => Form::Shell,
            ("commands", "getoutput" | "getstatusoutput") => Form::Shell,
            ("platform", "popen") => Form::Shell,
            ("popen2", _) => Form::Shell,
            ("asyncio" | "subprocess", "create_subprocess_shell") => Form::Shell,
            ("distutils.spawn" | "spawn", "spawn") => Form::Pty,
            ("subprocess", "run" | "call" | "check_call" | "check_output" | "Popen") => Form::Popen,
            ("asyncio" | "subprocess", "create_subprocess_exec") => Form::ExecArgs,
            ("os", "execl" | "execle" | "execlp" | "execlpe") => Form::ExecL,
            ("os", "execv" | "execve" | "execvp" | "execvpe" | "fexecve") => Form::ExecV,
            ("os", "spawnl" | "spawnle" | "spawnlp" | "spawnlpe") => Form::SpawnL,
            ("os", "spawnv" | "spawnve" | "spawnvp" | "spawnvpe") => Form::SpawnV,
            ("os", "posix_spawn" | "posix_spawnp") => Form::PosixSpawn,
            ("pty", "spawn") => Form::Pty,
            ("os", "startfile") => Form::Start,
            _ => return None,
        })
    }

    /// Builtins that run text as code or reach any function by name.
    const DYNAMIC_BUILTINS: &[&str] = &[
        "exec",
        "eval",
        "compile",
        "__import__",
        "globals",
        "locals",
        "vars",
        "breakpoint",
        "__builtins__",
    ];

    fn literal(node: Node<'_>, code: &str) -> Option<String> {
        match node.kind() {
            "string" => {
                let mut out = String::new();
                let raw = named_children(node).first().is_some_and(|s| {
                    s.kind() == "string_start" && text(*s, code).to_ascii_lowercase().contains('r')
                });
                for c in named_children(node) {
                    match c.kind() {
                        "string_start" | "string_end" => {}
                        "string_content" => {
                            let t = text(c, code);
                            out.push_str(&if raw { t.to_owned() } else { unescape(t) });
                        }
                        "escape_sequence" => out.push_str(&unescape(text(c, code))),
                        _ => return None,
                    }
                }
                Some(out)
            }
            "concatenated_string" => {
                let mut out = String::new();
                for c in named_children(node) {
                    out.push_str(&literal(c, code)?);
                }
                Some(out)
            }
            "parenthesized_expression" => {
                named_children(node).first().and_then(|c| literal(*c, code))
            }
            _ => None,
        }
    }

    fn literal_list(node: Node<'_>, code: &str) -> Option<Vec<String>> {
        match node.kind() {
            "list" | "tuple" => named_children(node)
                .into_iter()
                .map(|c| literal(c, code))
                .collect(),
            _ => None,
        }
    }

    fn lit(node: Node<'_>, code: &str) -> Option<Lit> {
        literal(node, code)
            .map(Lit::Str)
            .or_else(|| literal_list(node, code).map(Lit::List))
    }

    /// The qualified name `node` refers to: an imported name through its
    /// alias, an attribute of one, or a builtin. `None` for anything else.
    fn qualified(node: Node<'_>, code: &str, aliases: &HashMap<String, String>) -> Option<String> {
        match node.kind() {
            "identifier" => {
                let name = text(node, code);
                aliases
                    .get(name)
                    .cloned()
                    .or_else(|| {
                        DYNAMIC_BUILTINS
                            .contains(&name)
                            .then(|| format!("builtins.{name}"))
                    })
                    .or_else(|| {
                        (name == "getattr" || name == "setattr").then(|| format!("builtins.{name}"))
                    })
            }
            "attribute" => {
                let object = node.child_by_field_name("object")?;
                let attr = node.child_by_field_name("attribute")?;
                Some(format!(
                    "{}.{}",
                    qualified(object, code, aliases)?,
                    text(attr, code)
                ))
            }
            _ => None,
        }
    }

    pub(super) fn scan(code: &str) -> ScanResult {
        let lang: Language = tree_sitter_python::LANGUAGE.into();
        let tree = parse(&lang, code)?;
        let all = nodes(tree.root_node());
        let mut aliases: HashMap<String, String> = HashMap::new();
        let mut imports: Vec<String> = Vec::new();
        // Imports first: an alias may be used before its line in a
        // function body.
        for n in &all {
            match n.kind() {
                "import_statement" => {
                    for c in named_children(*n) {
                        let (module, local) = match c.kind() {
                            "dotted_name" => {
                                let m = text(c, code).to_owned();
                                let first = m.split('.').next().unwrap_or_default().to_owned();
                                (m, first.clone())
                            }
                            "aliased_import" => {
                                let m = c
                                    .child_by_field_name("name")
                                    .map(|x| text(x, code).to_owned())
                                    .unwrap_or_default();
                                let a = c
                                    .child_by_field_name("alias")
                                    .map(|x| text(x, code).to_owned())
                                    .unwrap_or_default();
                                (m.clone(), a)
                            }
                            _ => continue,
                        };
                        check_module(&module)?;
                        let root = module.split('.').next().unwrap_or_default().to_owned();
                        if module.contains('.') && c.kind() == "dotted_name" {
                            aliases.insert(local, root);
                        } else {
                            aliases.insert(local, module.clone());
                        }
                        imports.push(module);
                    }
                }
                "import_from_statement" => {
                    let module_node = n.child_by_field_name("module_name");
                    let module = module_node
                        .map(|m| text(m, code).to_owned())
                        .unwrap_or_default();
                    check_module(&module)?;
                    let mut star = false;
                    let mut names: Vec<(String, String)> = Vec::new();
                    for c in named_children(*n) {
                        if Some(c) == module_node {
                            continue;
                        }
                        match c.kind() {
                            "wildcard_import" => star = true,
                            "dotted_name" => {
                                let name = text(c, code).to_owned();
                                names.push((name.clone(), name));
                            }
                            "aliased_import" => {
                                let name = c
                                    .child_by_field_name("name")
                                    .map(|x| text(x, code).to_owned())
                                    .unwrap_or_default();
                                let alias = c
                                    .child_by_field_name("alias")
                                    .map(|x| text(x, code).to_owned())
                                    .unwrap_or_default();
                                names.push((name, alias));
                            }
                            _ => {}
                        }
                    }
                    let root = module
                        .trim_start_matches('.')
                        .split('.')
                        .next()
                        .unwrap_or_default();
                    if star && SPAWN_MODULES.contains(&root) {
                        return Err(format!(
                            "imports every name of `{module}`, so kGraf cannot tell which call \
                             starts a program"
                        ));
                    }
                    for (name, alias) in names {
                        let q = format!("{module}.{name}");
                        check_module(&q)?;
                        aliases.insert(alias, q);
                        if module.starts_with('.') {
                            imports.push(format!("{module}.{name}").replace("..", "."));
                        }
                    }
                    imports.push(module);
                }
                _ => {}
            }
        }
        // Whether the code imports a module whose functions start programs.
        let imports_spawning = aliases
            .values()
            .any(|q| SPAWN_MODULES.contains(&q.split('.').next().unwrap_or_default()));
        let mut found = Vec::new();
        for n in &all {
            match n.kind() {
                "call" => {
                    let Some(function) = n.child_by_field_name("function") else {
                        continue;
                    };
                    let Some(args) = n.child_by_field_name("arguments") else {
                        continue;
                    };
                    if function.kind() == "attribute"
                        && let Some(attr) = function.child_by_field_name("attribute")
                        && matches!(text(attr, code), "subprocess_exec" | "subprocess_shell")
                    {
                        return Err(dynamic("an event loop's subprocess call"));
                    }
                    let Some(q) = qualified(function, code, &aliases) else {
                        continue;
                    };
                    if let Some(name) = q.strip_prefix("builtins.") {
                        if name == "getattr" || name == "setattr" {
                            // Only a lookup on a spawning module reaches a
                            // spawning function by a name kGraf cannot see.
                            let target = named_children(args).first().copied();
                            let on_module = target
                                .and_then(|t| qualified(t, code, &aliases))
                                .is_some_and(|t| {
                                    SPAWN_MODULES.contains(&t.split('.').next().unwrap_or_default())
                                        || t.starts_with("builtins")
                                });
                            if on_module {
                                return Err(dynamic("a function looked up by name"));
                            }
                            continue;
                        }
                        return Err(format!("runs text as code (`{name}`)"));
                    }
                    if let Some(f) = form(&q) {
                        found.extend(read_call(f, &q, args, code)?);
                    }
                }
                "identifier" | "attribute" => {
                    // A spawning function referred to other than as a
                    // call's function, or a spawning module other than as
                    // an attribute's object. A spawning function as an
                    // attribute's object (`os.system.__call__`) is a value.
                    let parent = n.parent();
                    if parent.is_some_and(|p| p.kind() == "attribute")
                        && parent.and_then(|p| p.child_by_field_name("object")) == Some(*n)
                        && qualified(*n, code, &aliases).is_none_or(|q| form(&q).is_none())
                    {
                        continue;
                    }
                    if parent.is_some_and(|p| {
                        matches!(
                            p.kind(),
                            "import_statement"
                                | "import_from_statement"
                                | "aliased_import"
                                | "dotted_name"
                        )
                    }) {
                        continue;
                    }
                    let Some(q) = qualified(*n, code, &aliases) else {
                        continue;
                    };
                    let called = parent.is_some_and(|p| {
                        p.kind() == "call" && p.child_by_field_name("function") == Some(*n)
                    });
                    if called {
                        continue;
                    }
                    if form(&q).is_some() {
                        return Err(dynamic("a spawning function used as a value"));
                    }
                    if SPAWN_MODULES.contains(&q.as_str()) && n.kind() == "identifier" {
                        // `m = os` hands the module on.
                        if parent.is_some_and(|p| p.kind() != "attribute") {
                            return Err(dynamic("a spawning module used as a value"));
                        }
                    }
                    if q.starts_with("builtins.") {
                        return Err(dynamic("a builtin that reaches any function"));
                    }
                    // A spawning module's own attributes (`os.__dict__`) and
                    // the table of loaded modules (`sys.modules`) reach its
                    // functions by name.
                    let root = q.split('.').next().unwrap_or_default();
                    let last = q.rsplit('.').next().unwrap_or_default();
                    if (SPAWN_MODULES.contains(&root) && last.starts_with("__"))
                        || q == "sys.modules"
                    {
                        return Err(dynamic("a module's functions reached by name"));
                    }
                }
                "string" if imports_spawning => {
                    if let Some(s) = literal(*n, code)
                        && SPAWN_NAMES.contains(&s.as_str())
                    {
                        return Err(dynamic(&format!("the name `{s}` given as text")));
                    }
                }
                _ => {}
            }
        }
        let imports = imports.into_iter().filter(|m| !m.is_empty()).collect();
        Ok(Scan { found, imports })
    }

    fn check_module(module: &str) -> Result<(), String> {
        let root = module
            .trim_start_matches('.')
            .split('.')
            .next()
            .unwrap_or_default();
        if !module.starts_with('.') && OPAQUE_MODULES.contains(&root) {
            return Err(format!(
                "imports `{root}`, which starts programs or runs code in ways kGraf does not read"
            ));
        }
        Ok(())
    }

    /// The commands one spawning call starts, from its literal arguments.
    fn read_call(form: Form, q: &str, args: Node<'_>, code: &str) -> Result<Vec<Embedded>, String> {
        let mut positional: Vec<Node<'_>> = Vec::new();
        let mut shell = false;
        let mut executable: Option<String> = None;
        let mut moved = false;
        for a in named_children(args) {
            match a.kind() {
                "keyword_argument" => {
                    let name = a
                        .child_by_field_name("name")
                        .map(|x| text(x, code))
                        .unwrap_or_default();
                    let value = a.child_by_field_name("value");
                    match name {
                        "shell" => {
                            shell = value.is_some_and(|v| text(v, code) != "False");
                        }
                        "executable" => {
                            executable =
                                Some(value.and_then(|v| literal(v, code)).ok_or_else(|| {
                                    dynamic("an `executable` that is not literal")
                                })?);
                        }
                        "cwd" => moved = true,
                        "args" => {
                            positional.insert(0, value.ok_or_else(|| dynamic("no arguments"))?)
                        }
                        _ => {}
                    }
                }
                "list_splat" | "dictionary_splat" => {
                    return Err(dynamic("arguments unpacked from a value"));
                }
                "comment" => {}
                _ => positional.push(a),
            }
        }
        let need_lit = |n: Option<&Node<'_>>| -> Result<Lit, String> {
            n.and_then(|n| lit(*n, code))
                .ok_or_else(|| dynamic(&format!("arguments to `{q}` that are not literal")))
        };
        let strs = |nodes: &[Node<'_>]| -> Result<Vec<String>, String> {
            nodes
                .iter()
                .map(|n| {
                    literal(*n, code)
                        .ok_or_else(|| dynamic(&format!("arguments to `{q}` that are not literal")))
                })
                .collect()
        };
        let mut found = match form {
            Form::Shell => match need_lit(positional.first())? {
                Lit::Str(s) => vec![Embedded::Script(s)],
                Lit::List(l) => vec![Embedded::Argv(l)],
            },
            Form::Popen => spawned(need_lit(positional.first())?, &[], shell),
            Form::ExecArgs => {
                let words = strs(&positional)?;
                if words.is_empty() {
                    Vec::new()
                } else {
                    vec![Embedded::Argv(words)]
                }
            }
            Form::ExecL => {
                // `execle` and `execlpe` end with an environment mapping.
                let end = if q.ends_with('e') {
                    positional.len().saturating_sub(1)
                } else {
                    positional.len()
                };
                let words = strs(&positional[..end])?;
                match words.split_first() {
                    Some((file, rest)) => {
                        let mut argv = vec![file.clone()];
                        argv.extend(rest.iter().skip(1).cloned());
                        vec![Embedded::Argv(argv)]
                    }
                    None => Vec::new(),
                }
            }
            Form::ExecV | Form::PosixSpawn => {
                let file = positional.first().and_then(|n| literal(*n, code));
                let list = positional.get(1).and_then(|n| literal_list(*n, code));
                match (file, list) {
                    (Some(f), Some(l)) => {
                        let mut argv = vec![f];
                        argv.extend(l.into_iter().skip(1));
                        vec![Embedded::Argv(argv)]
                    }
                    _ => return Err(dynamic(&format!("arguments to `{q}` that are not literal"))),
                }
            }
            Form::SpawnL => {
                // The mode (`os.P_WAIT`) comes first.
                let end = if q.ends_with('e') {
                    positional.len().saturating_sub(1)
                } else {
                    positional.len()
                };
                let words = strs(positional.get(1..end).unwrap_or_default())?;
                match words.split_first() {
                    Some((file, rest)) => {
                        let mut argv = vec![file.clone()];
                        argv.extend(rest.iter().skip(1).cloned());
                        vec![Embedded::Argv(argv)]
                    }
                    None => Vec::new(),
                }
            }
            Form::SpawnV => {
                let file = positional.get(1).and_then(|n| literal(*n, code));
                let list = positional.get(2).and_then(|n| literal_list(*n, code));
                match (file, list) {
                    (Some(f), Some(l)) => {
                        let mut argv = vec![f];
                        argv.extend(l.into_iter().skip(1));
                        vec![Embedded::Argv(argv)]
                    }
                    _ => return Err(dynamic(&format!("arguments to `{q}` that are not literal"))),
                }
            }
            Form::Pty | Form::Start => match need_lit(positional.first())? {
                Lit::Str(s) => vec![Embedded::Argv(vec![s])],
                Lit::List(l) => vec![Embedded::Argv(l)],
            },
        };
        if let Some(exe) = executable {
            // `executable=` names the program; the list's first word is
            // only its name.
            found = found
                .into_iter()
                .map(|e| match e {
                    Embedded::Argv(mut a) if !a.is_empty() => {
                        a[0] = exe.clone();
                        Embedded::Argv(a)
                    }
                    Embedded::Script(_) => Embedded::Argv(vec![exe.clone()]),
                    other => other,
                })
                .collect();
        }
        if moved {
            found = found
                .into_iter()
                .map(|e| Embedded::In(ReadContext::default(), Box::new(e)))
                .collect();
        }
        Ok(found)
    }
}

// ------------------------------------------------------ JavaScript family

mod js {
    use super::*;

    const SPAWN_MODULES: &[&str] = &["child_process", "node:child_process"];

    /// Modules whose import makes code unreadable.
    const OPAQUE_MODULES: &[&str] = &[
        "vm",
        "node:vm",
        "worker_threads",
        "node:worker_threads",
        "shelljs",
        "execa",
        "zx",
        "cross-spawn",
        "child-process-promise",
        "node-pty",
        "ffi-napi",
        "koffi",
        "module",
        "node:module",
        "inspector",
        "node:inspector",
        "repl",
        "node:repl",
    ];

    fn literal(node: Node<'_>, code: &str) -> Option<String> {
        match node.kind() {
            "string" => {
                let mut out = String::new();
                for c in named_children(node) {
                    match c.kind() {
                        "string_fragment" => out.push_str(text(c, code)),
                        "escape_sequence" => out.push_str(&unescape(text(c, code))),
                        _ => return None,
                    }
                }
                Some(out)
            }
            "template_string" => {
                let mut out = String::new();
                for c in named_children(node) {
                    match c.kind() {
                        "string_fragment" => out.push_str(text(c, code)),
                        "escape_sequence" => out.push_str(&unescape(text(c, code))),
                        _ => return None,
                    }
                }
                Some(out)
            }
            "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression" => named_children(node).first().and_then(|c| literal(*c, code)),
            _ => None,
        }
    }

    fn literal_list(node: Node<'_>, code: &str) -> Option<Vec<String>> {
        match node.kind() {
            "array" => named_children(node)
                .into_iter()
                .map(|c| literal(c, code))
                .collect(),
            "as_expression" | "satisfies_expression" | "parenthesized_expression" => {
                named_children(node)
                    .first()
                    .and_then(|c| literal_list(*c, code))
            }
            _ => None,
        }
    }

    /// The value of property `key` in an object literal.
    fn property<'t>(object: Node<'t>, key: &str, code: &str) -> Option<Node<'t>> {
        named_children(object).into_iter().find_map(|p| {
            (p.kind() == "pair"
                && p.child_by_field_name("key")
                    .is_some_and(|k| text(k, code).trim_matches(['\'', '"']) == key))
            .then(|| p.child_by_field_name("value"))
            .flatten()
        })
    }

    /// The module a `require('x')` call or an `import('x')` loads.
    fn required(node: Node<'_>, code: &str) -> Option<Result<String, ()>> {
        if node.kind() != "call_expression" {
            return None;
        }
        let f = node.child_by_field_name("function")?;
        // `require` reached as a method loads a module too
        // (`module.require`, `process.mainModule.require`).
        let as_method = f.kind() == "member_expression"
            && f.child_by_field_name("property")
                .is_some_and(|p| text(p, code) == "require");
        if !as_method && !matches!(text(f, code), "require" | "import") {
            return None;
        }
        let args = node.child_by_field_name("arguments")?;
        let first = named_children(args).first().copied();
        Some(first.and_then(|a| literal(a, code)).ok_or(()))
    }

    /// The qualified name a callee refers to.
    fn qualified(node: Node<'_>, code: &str, aliases: &HashMap<String, String>) -> Option<String> {
        match node.kind() {
            "identifier" => {
                let name = text(node, code);
                aliases.get(name).cloned().or_else(|| {
                    matches!(
                        name,
                        "Bun" | "Deno" | "process" | "eval" | "Function" | "globalThis"
                    )
                    .then(|| name.to_owned())
                })
            }
            "member_expression" => {
                let object = node.child_by_field_name("object")?;
                let property = node.child_by_field_name("property")?;
                let base = match required(object, code) {
                    Some(Ok(m)) => m,
                    _ => qualified(object, code, aliases)?,
                };
                Some(format!("{base}.{}", text(property, code)))
            }
            "subscript_expression" => {
                let object = node.child_by_field_name("object")?;
                let base = match required(object, code) {
                    Some(Ok(m)) => m,
                    _ => qualified(object, code, aliases)?,
                };
                Some(format!("{base}[]"))
            }
            "parenthesized_expression" | "non_null_expression" => named_children(node)
                .first()
                .and_then(|c| qualified(*c, code, aliases)),
            _ => None,
        }
    }

    fn is_spawn_module(q: &str) -> bool {
        SPAWN_MODULES.contains(&q)
    }

    fn spawn_function(q: &str) -> Option<&'static str> {
        let (module, name) = q.rsplit_once('.')?;
        if is_spawn_module(module) {
            return match name {
                "exec" | "execSync" => Some("shell"),
                "execFile" | "execFileSync" | "spawn" | "spawnSync" => Some("argv"),
                "fork" => Some("fork"),
                _ => None,
            };
        }
        match q {
            "Bun.spawn" | "Bun.spawnSync" | "bun.spawn" | "bun.spawnSync" => Some("bun"),
            "Bun.$" | "bun.$" => Some("template"),
            "Deno.run" => Some("deno-run"),
            "Deno.Command" => Some("deno-command"),
            _ => None,
        }
    }

    pub(super) fn scan(lang: Lang, code: &str) -> ScanResult {
        let language: Language = match lang {
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            _ => tree_sitter_javascript::LANGUAGE.into(),
        };
        // A `#!` line is not JavaScript; the runtime skips it.
        let body: String = if code.starts_with("#!") {
            code.lines()
                .enumerate()
                .map(|(i, l)| if i == 0 { String::new() } else { l.to_owned() })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            code.to_owned()
        };
        let code = body.as_str();
        let tree = parse(&language, code)?;
        let all = nodes(tree.root_node());
        let mut aliases: HashMap<String, String> = HashMap::new();
        let mut imports: Vec<String> = Vec::new();
        let check = |m: &str| -> Result<(), String> {
            if OPAQUE_MODULES.contains(&m) {
                return Err(format!(
                    "loads `{m}`, which starts programs or runs code in ways kGraf does not read"
                ));
            }
            Ok(())
        };
        for n in &all {
            match n.kind() {
                "import_statement" => {
                    let Some(source) = n
                        .child_by_field_name("source")
                        .and_then(|s| literal(s, code))
                    else {
                        continue;
                    };
                    check(&source)?;
                    if names_a_file(&source) {
                        imports.push(source.clone());
                    }
                    for clause in named_children(*n)
                        .into_iter()
                        .filter(|c| c.kind() == "import_clause")
                    {
                        for c in named_children(clause) {
                            match c.kind() {
                                "identifier" => {
                                    aliases.insert(text(c, code).to_owned(), source.clone());
                                }
                                "namespace_import" => {
                                    if let Some(id) = named_children(c)
                                        .into_iter()
                                        .find(|x| x.kind() == "identifier")
                                    {
                                        aliases.insert(text(id, code).to_owned(), source.clone());
                                    }
                                }
                                "named_imports" => {
                                    for spec in named_children(c) {
                                        if spec.kind() != "import_specifier" {
                                            continue;
                                        }
                                        let name = spec
                                            .child_by_field_name("name")
                                            .map(|x| text(x, code).to_owned())
                                            .unwrap_or_default();
                                        let alias = spec
                                            .child_by_field_name("alias")
                                            .map(|x| text(x, code).to_owned())
                                            .unwrap_or_else(|| name.clone());
                                        aliases.insert(alias, format!("{source}.{name}"));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "call_expression" => {
                    if let Some(r) = required(*n, code) {
                        let Ok(m) = r else {
                            return Err("loads a module named by a value kGraf cannot see".into());
                        };
                        check(&m)?;
                        if names_a_file(&m) {
                            imports.push(m.clone());
                        }
                        // `const cp = require('child_process')`, `const {
                        // exec } = require(...)`.
                        if let Some(decl) = n.parent().filter(|p| p.kind() == "variable_declarator")
                        {
                            if let Some(name) = decl.child_by_field_name("name") {
                                match name.kind() {
                                    "identifier" => {
                                        aliases.insert(text(name, code).to_owned(), m.clone());
                                    }
                                    "object_pattern" => {
                                        for p in named_children(name) {
                                            match p.kind() {
                                                "shorthand_property_identifier_pattern" => {
                                                    let k = text(p, code).to_owned();
                                                    aliases.insert(k.clone(), format!("{m}.{k}"));
                                                }
                                                "pair_pattern" => {
                                                    let k = p
                                                        .child_by_field_name("key")
                                                        .map(|x| text(x, code).to_owned())
                                                        .unwrap_or_default();
                                                    let v = p
                                                        .child_by_field_name("value")
                                                        .map(|x| text(x, code).to_owned())
                                                        .unwrap_or_default();
                                                    aliases.insert(v, format!("{m}.{k}"));
                                                }
                                                _ => {
                                                    return Err(dynamic(
                                                        "names unpacked from a module kGraf cannot place",
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        } else if is_spawn_module(&m)
                            && n.parent().is_none_or(|p| {
                                // Only a spawning function read from it
                                // (`require('child_process').exec`) is
                                // followed; `.then(...)` hands it on.
                                p.kind() != "member_expression"
                                    || p.child_by_field_name("property").is_none_or(|prop| {
                                        spawn_function(&format!("{m}.{}", text(prop, code)))
                                            .is_none()
                                    })
                            })
                        {
                            return Err(dynamic("the process module used as a value"));
                        }
                    }
                }
                _ => {}
            }
        }
        // Whether the code loads the process module: a string naming one of
        // its functions may then reach it (`Reflect.get(cp, "exec")`).
        let loads_process = aliases.values().any(|q| {
            is_spawn_module(q.split('.').next().unwrap_or_default()) || is_spawn_module(q)
        }) || all
            .iter()
            .any(|n| matches!(required(*n, code), Some(Ok(m)) if is_spawn_module(&m)));
        let mut found = Vec::new();
        for n in &all {
            match n.kind() {
                "string" if loads_process => {
                    if let Some(s) = literal(*n, code)
                        && matches!(
                            s.as_str(),
                            "exec"
                                | "execSync"
                                | "execFile"
                                | "execFileSync"
                                | "spawn"
                                | "spawnSync"
                                | "fork"
                        )
                    {
                        return Err(dynamic(&format!("the function `{s}` named by text")));
                    }
                }
                "call_expression" | "new_expression" => {
                    let callee = if n.kind() == "call_expression" {
                        n.child_by_field_name("function")
                    } else {
                        n.child_by_field_name("constructor")
                    };
                    let Some(callee) = callee else { continue };
                    let args = n.child_by_field_name("arguments");
                    let Some(q) = qualified(callee, code, &aliases) else {
                        continue;
                    };
                    if q.ends_with("[]")
                        && (is_spawn_module(q.trim_end_matches("[]"))
                            || q.starts_with("Bun")
                            || q.starts_with("Deno")
                            || q.starts_with("globalThis")
                            || q.starts_with("process"))
                    {
                        return Err(dynamic("a function looked up by a computed name"));
                    }
                    if matches!(
                        q.as_str(),
                        "eval" | "Function" | "globalThis.eval" | "globalThis.Function"
                    ) {
                        return Err("runs text as code".into());
                    }
                    if matches!(
                        q.as_str(),
                        "process.binding" | "process._linkedBinding" | "process.dlopen"
                    ) {
                        return Err("loads native code kGraf does not read".into());
                    }
                    if matches!(q.as_str(), "setTimeout" | "setInterval")
                        && args
                            .and_then(|a| named_children(a).first().copied())
                            .is_some_and(|a| literal(a, code).is_some())
                    {
                        return Err("runs text as code".into());
                    }
                    let Some(kind) = spawn_function(&q) else {
                        continue;
                    };
                    let Some(args) = args else { continue };
                    found.extend(read_call(kind, &q, args, code)?);
                }
                "identifier" | "member_expression" => {
                    let parent = n.parent();
                    let is_callee = parent.is_some_and(|p| {
                        (p.kind() == "call_expression"
                            && p.child_by_field_name("function") == Some(*n))
                            || (p.kind() == "new_expression"
                                && p.child_by_field_name("constructor") == Some(*n))
                    });
                    let is_object = parent.is_some_and(|p| {
                        p.kind() == "member_expression"
                            && p.child_by_field_name("object") == Some(*n)
                    });
                    if is_callee || is_object {
                        continue;
                    }
                    // The name a declaration binds, or a name an import or
                    // a pattern binds, is not a use.
                    let binds = parent.is_some_and(|p| {
                        (p.kind() == "variable_declarator"
                            && p.child_by_field_name("name") == Some(*n))
                            || matches!(
                                p.kind(),
                                "import_specifier"
                                    | "namespace_import"
                                    | "import_clause"
                                    | "shorthand_property_identifier_pattern"
                                    | "pair_pattern"
                                    | "object_pattern"
                            )
                    });
                    if binds {
                        continue;
                    }
                    let Some(q) = qualified(*n, code, &aliases) else {
                        continue;
                    };
                    if spawn_function(&q).is_some() {
                        return Err(dynamic("a spawning function used as a value"));
                    }
                    if is_spawn_module(&q) || q == "eval" || q == "Function" {
                        return Err(dynamic("the process module or `eval` used as a value"));
                    }
                    if matches!(
                        q.as_str(),
                        "process.binding" | "process._linkedBinding" | "process.dlopen"
                    ) {
                        return Err("loads native code kGraf does not read".into());
                    }
                }
                _ => {}
            }
        }
        Ok(Scan { found, imports })
    }

    fn read_call(kind: &str, q: &str, args: Node<'_>, code: &str) -> Result<Vec<Embedded>, String> {
        let not_literal = || dynamic(&format!("arguments to `{q}` that are not literal"));
        if kind == "template" {
            // ``Bun.$`curl ${url}` ``: each substitution is one argument;
            // the program word must be literal.
            if args.kind() != "template_string" {
                return Err(not_literal());
            }
            let mut script = String::new();
            for c in named_children(args) {
                match c.kind() {
                    "string_fragment" => script.push_str(text(c, code)),
                    "escape_sequence" => script.push_str(&unescape(text(c, code))),
                    "template_substitution" => {
                        if script.trim().is_empty() {
                            return Err(dynamic("a program named by a substitution"));
                        }
                        script.push_str("ARG");
                    }
                    _ => return Err(not_literal()),
                }
            }
            return Ok(vec![Embedded::Script(script)]);
        }
        let list: Vec<Node<'_>> = named_children(args)
            .into_iter()
            .filter(|c| c.kind() != "comment")
            .collect();
        if list.iter().any(|c| c.kind() == "spread_element") {
            return Err(dynamic("arguments spread from a value"));
        }
        let first = list.first().copied();
        // An options object: `{ shell: true }` runs the command in a shell;
        // `cwd` moves it.
        let options = list.iter().find(|c| c.kind() == "object").copied();
        let shell = options
            .and_then(|o| property(o, "shell", code))
            .is_some_and(|v| text(v, code) != "false");
        let moved = options.and_then(|o| property(o, "cwd", code)).is_some();
        let found = match kind {
            "shell" => vec![Embedded::Script(
                first
                    .and_then(|f| literal(f, code))
                    .ok_or_else(not_literal)?,
            )],
            "argv" => {
                let program = first
                    .and_then(|f| literal(f, code))
                    .ok_or_else(not_literal)?;
                let rest = match list.get(1) {
                    Some(a) if a.kind() == "array" => {
                        literal_list(*a, code).ok_or_else(not_literal)?
                    }
                    Some(a) if a.kind() == "object" => Vec::new(),
                    Some(_) => return Err(not_literal()),
                    None => Vec::new(),
                };
                spawned(Lit::Str(program), &rest, shell)
            }
            "fork" => {
                let module = first
                    .and_then(|f| literal(f, code))
                    .ok_or_else(not_literal)?;
                vec![Embedded::Argv(vec!["node".into(), module])]
            }
            "bun" => {
                let cmd = match first {
                    Some(f) if f.kind() == "object" => property(f, "cmd", code),
                    other => other,
                };
                let argv = cmd
                    .and_then(|c| literal_list(c, code))
                    .ok_or_else(not_literal)?;
                spawned(Lit::List(argv), &[], false)
            }
            "deno-run" => {
                let cmd = first
                    .filter(|f| f.kind() == "object")
                    .and_then(|f| property(f, "cmd", code));
                let argv = cmd
                    .and_then(|c| literal_list(c, code))
                    .ok_or_else(not_literal)?;
                spawned(Lit::List(argv), &[], false)
            }
            "deno-command" => {
                let program = first
                    .and_then(|f| literal(f, code))
                    .ok_or_else(not_literal)?;
                let rest = match options.and_then(|o| property(o, "args", code)) {
                    Some(a) => literal_list(a, code).ok_or_else(not_literal)?,
                    None => Vec::new(),
                };
                spawned(Lit::Str(program), &rest, false)
            }
            _ => Vec::new(),
        };
        Ok(if moved {
            found
                .into_iter()
                .map(|e| Embedded::In(ReadContext::default(), Box::new(e)))
                .collect()
        } else {
            found
        })
    }
}

// -------------------------------------------------------------------- Ruby

mod ruby {
    use super::*;

    /// Methods that run text or reach any method by name.
    const DYNAMIC: &[&str] = &[
        "eval",
        "instance_eval",
        "class_eval",
        "module_eval",
        "instance_exec",
        "class_exec",
        "module_exec",
        "send",
        "__send__",
        "public_send",
        "method",
        "public_method",
        "define_method",
        "const_get",
        "binding",
        "load",
        "autoload",
        "syscall",
        "instance_variable_get",
    ];

    fn literal(node: Node<'_>, code: &str) -> Option<String> {
        match node.kind() {
            "string" => {
                let mut out = String::new();
                for c in named_children(node) {
                    match c.kind() {
                        "string_content" => out.push_str(text(c, code)),
                        "escape_sequence" => out.push_str(&unescape(text(c, code))),
                        _ => return None,
                    }
                }
                Some(out)
            }
            "parenthesized_statements" => {
                named_children(node).first().and_then(|c| literal(*c, code))
            }
            _ => None,
        }
    }

    fn literal_list(node: Node<'_>, code: &str) -> Option<Vec<String>> {
        (node.kind() == "array")
            .then(|| {
                named_children(node)
                    .into_iter()
                    .map(|c| literal(c, code))
                    .collect()
            })
            .flatten()
    }

    /// The spawning method a call names, by its receiver and method.
    fn spawning(receiver: Option<&str>, method: &str) -> Option<&'static str> {
        Some(match (receiver, method) {
            (None | Some("Kernel"), "system" | "exec" | "spawn" | "`") => "kernel",
            (Some("Process"), "spawn" | "exec") => "kernel",
            (Some("IO"), "popen") => "popen",
            (
                Some("Open3"),
                "popen3" | "popen2" | "popen2e" | "capture2" | "capture2e" | "capture3"
                | "pipeline" | "pipeline_r" | "pipeline_w" | "pipeline_rw" | "pipeline_start",
            ) => "kernel",
            (Some("PTY"), "spawn" | "getpty") => "kernel",
            (None | Some("Kernel") | Some("URI") | Some("IO"), "open") => "open",
            // `IO.read("|cmd")` and its kin start cmd (Ruby before 4.0).
            (Some("IO"), "read" | "readlines" | "write" | "foreach" | "binread" | "binwrite") => {
                "io_path"
            }
            _ => return None,
        })
    }

    pub(super) fn scan(code: &str) -> ScanResult {
        let lang: Language = tree_sitter_ruby::LANGUAGE.into();
        let tree = parse(&lang, code)?;
        let mut found = Vec::new();
        let mut imports = Vec::new();
        for n in nodes(tree.root_node()) {
            match n.kind() {
                "subshell" => {
                    let mut script = String::new();
                    for c in named_children(n) {
                        match c.kind() {
                            "string_content" => script.push_str(text(c, code)),
                            "escape_sequence" => script.push_str(&unescape(text(c, code))),
                            _ => return Err(dynamic("a backquoted command with a substitution")),
                        }
                    }
                    found.push(Embedded::Script(script));
                }
                "call" | "identifier" => {
                    let (receiver, method, args) = if n.kind() == "call" {
                        let receiver = n.child_by_field_name("receiver").map(|r| text(r, code));
                        let method = n
                            .child_by_field_name("method")
                            .map(|m| text(m, code))
                            .unwrap_or_default();
                        (receiver, method, n.child_by_field_name("arguments"))
                    } else {
                        // A bare identifier is a call with no arguments when
                        // nothing assigned it.
                        if n.parent().is_some_and(|p| p.kind() == "call") {
                            continue;
                        }
                        (None, text(n, code), None)
                    };
                    if DYNAMIC.contains(&method) {
                        return Err(format!(
                            "reaches code by name or runs text as code (`{method}`)"
                        ));
                    }
                    if matches!(method, "require" | "require_relative") && receiver.is_none() {
                        let target = args.and_then(|a| named_children(a).first().copied());
                        match target.and_then(|t| literal(t, code)) {
                            Some(t) if method == "require_relative" => {
                                imports.push(format!("./{t}"))
                            }
                            Some(t) if names_a_file(&t) => imports.push(t),
                            Some(_) => {}
                            None => {
                                return Err("loads code named by a value kGraf cannot see".into());
                            }
                        }
                        continue;
                    }
                    let Some(kind) = spawning(receiver, method) else {
                        continue;
                    };
                    let list: Vec<Node<'_>> = args
                        .map(named_children)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|c| !matches!(c.kind(), "hash" | "pair" | "comment"))
                        .collect();
                    if list.iter().any(|c| {
                        matches!(
                            c.kind(),
                            "splat_argument" | "hash_splat_argument" | "block_argument"
                        )
                    }) {
                        return Err(dynamic("arguments unpacked from a value"));
                    }
                    let not_literal =
                        || dynamic(&format!("arguments to `{method}` that are not literal"));
                    match kind {
                        "open" => {
                            // `open("|cmd")` starts cmd; any other literal
                            // opens a file.
                            match list.first().and_then(|f| literal(*f, code)) {
                                Some(s) => {
                                    if let Some(cmd) = s.strip_prefix('|') {
                                        found.push(Embedded::Script(cmd.to_owned()));
                                    }
                                }
                                None if receiver.is_none() || receiver == Some("Kernel") => {
                                    return Err(dynamic(
                                        "`open` given a value that may start with `|`",
                                    ));
                                }
                                None => {}
                            }
                        }
                        "io_path" => match list.first().and_then(|f| literal(*f, code)) {
                            Some(s) => {
                                if let Some(cmd) = s.strip_prefix('|') {
                                    found.push(Embedded::Script(cmd.to_owned()));
                                }
                            }
                            None => {
                                return Err(dynamic(&format!(
                                    "`IO.{method}` given a value that may start with `|`"
                                )));
                            }
                        },
                        "popen" => match list.first() {
                            Some(f) => match (literal(*f, code), literal_list(*f, code)) {
                                (Some(s), _) => found.push(Embedded::Script(s)),
                                (None, Some(l)) => found.push(Embedded::Argv(l)),
                                _ => return Err(not_literal()),
                            },
                            None => return Err(not_literal()),
                        },
                        _ => {
                            // A backquote literal is a `subshell`, read
                            // above; the method called by name
                            // (`Kernel.`("cmd")`) runs its argument as a
                            // script.
                            if method == "`" {
                                if receiver.is_none() && args.is_none() {
                                    continue;
                                }
                                match list.first().and_then(|f| literal(*f, code)) {
                                    Some(s) => found.push(Embedded::Script(s)),
                                    None => return Err(not_literal()),
                                }
                                continue;
                            }
                            let Some(first) = list.first() else {
                                return Err(not_literal());
                            };
                            // `system(["prog", "argv0"], ARGS)`.
                            let program = match (literal(*first, code), literal_list(*first, code))
                            {
                                (Some(s), _) => s,
                                (None, Some(l)) if !l.is_empty() => l[0].clone(),
                                _ => return Err(not_literal()),
                            };
                            let rest: Vec<String> = list[1..]
                                .iter()
                                .map(|r| literal(*r, code).ok_or_else(not_literal))
                                .collect::<Result<_, _>>()?;
                            found.extend(spawned(Lit::Str(program), &rest, false));
                        }
                    }
                }
                "symbol" | "simple_symbol" => {
                    let s = text(n, code).trim_start_matches(':');
                    if matches!(s, "system" | "exec" | "spawn" | "popen" | "`") {
                        return Err(dynamic(&format!("the method `{s}` named by a symbol")));
                    }
                }
                _ => {}
            }
        }
        Ok(Scan { found, imports })
    }
}

// --------------------------------------------------------------------- PHP

mod php {
    use super::*;

    /// Functions that start programs.
    fn spawning(name: &str) -> Option<&'static str> {
        Some(match name.to_ascii_lowercase().trim_start_matches('\\') {
            "system" | "exec" | "shell_exec" | "passthru" | "popen" | "expect_popen" => "shell",
            "proc_open" => "proc",
            "pcntl_exec" => "pcntl",
            _ => return None,
        })
    }

    /// Functions that run text as code or call any function by name.
    const DYNAMIC: &[&str] = &[
        "eval",
        "assert",
        "create_function",
        "call_user_func",
        "call_user_func_array",
        "forward_static_call",
        "forward_static_call_array",
        "register_shutdown_function",
        "register_tick_function",
        "array_map",
        "array_walk",
        "array_filter",
        "usort",
        "uasort",
        "uksort",
        "preg_replace_callback",
        "iterator_apply",
        "ffi",
        "dl",
    ];

    const SPAWN_NAMES: &[&str] = &[
        "system",
        "exec",
        "shell_exec",
        "passthru",
        "popen",
        "proc_open",
        "pcntl_exec",
        "expect_popen",
    ];

    fn literal(node: Node<'_>, code: &str) -> Option<String> {
        match node.kind() {
            "string" | "encapsed_string" => {
                let mut out = String::new();
                let mut any = false;
                for c in named_children(node) {
                    match c.kind() {
                        "string_content" | "string_value" => {
                            out.push_str(text(c, code));
                            any = true;
                        }
                        "escape_sequence" => out.push_str(&unescape(text(c, code))),
                        _ => return None,
                    }
                }
                if !any && named_children(node).is_empty() {
                    // `''` or a string whose content the grammar keeps as
                    // its own text.
                    let t = text(node, code);
                    return Some(t.trim_matches(['\'', '"']).to_owned());
                }
                Some(out)
            }
            "argument" => named_children(node).first().and_then(|c| literal(*c, code)),
            "parenthesized_expression" => {
                named_children(node).first().and_then(|c| literal(*c, code))
            }
            _ => None,
        }
    }

    fn literal_list(node: Node<'_>, code: &str) -> Option<Vec<String>> {
        let node = if node.kind() == "argument" {
            named_children(node).first().copied()?
        } else {
            node
        };
        (node.kind() == "array_creation_expression")
            .then(|| {
                named_children(node)
                    .into_iter()
                    .map(|e| {
                        // `array_element_initializer` holds the value.
                        let v = named_children(e).last().copied().unwrap_or(e);
                        literal(v, code)
                    })
                    .collect()
            })
            .flatten()
    }

    pub(super) fn scan(lang: Lang, code: &str) -> ScanResult {
        let language: Language = match lang {
            Lang::PhpOnly => tree_sitter_php::LANGUAGE_PHP_ONLY.into(),
            _ => tree_sitter_php::LANGUAGE_PHP.into(),
        };
        let tree = parse(&language, code)?;
        let mut found = Vec::new();
        let mut imports = Vec::new();
        for n in nodes(tree.root_node()) {
            match n.kind() {
                "shell_command_expression" => {
                    let mut script = String::new();
                    for c in named_children(n) {
                        match c.kind() {
                            "string_content" | "string_value" => script.push_str(text(c, code)),
                            "escape_sequence" => script.push_str(&unescape(text(c, code))),
                            _ => return Err(dynamic("a backquoted command with a variable")),
                        }
                    }
                    if named_children(n).is_empty() {
                        script = text(n, code).trim_matches('`').to_owned();
                    }
                    found.push(Embedded::Script(script));
                }
                "function_call_expression" => {
                    let Some(function) = n.child_by_field_name("function") else {
                        continue;
                    };
                    if !matches!(function.kind(), "name" | "qualified_name") {
                        return Err(dynamic("a function named by a value"));
                    }
                    let name = text(function, code).to_owned();
                    let lower = name.to_ascii_lowercase();
                    let lower = lower.trim_start_matches('\\');
                    if DYNAMIC.contains(&lower) {
                        return Err(format!(
                            "reaches code by name or runs text as code (`{name}`)"
                        ));
                    }
                    let Some(kind) = spawning(&name) else {
                        continue;
                    };
                    let args: Vec<Node<'_>> = n
                        .child_by_field_name("arguments")
                        .map(named_children)
                        .unwrap_or_default();
                    if args.iter().any(|a| text(*a, code).starts_with("...")) {
                        return Err(dynamic("arguments unpacked from a value"));
                    }
                    let not_literal =
                        || dynamic(&format!("arguments to `{name}` that are not literal"));
                    let first = args.first().copied();
                    match kind {
                        "shell" => found.push(Embedded::Script(
                            first
                                .and_then(|f| literal(f, code))
                                .ok_or_else(not_literal)?,
                        )),
                        "proc" => match first {
                            Some(f) => match (literal(f, code), literal_list(f, code)) {
                                (Some(s), _) => found.push(Embedded::Script(s)),
                                (None, Some(l)) => found.push(Embedded::Argv(l)),
                                _ => return Err(not_literal()),
                            },
                            None => return Err(not_literal()),
                        },
                        _ => {
                            let program = first
                                .and_then(|f| literal(f, code))
                                .ok_or_else(not_literal)?;
                            let rest = match args.get(1) {
                                Some(a) => literal_list(*a, code).ok_or_else(not_literal)?,
                                None => Vec::new(),
                            };
                            found.extend(spawned(Lit::Str(program), &rest, false));
                        }
                    }
                }
                "member_call_expression"
                | "scoped_call_expression"
                | "nullsafe_member_call_expression"
                    if n.child_by_field_name("name")
                        .is_some_and(|m| m.kind() != "name") =>
                {
                    return Err(dynamic("a method named by a value"));
                }
                // `FFI::cdef(...)` calls any C function, `system` included.
                "scoped_call_expression"
                    if n.child_by_field_name("scope").is_some_and(|s| {
                        text(s, code)
                            .trim_start_matches('\\')
                            .eq_ignore_ascii_case("ffi")
                    }) =>
                {
                    return Err("calls native code through FFI, which kGraf does not read".into());
                }
                "object_creation_expression" => {
                    let t = text(n, code).to_ascii_lowercase();
                    if t.contains("reflectionfunction")
                        || t.contains("reflectionmethod")
                        || t.contains("ffi")
                    {
                        return Err(dynamic("a function reached by reflection"));
                    }
                }
                "include_expression"
                | "include_once_expression"
                | "require_expression"
                | "require_once_expression" => {
                    let target = named_children(n).first().copied();
                    match target.and_then(|t| literal(t, code)) {
                        Some(t) => imports.push(t),
                        None => {
                            return Err("includes a file named by a value kGraf cannot see".into());
                        }
                    }
                }
                "string" | "encapsed_string" => {
                    if let Some(s) = literal(n, code)
                        && SPAWN_NAMES.contains(&s.to_ascii_lowercase().as_str())
                    {
                        return Err(dynamic(&format!("the function `{s}` named by text")));
                    }
                }
                _ => {}
            }
        }
        Ok(Scan { found, imports })
    }
}

// --------------------------------------------- lexical: Perl, awk, AppleScript

mod lexical {
    use super::*;

    /// A quoted literal at the start of `s` (after spaces): its text and
    /// the rest. `interpolates`: a double-quoted literal that holds `$` or
    /// `@` is not literal. Perl's `q(...)`, `qq(...)`, `qw(...)` forms are
    /// read when `perl` is set.
    pub(super) fn quoted(s: &str, perl: bool) -> Option<(Vec<String>, &str)> {
        let s = s.trim_start();
        let mut chars = s.char_indices();
        let (_, first) = chars.next()?;
        let close_of = |open: char| match open {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            '<' => '>',
            c => c,
        };
        let take = |open: char, body_start: usize, interpolates: bool| -> Option<(String, usize)> {
            let close = close_of(open);
            let mut depth = 0;
            let mut out = String::new();
            let mut it = s[body_start..].char_indices();
            while let Some((k, c)) = it.next() {
                if c == '\\' {
                    if let Some((_, n)) = it.next() {
                        out.push(n);
                    }
                    continue;
                }
                if c == open && open != close {
                    depth += 1;
                } else if c == close {
                    if depth == 0 {
                        if interpolates && (out.contains('$') || out.contains('@')) {
                            return None;
                        }
                        return Some((out, body_start + k + c.len_utf8()));
                    }
                    depth -= 1;
                }
                out.push(c);
            }
            None
        };
        match first {
            '\'' => {
                let (t, end) = take('\'', 1, false)?;
                Some((vec![t], &s[end..]))
            }
            '"' => {
                let (t, end) = take('"', 1, true)?;
                Some((vec![t], &s[end..]))
            }
            'q' if perl => {
                let rest = &s[1..];
                let (kind, after) = if let Some(r) = rest.strip_prefix('q') {
                    ("qq", r)
                } else if let Some(r) = rest.strip_prefix('w') {
                    ("qw", r)
                } else {
                    ("q", rest)
                };
                let after_trim = after.trim_start();
                let open = after_trim.chars().next()?;
                if open.is_alphanumeric()
                    || open == '_'
                    || open == ','
                    || open == ';'
                    || open == '='
                {
                    return None;
                }
                let body_start = s.len() - after_trim.len() + open.len_utf8();
                let (t, end) = take(open, body_start, kind == "qq")?;
                let words = if kind == "qw" {
                    t.split_whitespace().map(str::to_owned).collect()
                } else {
                    vec![t]
                };
                Some((words, &s[end..]))
            }
            _ => None,
        }
    }

    /// A list of literals at the start of `s`, in parentheses or not,
    /// separated by `,` or `=>`, ending at `)`, `;`, a newline, `}` or the
    /// end. `None` when anything in it is not a literal.
    pub(super) fn literal_list(s: &str, perl: bool) -> Option<Vec<String>> {
        let mut rest = s.trim_start();
        let paren = rest.starts_with('(');
        if paren {
            rest = &rest[1..];
        }
        let mut out = Vec::new();
        loop {
            let (words, after) = quoted(rest, perl)?;
            out.extend(words);
            let after = after.trim_start();
            if let Some(a) = after.strip_prefix(',').or_else(|| after.strip_prefix("=>")) {
                rest = a;
                continue;
            }
            let ends = if paren {
                after.starts_with(')')
            } else {
                after.is_empty()
                    || after.starts_with(';')
                    || after.starts_with('}')
                    || after.starts_with('\n')
                    || after.starts_with("or ")
                    || after.starts_with("and ")
                    || after.starts_with("||")
                    || after.starts_with("&&")
                    || after.starts_with("if ")
                    || after.starts_with("unless ")
            };
            return ends.then_some(out);
        }
    }

    /// Every position of `word` in `code` as a whole word.
    pub(super) fn word_at(code: &str, word: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let bytes = code.as_bytes();
        let mut from = 0;
        while let Some(p) = code[from..].find(word) {
            let at = from + p;
            let before = at.checked_sub(1).map(|b| bytes[b]);
            let after = bytes.get(at + word.len()).copied();
            let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
            if !before.is_some_and(|b| {
                ident(b) || b == b'$' || b == b'@' || b == b'%' || b == b'-' || b == b'>'
            }) && !after.is_some_and(ident)
            {
                out.push(at);
            }
            from = at + word.len();
        }
        out
    }

    pub(super) fn perl(code: &str) -> ScanResult {
        let mut found = Vec::new();
        for w in ["system", "exec"] {
            for at in word_at(code, w) {
                let rest = &code[at + w.len()..];
                let trimmed = rest.trim_start();
                if trimmed.starts_with('{') || trimmed.starts_with("$") {
                    return Err(dynamic(&format!("`{w}` given a program by a value")));
                }
                let list = literal_list(rest, true)
                    .ok_or_else(|| dynamic(&format!("arguments to `{w}` that are not literal")))?;
                match list.split_first() {
                    Some((only, [])) => found.push(Embedded::Script(only.clone())),
                    Some(_) => found.push(Embedded::Argv(list)),
                    None => {}
                }
            }
        }
        for at in word_at(code, "readpipe") {
            let list = literal_list(&code[at + "readpipe".len()..], true)
                .ok_or_else(|| dynamic("arguments to `readpipe` that are not literal"))?;
            found.extend(list.into_iter().map(Embedded::Script));
        }
        // `qx/CMD/`, `qx'CMD'` (never interpolated) and backquotes.
        for at in word_at(code, "qx") {
            let rest = code[at + 2..].trim_start();
            let open = rest.chars().next().unwrap_or(' ');
            if open.is_alphanumeric() || open == '_' || open == '=' || open == ',' || open == ';' {
                continue;
            }
            let as_q = format!("q{rest}");
            let (words, _) =
                quoted(&as_q, true).ok_or_else(|| dynamic("a `qx` command kGraf cannot read"))?;
            let cmd = words.join(" ");
            if open != '\'' && (cmd.contains('$') || cmd.contains('@')) {
                return Err(dynamic("a `qx` command with a variable"));
            }
            found.push(Embedded::Script(cmd));
        }
        let mut rest = code;
        while let Some(p) = rest.find('`') {
            let after = &rest[p + 1..];
            let Some(end) = after.find('`') else {
                return Err(dynamic("a backquoted command kGraf cannot read"));
            };
            let cmd = &after[..end];
            if cmd.contains('$') || cmd.contains('@') {
                return Err(dynamic("a backquoted command with a variable"));
            }
            found.push(Embedded::Script(cmd.to_owned()));
            rest = &after[end + 1..];
        }
        // `open(FH, "-|", "cmd", ARGS)`, `open(FH, "cmd |")`, `open(FH,
        // "| cmd")`.
        for at in word_at(code, "open") {
            let rest = code[at + 4..].trim_start();
            // `open(F, ...)`, or `open F, ...;` without parentheses.
            let list = match rest.strip_prefix('(') {
                Some(inner) => match inner.find(')') {
                    Some(close) => &inner[..close],
                    None => return Err(dynamic("an `open` kGraf cannot read")),
                },
                None => {
                    let end = [";", "\n", " or ", "||"]
                        .iter()
                        .filter_map(|t| rest.find(t))
                        .min()
                        .unwrap_or(rest.len());
                    &rest[..end]
                }
            };
            let args: Vec<&str> = list.split(',').map(str::trim).collect();
            let lit = |a: &str| {
                quoted(a, true)
                    .filter(|(_, r)| r.trim().is_empty())
                    .map(|(w, _)| w.join(" "))
            };
            if let [_, mode, rest @ ..] = args.as_slice() {
                match lit(mode) {
                    Some(m) if m == "-|" || m == "|-" => {
                        let words: Option<Vec<String>> = rest.iter().map(|a| lit(a)).collect();
                        let words = words
                            .ok_or_else(|| dynamic("arguments to `open` that are not literal"))?;
                        match words.split_first() {
                            Some((only, [])) => found.push(Embedded::Script(only.clone())),
                            Some(_) => found.push(Embedded::Argv(words)),
                            None => {}
                        }
                    }
                    Some(m) if rest.is_empty() => {
                        let t = m.trim();
                        if let Some(cmd) = t.strip_prefix('|').or_else(|| t.strip_suffix('|')) {
                            found.push(Embedded::Script(cmd.trim().to_owned()));
                        }
                    }
                    Some(_) => {}
                    None if rest.is_empty() => {
                        return Err(dynamic("a two-argument `open` given a value"));
                    }
                    None => return Err(dynamic("an `open` mode given by a value")),
                }
            }
        }
        // Text run as code, files run as code, code reached by name, and
        // `s///e`.
        for at in word_at(code, "eval") {
            if !code[at + 4..].trim_start().starts_with('{') {
                return Err("runs text as code (`eval`)".into());
            }
        }
        for at in word_at(code, "do") {
            let next = code[at + 2..].trim_start();
            if next.starts_with('"') || next.starts_with('\'') || next.starts_with('$') {
                return Err("runs a file as code (`do FILE`)".into());
            }
        }
        for at in word_at(code, "require") {
            let next = code[at + 7..].trim_start();
            if next.starts_with('"') || next.starts_with('\'') || next.starts_with('$') {
                return Err("runs a file as code (`require FILE`)".into());
            }
        }
        for module in PERL_OPAQUE_MODULES {
            if code.contains(module) {
                return Err(format!(
                    "uses `{module}`, which starts programs kGraf cannot read"
                ));
            }
        }
        for marker in ["&$", "&{", "->$", "*{", "CORE::GLOBAL", "->can("] {
            if code.contains(marker) {
                return Err(dynamic(&format!("code reached by a value (`{marker}`)")));
            }
        }
        if substitution_with_e(code) {
            return Err("runs a substitution's replacement as code (`s///e`)".into());
        }
        Ok(Scan {
            found,
            imports: Vec::new(),
        })
    }

    /// Whether `code` holds an `s///` substitution with the `e` flag.
    pub(super) fn substitution_with_e(code: &str) -> bool {
        for at in word_at(code, "s") {
            let rest = &code[at + 1..];
            let Some(open) = rest.chars().next() else {
                continue;
            };
            if open.is_alphanumeric()
                || open.is_whitespace()
                || matches!(open, '_' | ',' | ';' | ')' | '=' | '>')
            {
                continue;
            }
            let close = match open {
                '(' => ')',
                '[' => ']',
                '{' => '}',
                '<' => '>',
                c => c,
            };
            // Skip the pattern and the replacement (a bracketed pair may
            // have a space and its own brackets between).
            let mut parts = 0;
            let mut it = rest.char_indices().skip(1);
            let mut end = None;
            let mut escaped = false;
            let mut depth = 0;
            for (k, c) in it.by_ref() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if open != close && c == open {
                    depth += 1;
                    continue;
                }
                if c == close {
                    if depth > 0 {
                        depth -= 1;
                        continue;
                    }
                    parts += 1;
                    let needed = if open == close { 2 } else { 1 };
                    if parts == needed {
                        if open == close {
                            end = Some(k + 1);
                            break;
                        }
                        // A bracketed replacement follows: `s{a}{b}e`.
                        let after = rest[k + 1..].trim_start();
                        let Some(o2) = after.chars().next() else {
                            break;
                        };
                        let c2 = match o2 {
                            '(' => ')',
                            '[' => ']',
                            '{' => '}',
                            '<' => '>',
                            c => c,
                        };
                        let base = rest.len() - after.len();
                        // The replacement ends at its own closing bracket:
                        // brackets inside it nest, and a backslash escapes.
                        let mut nested = 0;
                        let mut escaped2 = false;
                        for (j, ch) in after[o2.len_utf8()..].char_indices() {
                            if escaped2 {
                                escaped2 = false;
                            } else if ch == '\\' {
                                escaped2 = true;
                            } else if o2 != c2 && ch == o2 {
                                nested += 1;
                            } else if ch == c2 {
                                if nested > 0 {
                                    nested -= 1;
                                } else {
                                    end = Some(base + o2.len_utf8() + j + ch.len_utf8());
                                    break;
                                }
                            }
                        }
                        break;
                    }
                }
            }
            if let Some(e) = end {
                let flags: String = rest[e..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .collect();
                if flags.contains('e') {
                    return true;
                }
            }
        }
        false
    }

    /// An awk program's tokens of interest: string literals, `|` operators
    /// and the rest, with regular expressions and comments skipped.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum AwkToken {
        Str(String),
        Pipe,
        Word(String),
        Other(char),
    }

    pub(super) fn awk_tokens(code: &str) -> Vec<AwkToken> {
        let chars: Vec<char> = code.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        let operand_before = |out: &Vec<AwkToken>| {
            matches!(
                out.last(),
                Some(AwkToken::Str(_))
                    | Some(AwkToken::Word(_))
                    | Some(AwkToken::Other(')' | ']' | '$'))
            )
        };
        while i < chars.len() {
            let c = chars[i];
            match c {
                '"' => {
                    let mut s = String::new();
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        if chars[i] == '\\' && i + 1 < chars.len() {
                            s.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        s.push(chars[i]);
                        i += 1;
                    }
                    out.push(AwkToken::Str(s));
                    i += 1;
                }
                '#' => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
                '/' if !operand_before(&out) => {
                    // A regular expression literal.
                    i += 1;
                    while i < chars.len() && chars[i] != '/' {
                        if chars[i] == '\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    out.push(AwkToken::Word("REGEX".into()));
                    i += 1;
                }
                '|' if chars.get(i + 1) == Some(&'|') => {
                    out.push(AwkToken::Other('|'));
                    out.push(AwkToken::Other('|'));
                    i += 2;
                }
                '|' => {
                    out.push(AwkToken::Pipe);
                    // `|&` (gawk's coprocess) reads like `|`.
                    i += if chars.get(i + 1) == Some(&'&') { 2 } else { 1 };
                }
                _ if c.is_alphanumeric() || c == '_' => {
                    let mut w = String::new();
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        w.push(chars[i]);
                        i += 1;
                    }
                    out.push(AwkToken::Word(w));
                }
                _ if c.is_whitespace() => i += 1,
                _ => {
                    out.push(AwkToken::Other(c));
                    i += 1;
                }
            }
        }
        out
    }

    pub(super) fn awk(code: &str) -> ScanResult {
        if code.contains("@load") || code.contains("@include") {
            return Err("loads an extension or a library kGraf does not read".into());
        }
        let tokens = awk_tokens(code);
        let mut found = Vec::new();
        for (k, t) in tokens.iter().enumerate() {
            match t {
                AwkToken::Word(w) if w == "system" => {
                    // `system("cmd")`: a lone literal in parentheses.
                    match (tokens.get(k + 1), tokens.get(k + 2), tokens.get(k + 3)) {
                        (
                            Some(AwkToken::Other('(')),
                            Some(AwkToken::Str(s)),
                            Some(AwkToken::Other(')')),
                        ) => {
                            found.push(Embedded::Script(s.clone()));
                        }
                        _ => return Err(dynamic("`system` given a value that is not literal")),
                    }
                }
                AwkToken::Pipe => {
                    // `"cmd" | getline` reads the literal before; `print |
                    // "cmd"` writes to the literal after.
                    match (
                        k.checked_sub(1).and_then(|p| tokens.get(p)),
                        tokens.get(k + 1),
                    ) {
                        (Some(AwkToken::Str(s)), Some(AwkToken::Word(w))) if w == "getline" => {
                            found.push(Embedded::Script(s.clone()));
                        }
                        (_, Some(AwkToken::Str(s)))
                            if !matches!(tokens.get(k + 2), Some(AwkToken::Other(c)) if "+-*/%^".contains(*c))
                                && !matches!(tokens.get(k + 2), Some(AwkToken::Str(_))) =>
                        {
                            found.push(Embedded::Script(s.clone()));
                        }
                        _ => {
                            return Err(dynamic("a pipe to or from a command that is not literal"));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(Scan {
            found,
            imports: Vec::new(),
        })
    }

    pub(super) fn applescript(code: &str) -> ScanResult {
        let lower = code.to_ascii_lowercase();
        for marker in [
            "run script",
            "load script",
            "store script",
            "keystroke",
            "key code",
            "\u{ab}event",
            "$.nstask",
            "objc.import",
            "objc.",
            "eval(",
        ] {
            if lower.contains(marker) {
                return Err(format!("uses `{marker}`, whose effect kGraf cannot read"));
            }
        }
        let mut found = Vec::new();
        for phrase in ["do shell script", "do script", "doshellscript("] {
            let mut from = 0;
            while let Some(p) = lower[from..].find(phrase) {
                let at = from + p + phrase.len();
                let rest = code[at..].trim_start();
                let Some((words, after)) = quoted(rest, false) else {
                    return Err(dynamic(&format!(
                        "`{phrase}` given a value that is not literal"
                    )));
                };
                let after = after.trim_start();
                if after.starts_with('&') || after.starts_with('+') {
                    return Err(dynamic(&format!("`{phrase}` given text joined to a value")));
                }
                found.push(Embedded::Script(words.join(" ")));
                from = at;
            }
        }
        Ok(Scan {
            found,
            imports: Vec::new(),
        })
    }
}

#[cfg(test)]
#[path = "permission_code_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "permission_code_more_tests.rs"]
mod more_tests;

#[cfg(test)]
#[path = "permission_code_scan_tests.rs"]
mod scan_tests;
