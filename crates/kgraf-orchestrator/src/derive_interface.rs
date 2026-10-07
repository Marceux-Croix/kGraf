//! A compile-only interface derived from the repository, before any planner
//! call (decision 0030, amendments of 2026-10-02 and 2026-10-06).
//!
//! The stub is what the producer's files hold at the dispatch base
//! ([`derived_stub`]):
//!
//! - **A scaffold** (a file with a placeholder body such as `todo!()` or
//!   `raise NotImplementedError`): the scaffold is the stub.
//! - **Current code** (no file the producer writes holds a scaffold, and at
//!   least one exists): the current files are the stub. Their shapes (the
//!   signatures and types the consumer files use) are the interface.
//!
//! The repository files that use the producer's items are the consumer and
//! test files, and the project's own build tool compiles them: stub-compile
//! and its canaries run. The result is registered as an interface of kind
//! compile-only. When the producer's attempt passes its own verification, the
//! same compile command runs again over the producer's delivered files
//! ([`crate::checks::check_delivery`]): a delivery that breaks a shape the
//! consumer files use fails the attempt, and a change they do not use does
//! not. Nothing correct exists to pass a test, so the behavior check is not
//! part of it: whether the planner is asked for one is [`behavior_plan`]'s
//! decision.
//!
//! What the compile command checks depends on the language ([`coverage`]):
//!
//! | Language | Command | What it checks |
//! | --- | --- | --- |
//! | Rust | `cargo test --no-run -p <consumer packages>` | every item, signature and type the consumer files use |
//! | Python | `python3 -c "import <consumer modules>"` (`python` on Windows) | the names the consumer modules import when they load; call shapes and attribute uses are not checked |
//! | TypeScript | `node <project tsc> --noEmit -p <dir>` | every file the tsconfig.json includes; only with the project's own compiler in the repository |
//! | Go | `go vet <packages>` | each package of the consumer files, with its test files |
//! | Java | `javac -d <scratch> -sourcepath <roots> <files>` | the consumer files and the sources they use; only with no Maven, Gradle or Ant build |
//! | C# | `dotnet build --no-restore <project>` | the project of the consumer files; only with its dependencies restored |
//! | any other | none | no compile check is derived; the edge is recorded unchecked with the reason ([`no_command_reason`]) |
//!
//! When a project or machine does not allow the check (no compiler, a build
//! tool a bare compiler cannot stand in for, dependencies not restored), the
//! edge is recorded unchecked with that reason ([`command_for`]). A derived
//! command that fails at the base before dispatch is not registered, so a
//! check that cannot apply never blocks a correct project.
//!
//! The decisions here are pure (what is a scaffold, a test file, which
//! command builds the files, whether a test of one producer alone can exist);
//! the orchestrator gathers the facts from the graph and the dispatch base.

use crate::checks::{FileOverlay, InterfaceContract, InterfaceKind};
use std::collections::{BTreeMap, BTreeSet};

/// Whether `content`, the text of `path`, is a scaffold: it holds a
/// placeholder body in code. The words in a string literal or a comment do
/// not make a file a scaffold ([`crate::placeholder::has_placeholder_body`]).
pub fn is_scaffold(path: &str, content: &str) -> bool {
    crate::placeholder::has_placeholder_body(path, content)
}

/// Whether `path` names a test file by the common conventions: under a
/// `tests`, `test`, `Tests`, `__tests__` or `spec` directory or a .NET test
/// project directory (`App.Tests`, `App.Test`), or named `test_*`, `*_test.*`,
/// `*.test.*`, `*.spec.*`, `*_spec.*`, or `*Test.*` / `*Tests.*` (the Java and
/// C# class conventions: `PriceTest.java`, `PriceTests.cs`; the capital `T`
/// after a lower-case letter or digit, so `Contest.java` is not one).
pub fn is_test_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let Some((name, dirs)) = parts.split_last() else {
        return false;
    };
    let stem = name.split_once('.').map_or(*name, |(s, _)| s);
    let class_test = ["Tests", "Test"].iter().any(|suffix| {
        stem.strip_suffix(suffix).is_some_and(|head| {
            head.chars()
                .last()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
    });
    dirs.iter().any(|d| {
        matches!(*d, "tests" | "test" | "Tests" | "__tests__" | "spec")
            || d.ends_with(".Tests")
            || d.ends_with(".Test")
    }) || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains("_spec.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || class_test
}

/// Whether `text` uses `name` as a whole word.
pub fn uses_word(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut from = 0;
    while let Some(at) = text[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let before = start == 0 || !word(bytes[start - 1]);
        let after = end >= bytes.len() || !word(bytes[end]);
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

/// Names `content` declares, read by keyword: the fallback when the graph
/// holds no node for a file (not indexed yet, or a language it does not
/// parse). A token scan, not a parser: `def`, `class`, `fn`, `struct`,
/// `enum`, `trait`, `type`, `function`, `interface`, `record`, then an
/// identifier; and Go's `func`, with a method's receiver skipped.
pub fn declared_names(content: &str) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "def",
        "class",
        "fn",
        "struct",
        "enum",
        "trait",
        "type",
        "function",
        "interface",
        "record",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut words = content
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .peekable();
    while let Some(w) = words.next() {
        if KEYWORDS.contains(&w)
            && let Some(name) = words.peek()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && !KEYWORDS.contains(name)
            && !out.iter().any(|o| o == name)
        {
            out.push((*name).to_owned());
        }
    }
    // Go: `func Name(` and `func (r *Recv) Name(`. Read from the text, not
    // the word stream, so a method's receiver name is not taken for its name.
    let bytes = content.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    for (at, _) in content.match_indices("func") {
        let end = at + 4;
        if (at > 0 && ident(bytes[at - 1])) || bytes.get(end).is_some_and(|b| ident(*b)) {
            continue;
        }
        let mut rest = content[end..].trim_start();
        if let Some(after) = rest.strip_prefix('(') {
            match after.find(')') {
                Some(close) => rest = after[close + 1..].trim_start(),
                None => continue,
            }
        }
        let name: String = rest
            .chars()
            .take_while(|c| ident(*c as u8) && c.is_ascii())
            .collect();
        if !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    out
}

/// The package name a `Cargo.toml` declares under `[package]`.
pub fn cargo_package(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("name")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            let v = value.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// The directories from `path`'s own up to the repository root (`""`).
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut dir = path.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    loop {
        out.push(dir.clone());
        if dir.is_empty() {
            break;
        }
        dir = dir.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    }
    out
}

/// The Python module a repository-relative `.py` path imports as, when every
/// part is an identifier.
pub fn python_module(path: &str) -> Option<String> {
    let stem = path.strip_suffix(".py")?;
    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
    let parts: Vec<&str> = stem.split('/').collect();
    let ident = |p: &str| {
        !p.is_empty()
            && !p.starts_with(|c: char| c.is_ascii_digit())
            && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    parts.iter().all(|p| ident(p)).then(|| parts.join("."))
}

/// The command that builds `files` with the project's own tool: for Rust
/// files, `cargo test --no-run` over the packages that hold them (`package`
/// gives a file's package); for Python files, an import of each module, which
/// resolves every name the module imports from the producer. `None` for any
/// other language, or a file whose package or module cannot be told.
pub fn compile_command(
    files: &[String],
    package: impl Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    if files.is_empty() {
        return None;
    }
    if files.iter().all(|f| f.ends_with(".rs")) {
        let mut packages = BTreeSet::new();
        for f in files {
            packages.insert(package(f)?);
        }
        let mut argv = vec!["cargo".to_owned(), "test".to_owned(), "--no-run".to_owned()];
        for p in packages {
            argv.push("-p".to_owned());
            argv.push(p);
        }
        return Some(argv);
    }
    if files.iter().all(|f| f.ends_with(".py")) {
        let mut imports = Vec::new();
        for f in files {
            imports.push(format!("import {}", python_module(f)?));
        }
        return Some(vec![
            kgraf_sandbox::python_program().to_owned(),
            "-c".to_owned(),
            imports.join("; "),
        ]);
    }
    None
}

/// The lower-case extension of `path`'s file name; empty when it has none.
fn extension(path: &str) -> String {
    let name = path.rsplit_once('/').map_or(path, |(_, n)| n);
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Whether `path` is in a language a compile command can be derived for:
/// Rust and Python ([`compile_command`]), and TypeScript, Go, Java and C#
/// when the project and the machine allow it ([`command_for`]).
pub fn has_compile_language(path: &str) -> bool {
    matches!(
        extension(path).as_str(),
        "rs" | "py" | "ts" | "tsx" | "mts" | "cts" | "go" | "java" | "cs"
    )
}

/// What the dispatch base and the machine hold that a compile command of
/// TypeScript, Go, Java or C# needs ([`command_for`]). Gathered by the
/// orchestrator for the files at hand ([`config_probes`]); the decisions over
/// them are pure.
#[derive(Clone, Debug, Default)]
pub struct BuildFacts {
    /// Files at the base, by repository-relative path, that were looked for:
    /// `true` when the base holds one.
    pub present: BTreeMap<String, bool>,
    /// The file names in each directory looked at (`""` is the root).
    pub listings: BTreeMap<String, Vec<String>>,
    /// The text of each consumer or test file that was read (a Java file's
    /// `package` line gives its source root).
    pub texts: BTreeMap<String, String>,
    /// Programs found on the `PATH` among those looked for.
    pub programs: BTreeSet<String>,
}

impl BuildFacts {
    fn has(&self, path: &str) -> bool {
        self.present.get(path).copied().unwrap_or(false)
    }
}

/// `dir` joined with `name` as a repository-relative path.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The files of the TypeScript language family a type check builds.
fn is_typescript(path: &str) -> bool {
    matches!(extension(path).as_str(), "ts" | "tsx" | "mts" | "cts")
}

/// Build-tool files whose presence means a Java project builds with Maven,
/// Gradle or Ant, so a bare `javac` would miss its dependencies and settings.
const JAVA_BUILD_FILES: [&str; 6] = [
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "build.xml",
];

/// The probes [`command_for`] reads for `files`: every file it may look for at
/// the base, every directory it may list, and every program it may run. Empty
/// for Rust and Python, which need none.
pub fn config_probes(files: &[String]) -> (Vec<String>, Vec<String>, Vec<&'static str>) {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut programs: Vec<&'static str> = Vec::new();
    for f in files {
        let up = ancestors(f);
        match extension(f).as_str() {
            "ts" | "tsx" | "mts" | "cts" => {
                for d in &up {
                    paths.insert(join(d, "tsconfig.json"));
                    paths.insert(join(d, "node_modules/typescript/bin/tsc"));
                }
                programs.push("node");
            }
            "go" => {
                for d in &up {
                    paths.insert(join(d, "go.mod"));
                }
                paths.insert("go.work".to_owned());
                programs.push("go");
            }
            "java" => {
                for d in &up {
                    for b in JAVA_BUILD_FILES {
                        paths.insert(join(d, b));
                    }
                }
                programs.push("javac");
            }
            "cs" => {
                for d in &up {
                    dirs.insert(d.clone());
                    paths.insert(join(d, "obj/project.assets.json"));
                }
                programs.push("dotnet");
            }
            _ => {}
        }
    }
    programs.sort_unstable();
    programs.dedup();
    (
        paths.into_iter().collect(),
        dirs.into_iter().collect(),
        programs,
    )
}

/// The compile command for `files`, or why there is none: Rust and Python as
/// [`compile_command`] gives them, then a compile-only check for TypeScript,
/// Go, Java and C# where a cheap, offline check exists and the project and the
/// machine allow it. Every case that cannot be checked is an `Err` with the
/// reason, recorded with the edge as unchecked; none is a block.
///
/// | Language | Command | Only when |
/// | --- | --- | --- |
/// | TypeScript | `node <tsc> --noEmit -p <dir>` | a `tsconfig.json` above the files, the project's own compiler at `node_modules/typescript` at the base, and `node` on the `PATH` |
/// | Go | `go vet <packages>` (with `-C <module>` when the module is not at the root) | one `go.mod` above every file, and `go` on the `PATH` |
/// | Java | `javac -d <scratch> -sourcepath <roots> <files>` | no Maven, Gradle or Ant file above any file, a `package` line that names each file's source root, and `javac` on the `PATH` |
/// | C# | `dotnet build --no-restore <project>` | one `.csproj` above every file, its `obj/project.assets.json` at the base (dependencies restored), and `dotnet` on the `PATH` |
pub fn command_for(
    files: &[String],
    package: impl Fn(&str) -> Option<String>,
    build: &BuildFacts,
) -> Result<Vec<String>, String> {
    if let Some(c) = compile_command(files, &package) {
        return Ok(c);
    }
    if files.is_empty() {
        return Err("no consumer or test file to build".to_owned());
    }
    if files.iter().all(|f| is_typescript(f)) {
        return typescript_command(files, build);
    }
    let all = |ext: &str| files.iter().all(|f| extension(f) == ext);
    if all("go") {
        return go_command(files, build);
    }
    if all("java") {
        return java_command(files, build);
    }
    if all("cs") {
        return csharp_command(files, build);
    }
    Err(no_command_reason(files, package))
}

/// The one directory, among the ancestors of every file, that holds `marker`
/// nearest to that file; `Err` names the files when they have none or more
/// than one.
fn nearest_shared(
    files: &[String],
    holds: impl Fn(&str) -> bool,
    what: &str,
) -> Result<String, String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    for f in files {
        match ancestors(f).into_iter().find(|d| holds(d)) {
            Some(d) => {
                found.insert(d);
            }
            None => return Err(format!("no {what} above `{f}`")),
        }
    }
    if found.len() > 1 {
        let list: Vec<String> = found.into_iter().map(|d| format!("`{d}/`")).collect();
        return Err(format!(
            "the files belong to more than one {what} ({}), and one command checks one",
            list.join(", ")
        ));
    }
    Ok(found.into_iter().next().unwrap_or_default())
}

fn typescript_command(files: &[String], build: &BuildFacts) -> Result<Vec<String>, String> {
    let list = files.join(", ");
    let unchecked = |why: String| Err(format!("TypeScript ({list}): {why}, so no type check runs"));
    let project = match nearest_shared(
        files,
        |d| build.has(&join(d, "tsconfig.json")),
        "tsconfig.json",
    ) {
        Ok(p) => p,
        Err(why) => return unchecked(why),
    };
    // Node resolves a package from the project directory upward; the
    // compiler must be the project's own, so its settings and version apply.
    let Some(tsc) = ancestors(&join(&project, "tsconfig.json"))
        .into_iter()
        .map(|d| join(&d, "node_modules/typescript/bin/tsc"))
        .find(|p| build.has(p))
    else {
        return unchecked(
            "the repository holds no TypeScript compiler of its own (`node_modules/typescript`), \
             and a check uses only what the repository holds"
                .to_owned(),
        );
    };
    if !build.programs.contains("node") {
        return unchecked("`node` is not installed".to_owned());
    }
    let dir = if project.is_empty() {
        "."
    } else {
        project.as_str()
    };
    Ok(vec![
        "node".to_owned(),
        tsc,
        "--noEmit".to_owned(),
        "-p".to_owned(),
        dir.to_owned(),
    ])
}

fn go_command(files: &[String], build: &BuildFacts) -> Result<Vec<String>, String> {
    let list = files.join(", ");
    let unchecked = |why: String| Err(format!("Go ({list}): {why}, so no compile check runs"));
    let module = match nearest_shared(
        files,
        |d| build.has(&join(d, "go.mod")),
        "Go module (go.mod)",
    ) {
        Ok(m) => m,
        Err(why) => return unchecked(why),
    };
    if !build.programs.contains("go") {
        return unchecked("`go` is not installed".to_owned());
    }
    // `go vet` type-checks each package with its test files, which `go build`
    // leaves out; a vet finding the base already has makes the check fail
    // before dispatch, so it is not registered and blocks nothing.
    let mut packages: BTreeSet<String> = BTreeSet::new();
    for f in files {
        let dir = f.rsplit_once('/').map_or("", |(d, _)| d);
        let rel = if module.is_empty() {
            dir
        } else {
            dir.strip_prefix(module.as_str())
                .map_or(dir, |r| r.trim_start_matches('/'))
        };
        packages.insert(if rel.is_empty() {
            ".".to_owned()
        } else {
            format!("./{rel}")
        });
    }
    let mut argv = vec!["go".to_owned()];
    if !module.is_empty() {
        argv.push("-C".to_owned());
        argv.push(module);
    }
    argv.push("vet".to_owned());
    argv.extend(packages);
    Ok(argv)
}

/// The source root of a Java file: its directory less the path of the
/// package its `package` line names. `None` when the line and the directory
/// do not agree, or the text is not known.
fn java_source_root(path: &str, text: Option<&str>) -> Option<String> {
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let package = text?
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("package "))
        .map(|p| p.trim_end_matches(';').trim().replace('.', "/"));
    match package {
        None => Some(dir.to_owned()),
        Some(p) if dir == p => Some(String::new()),
        Some(p) => dir.strip_suffix(&format!("/{p}")).map(str::to_owned),
    }
}

/// Where `javac` writes its class files in the check tree, which is thrown
/// away after the run.
const JAVAC_OUT: &str = ".kgraf-javac-out";

fn java_command(files: &[String], build: &BuildFacts) -> Result<Vec<String>, String> {
    let list = files.join(", ");
    let unchecked = |why: String| Err(format!("Java ({list}): {why}, so no compile check runs"));
    for f in files {
        for d in ancestors(f) {
            if let Some(b) = JAVA_BUILD_FILES.iter().find(|b| build.has(&join(&d, b))) {
                return unchecked(format!(
                    "the project builds with `{}`, whose dependencies and settings a bare `javac` \
                     does not see",
                    join(&d, b)
                ));
            }
        }
    }
    if !build.programs.contains("javac") {
        return unchecked("`javac` is not installed".to_owned());
    }
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for f in files {
        match java_source_root(f, build.texts.get(f).map(String::as_str)) {
            Some(r) => {
                roots.insert(if r.is_empty() { ".".to_owned() } else { r });
            }
            None => {
                return unchecked(format!(
                    "the `package` line of `{f}` does not match its directory"
                ));
            }
        }
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut argv = vec![
        "javac".to_owned(),
        "-proc:none".to_owned(),
        "-d".to_owned(),
        JAVAC_OUT.to_owned(),
        "-sourcepath".to_owned(),
        roots.into_iter().collect::<Vec<_>>().join(sep),
    ];
    argv.extend(files.iter().cloned());
    Ok(argv)
}

fn csharp_command(files: &[String], build: &BuildFacts) -> Result<Vec<String>, String> {
    let list = files.join(", ");
    let unchecked = |why: String| Err(format!("C# ({list}): {why}, so no compile check runs"));
    let projects = |d: &str| -> Vec<String> {
        build
            .listings
            .get(d)
            .map(|names| {
                names
                    .iter()
                    .filter(|n| n.ends_with(".csproj"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };
    let dir = match nearest_shared(files, |d| !projects(d).is_empty(), "C# project (.csproj)") {
        Ok(d) => d,
        Err(why) => return unchecked(why),
    };
    let found = projects(&dir);
    let [project] = found.as_slice() else {
        return unchecked(format!(
            "`{}` holds more than one project file",
            if dir.is_empty() { "." } else { dir.as_str() }
        ));
    };
    if !build.has(&join(&dir, "obj/project.assets.json")) {
        return unchecked(format!(
            "the dependencies of `{}` are not restored in the repository \
             (`obj/project.assets.json`), and a check cannot download them",
            join(&dir, project)
        ));
    }
    if !build.programs.contains("dotnet") {
        return unchecked("`dotnet` is not installed".to_owned());
    }
    Ok(vec![
        "dotnet".to_owned(),
        "build".to_owned(),
        "--no-restore".to_owned(),
        join(&dir, project),
    ])
}

/// Extensions of prose files, which no compile command builds and no
/// language reason names.
const PROSE: [&str; 5] = ["md", "markdown", "txt", "rst", "adoc"];

/// Why [`compile_command`] gives no command for `files`, per language: the
/// reason an edge with no derived compile check is recorded unchecked.
/// `package` gives a Rust file's Cargo package.
pub fn no_command_reason(files: &[String], package: impl Fn(&str) -> Option<String>) -> String {
    let exts: BTreeSet<String> = files.iter().map(|f| extension(f)).collect();
    let list = files.join(", ");
    if exts.len() > 1 {
        return format!(
            "{list} are in more than one language, and kGraf derives a compile command for one \
             language at a time"
        );
    }
    match exts.iter().next().map(String::as_str).unwrap_or_default() {
        "rs" => match files.iter().find(|f| package(f).is_none()) {
            Some(f) => format!("no Cargo package holds `{f}`, so no cargo command builds it"),
            None => format!("kGraf knows no build command for {list} in this project"),
        },
        "py" => match files.iter().find(|f| python_module(f).is_none()) {
            Some(f) => format!(
                "`{f}` is not importable as a Python module (each part of its path must be an \
                 identifier), so no import resolves the names it uses"
            ),
            None => format!("kGraf knows no build command for {list} in this project"),
        },
        "sh" | "bash" | "zsh" | "ksh" => format!(
            "{list}: shell scripts; shell has no compile step that resolves the commands or \
             functions a script takes from another file, so no compile check applies"
        ),
        "js" | "jsx" | "mjs" | "cjs" => format!(
            "kGraf derives no JavaScript compile check ({list}): JavaScript has no compile step \
             that resolves the names a file takes from another, so no compile check applies"
        ),
        "" => format!("{list}: no file extension that names a language"),
        other => format!("kGraf derives no compile check for `.{other}` files ({list})"),
    }
}

/// What the compile command `compile` checks, in one clause, for the record
/// of a derived interface.
pub fn coverage(compile: &[String]) -> &'static str {
    match compile.first().map(String::as_str) {
        Some("cargo") => "Rust: every item, signature and type the files use is type-checked",
        Some("python3" | "python") => {
            "Python: the import resolves the names the files import when they load; call shapes \
             and attribute uses are not checked"
        }
        Some("node") => {
            "TypeScript: the project's own compiler type-checks every file its tsconfig.json \
             includes, with no output"
        }
        Some("go") => "Go: go vet type-checks each package the files are in, with its test files",
        Some("javac") => {
            "Java: javac compiles the files and every source they use from the source path"
        }
        Some("dotnet") => {
            "C#: dotnet build compiles the project that holds the files, with its restored \
             dependencies"
        }
        _ => "the files are built by the command",
    }
}

/// What the orchestrator found for one producer.
#[derive(Clone, Debug, Default)]
pub struct ProducerFacts {
    pub producer: String,
    /// The consuming contracts left unbound against this producer.
    pub consumers: Vec<String>,
    /// The producer's declared writes and their content at the dispatch base
    /// (`None`: no such file there).
    pub writes: Vec<(String, Option<String>)>,
    /// Names the producer's files define (graph nodes).
    pub symbols: Vec<String>,
    /// Repository files that use the producer's files (graph edges, the
    /// consumers' own existing files), with their content.
    pub users: Vec<(String, String)>,
}

/// The producer's files that hold a scaffold at the dispatch base: the stub
/// the repository already provides.
pub fn stub_of(facts: &ProducerFacts) -> Vec<FileOverlay> {
    facts
        .writes
        .iter()
        .filter_map(|(p, c)| {
            c.as_ref()
                .filter(|c| is_scaffold(p, c))
                .map(|c| FileOverlay::new(p, c))
        })
        .collect()
}

/// Where a derived interface's stub comes from ([`derived_stub`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StubSource {
    /// The producer's scaffold files.
    Scaffold,
    /// The producer's existing files as they are: no file it writes holds a
    /// scaffold, so the shapes they have now are the interface.
    CurrentCode,
}

/// The stub of a derived interface and where it comes from: the scaffold
/// files when any file the producer writes holds one, else every file it
/// writes that exists at the dispatch base. A producer that edits existing
/// code still has an interface: the shapes its files have now. Empty when no
/// file it writes exists yet.
pub fn derived_stub(facts: &ProducerFacts) -> (Vec<FileOverlay>, StubSource) {
    let scaffold = stub_of(facts);
    if !scaffold.is_empty() {
        return (scaffold, StubSource::Scaffold);
    }
    let current = facts
        .writes
        .iter()
        .filter_map(|(p, c)| c.as_ref().map(|c| FileOverlay::new(p, c)))
        .collect();
    (current, StubSource::CurrentCode)
}

/// A compile-only interface for `facts`, or why none can be derived. The
/// command is given by `command` over the consumer and test paths; when it
/// gives none, `no_command` says why for those paths.
pub fn derive(
    facts: &ProducerFacts,
    command: impl Fn(&[String]) -> Option<Vec<String>>,
    no_command: impl Fn(&[String]) -> String,
) -> Result<InterfaceContract, String> {
    let (stub, _) = derived_stub(facts);
    if stub.is_empty() {
        return Err(format!(
            "no file {} writes exists in the repository yet, so neither a scaffold nor current \
             code stands in as its stub",
            facts.producer
        ));
    }
    // A producer in a language kGraf has no compile command for (shell,
    // TypeScript, ...) gets no check that cannot apply: the reason names its
    // language, before any search for the files that use it, which in such a
    // language often finds none and would name the wrong cause.
    if !stub.iter().any(|f| has_compile_language(&f.path)) {
        let code: Vec<String> = stub
            .iter()
            .map(|f| f.path.clone())
            .filter(|p| !PROSE.contains(&extension(p).as_str()))
            .collect();
        let named = if code.is_empty() {
            stub.iter().map(|f| f.path.clone()).collect()
        } else {
            code
        };
        return Err(no_command(&named));
    }
    let stub_paths: BTreeSet<&str> = stub.iter().map(|f| f.path.as_str()).collect();
    let mut symbols: Vec<String> = Vec::new();
    let mut consumer = Vec::new();
    let mut tests = Vec::new();
    for (path, content) in &facts.users {
        if stub_paths.contains(path.as_str()) {
            continue;
        }
        let used: Vec<&String> = facts
            .symbols
            .iter()
            .filter(|s| uses_word(content, s))
            .collect();
        if used.is_empty() {
            continue;
        }
        for s in used {
            if !symbols.contains(s) {
                symbols.push(s.clone());
            }
        }
        let file = FileOverlay::new(path, content);
        if is_test_path(path) {
            tests.push(file);
        } else {
            consumer.push(file);
        }
    }
    if consumer.is_empty() && tests.is_empty() {
        return Err(format!(
            "no repository file uses an item {}'s files define",
            facts.producer
        ));
    }
    let built: Vec<String> = consumer
        .iter()
        .chain(&tests)
        .map(|f| f.path.clone())
        .collect();
    let Some(compile) = command(&built) else {
        return Err(no_command(&built));
    };
    let mut parties = vec![facts.producer.clone()];
    parties.extend(facts.consumers.iter().cloned());
    Ok(InterfaceContract {
        kind: InterfaceKind::CompileOnly,
        id: format!("derived-{}", facts.producer),
        version: 1,
        parties,
        symbols,
        stub,
        wrong: Vec::new(),
        reference: Vec::new(),
        consumer,
        tests,
        compile,
        test: Vec::new(),
        consulted: Vec::new(),
        replaces: Vec::new(),
    })
}

/// The words that show a file uses a contract's code: the crate name of the
/// package that holds each of its files (`-` read as `_`), the Python module
/// of each `.py` file, and the distinctive item names its files declare (an
/// identifier in camel case or with an underscore, four characters or more,
/// so that `new` or `len` do not link every file to every contract).
pub fn use_tokens(
    writes: &[(String, Option<String>)],
    package: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |t: String| {
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    };
    for (path, content) in writes {
        if let Some(p) = package(path) {
            add(p.replace('-', "_"));
        }
        if let Some(m) = python_module(path) {
            add(m.rsplit('.').next().unwrap_or_default().to_owned());
        }
        for name in content.as_deref().map(declared_names).unwrap_or_default() {
            let camel = name.starts_with(|c: char| c.is_ascii_uppercase())
                && name.chars().any(|c| c.is_ascii_lowercase());
            if name.len() >= 4 && (camel || name.contains('_')) {
                add(name);
            }
        }
    }
    out
}

/// One contract of the plan, as far as the reach of a test goes.
#[derive(Clone, Debug, Default)]
pub struct ContractFacts {
    pub id: String,
    pub depends_on: Vec<String>,
    /// Whether a file the contract writes is missing at the dispatch base or
    /// holds a scaffold: its code does not exist yet.
    pub unimplemented: bool,
    /// [`use_tokens`] of its files.
    pub tokens: Vec<String>,
}

/// A test file of the repository.
#[derive(Clone, Debug)]
pub struct TestFile {
    pub path: String,
    pub text: String,
}

/// Whether the planner is asked for a behavior check of one producer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BehaviorPlan {
    /// Judgment is needed: no repository test reaches the producer, so the
    /// planner writes a reference and a small test of it alone.
    Ask,
    /// No ask. The reason is one line, recorded with the edge.
    Skip(String),
}

/// Decide whether a behavior check of `producer` needs the planner.
///
/// A test reaches a contract when it uses one of the contract's tokens, and
/// reaches everything that contract depends on, transitively (a test of the
/// outermost contract runs the code below it). Over the tests that reach
/// `producer`:
/// - none: [`BehaviorPlan::Ask`].
/// - one reaches no other unimplemented contract: the repository already has a
///   test of the producer alone, which the verifier runs; no ask.
/// - every one reaches other unimplemented contracts: a reference of the
///   producer alone cannot pass any of them (the others' scaffolds panic), so
///   a check asked for would be refused as vacuous; no ask.
pub fn behavior_plan(
    producer: &str,
    tests: &[TestFile],
    contracts: &[ContractFacts],
) -> BehaviorPlan {
    let by_id: BTreeMap<&str, &ContractFacts> =
        contracts.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut reaching: Vec<(&TestFile, BTreeSet<&str>)> = Vec::new();
    for t in tests {
        let mut reach: BTreeSet<&str> = BTreeSet::new();
        let mut open: Vec<&str> = contracts
            .iter()
            .filter(|c| c.tokens.iter().any(|tok| uses_word(&t.text, tok)))
            .map(|c| c.id.as_str())
            .collect();
        while let Some(id) = open.pop() {
            if !reach.insert(id) {
                continue;
            }
            if let Some(c) = by_id.get(id) {
                open.extend(c.depends_on.iter().map(String::as_str));
            }
        }
        if reach.contains(producer) {
            reaching.push((t, reach));
        }
    }
    if reaching.is_empty() {
        return BehaviorPlan::Ask;
    }
    let blockers = |reach: &BTreeSet<&str>| -> Vec<String> {
        reach
            .iter()
            .filter(|id| **id != producer && by_id.get(**id).is_some_and(|c| c.unimplemented))
            .map(|id| (*id).to_owned())
            .collect()
    };
    if let Some((t, _)) = reaching.iter().find(|(_, r)| blockers(r).is_empty()) {
        return BehaviorPlan::Skip(format!(
            "the repository's test {} already exercises {producer} alone, and the verifier runs it",
            t.path
        ));
    }
    let mut paths: Vec<&str> = reaching.iter().map(|(t, _)| t.path.as_str()).collect();
    paths.sort_unstable();
    let mut others: BTreeSet<String> = BTreeSet::new();
    for (_, r) in &reaching {
        others.extend(blockers(r));
    }
    BehaviorPlan::Skip(format!(
        "the only tests that exercise {producer} ({}) also need {} unimplemented, so no test of \
         {producer} alone can pass against a reference of it",
        paths.join(", "),
        others.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ProducerFacts {
        ProducerFacts {
            producer: "c1".into(),
            consumers: vec!["c5".into()],
            writes: vec![(
                "discount_flat/src/lib.rs".into(),
                Some("pub struct FlatDiscount;\nfn d() -> i64 { todo!() }\n".into()),
            )],
            symbols: vec!["FlatDiscount".into(), "d".into()],
            users: vec![
                (
                    "pricing_engine/src/lib.rs".into(),
                    "use discount_flat::FlatDiscount;\npub fn price() { todo!() }\n".into(),
                ),
                ("checkout/tests/integration.rs".into(), "fn t() {}\n".into()),
            ],
        }
    }

    fn cargo_pkg(files: &[String]) -> Option<Vec<String>> {
        compile_command(files, |f| {
            Some(f.split('/').next().unwrap_or("").to_owned())
        })
    }

    fn reason(files: &[String]) -> String {
        no_command_reason(files, |f| {
            Some(f.split('/').next().unwrap_or("").to_owned())
        })
    }

    #[test]
    fn a_scaffold_becomes_the_stub_and_its_users_the_consumers() {
        let ic = derive(&facts(), cargo_pkg, reason).expect("derived");
        assert_eq!(ic.stub[0].path, "discount_flat/src/lib.rs");
        assert_eq!(derived_stub(&facts()).1, StubSource::Scaffold);
        assert_eq!(ic.consumer.len(), 1, "the integration test uses no symbol");
        assert!(ic.tests.is_empty());
        assert_eq!(ic.symbols, vec!["FlatDiscount".to_owned()]);
        assert_eq!(
            ic.compile,
            ["cargo", "test", "--no-run", "-p", "pricing_engine"]
        );
        assert_eq!(ic.parties, ["c1", "c5"]);
        assert!(ic.reference.is_empty(), "no reference: vacuity cannot run");
    }

    /// A producer that edits existing code holds no scaffold: its current
    /// files are the stub, and the files that use them are the consumers.
    /// Before, no check was derived and the edge read "no file holds a
    /// scaffold".
    #[test]
    fn a_producer_that_edits_existing_code_gets_its_current_code_as_the_stub() {
        let mut f = facts();
        let current = "pub struct FlatDiscount;\npub fn d() -> i64 { 3 }\n";
        f.writes[0].1 = Some(current.into());
        f.writes.push(("discount_flat/src/new.rs".into(), None));
        let (stub, source) = derived_stub(&f);
        assert_eq!(source, StubSource::CurrentCode);
        assert_eq!(stub.len(), 1, "a file that does not exist yet is no stub");
        let ic = derive(&f, cargo_pkg, reason).expect("derived from current code");
        assert_eq!(ic.stub[0].path, "discount_flat/src/lib.rs");
        assert_eq!(ic.stub[0].content, current);
        assert_eq!(
            ic.compile,
            ["cargo", "test", "--no-run", "-p", "pricing_engine"]
        );
        assert_eq!(ic.symbols, vec!["FlatDiscount".to_owned()]);
    }

    #[test]
    fn no_file_yet_or_no_user_or_no_command_derives_nothing_and_says_why() {
        let mut f = facts();
        f.writes[0].1 = None;
        let why = derive(&f, cargo_pkg, reason).expect_err("no file yet");
        assert!(why.contains("exists in the repository yet"), "{why}");
        let mut f = facts();
        f.users.clear();
        assert!(
            derive(&f, cargo_pkg, reason)
                .expect_err("no user")
                .contains("uses")
        );
        let why = derive(
            &facts(),
            |_| None,
            |files| no_command_reason(files, |_| None),
        )
        .expect_err("no command");
        assert!(
            why.contains("no Cargo package holds `pricing_engine/src/lib.rs`"),
            "{why}"
        );
    }

    /// A producer whose users are shell scripts is never given a compile
    /// check that cannot apply: no interface is derived, and the reason names
    /// the language. The same holds for every language kGraf has no compile
    /// command for.
    #[test]
    fn a_shell_or_typescript_user_gets_no_compile_check_and_the_reason_names_the_language() {
        let f = ProducerFacts {
            producer: "c1".into(),
            consumers: vec!["c2".into()],
            writes: vec![(
                "lib/common.sh".into(),
                Some("greet() {\n  echo hi\n}\n".into()),
            )],
            symbols: vec!["greet".into()],
            users: vec![("bin/run.sh".into(), ". lib/common.sh\ngreet\n".into())],
        };
        let why = derive(&f, cargo_pkg, reason).expect_err("no shell compile check");
        assert!(
            why.contains("shell scripts") && why.contains("no compile check applies"),
            "{why}"
        );
        let none = BuildFacts::default();
        let ts = command_for(&["web/a.ts".into()], |_| None, &none).expect_err("no tsconfig");
        assert!(
            ts.contains("TypeScript") && ts.contains("no type check"),
            "{ts}"
        );
        assert!(reason(&["web/a.js".into()]).contains("JavaScript"));
        assert!(reason(&["a/x.rs".into(), "b/y.py".into()]).contains("more than one language"));
        assert!(reason(&["my-pkg/x.py".into()]).contains("not importable"));
        assert!(coverage(&["python3".into()]).contains("call shapes"));
        assert!(coverage(&["python".into()]).contains("call shapes"));
        assert!(coverage(&["cargo".into()]).contains("signature"));
    }

    fn build(present: &[&str], programs: &[&str]) -> BuildFacts {
        BuildFacts {
            present: present.iter().map(|p| ((*p).to_owned(), true)).collect(),
            programs: programs.iter().map(|p| (*p).to_owned()).collect(),
            ..BuildFacts::default()
        }
    }

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|f| (*f).to_owned()).collect()
    }

    /// TypeScript: a type check with the project's own compiler when a
    /// tsconfig.json and `node_modules/typescript` are at the base and `node`
    /// is installed; each missing part is a reason, never a command.
    #[test]
    fn typescript_gets_a_type_check_only_with_its_own_compiler() {
        let f = files(&["web/src/cart.ts", "web/src/view.tsx"]);
        let all = build(
            &["web/tsconfig.json", "node_modules/typescript/bin/tsc"],
            &["node"],
        );
        assert_eq!(
            command_for(&f, |_| None, &all).expect("command"),
            [
                "node",
                "node_modules/typescript/bin/tsc",
                "--noEmit",
                "-p",
                "web"
            ]
        );
        let why = command_for(&f, |_| None, &build(&["web/tsconfig.json"], &["node"]))
            .expect_err("no compiler");
        assert!(why.contains("node_modules/typescript"), "{why}");
        let why = command_for(
            &f,
            |_| None,
            &build(
                &["web/tsconfig.json", "web/node_modules/typescript/bin/tsc"],
                &[],
            ),
        )
        .expect_err("no node");
        assert!(why.contains("`node` is not installed"), "{why}");
        let why = command_for(&f, |_| None, &build(&[], &["node"])).expect_err("no tsconfig");
        assert!(why.contains("no tsconfig.json above"), "{why}");
        let why = command_for(
            &files(&["a/x.ts", "b/y.ts"]),
            |_| None,
            &build(&["a/tsconfig.json", "b/tsconfig.json"], &["node"]),
        )
        .expect_err("two projects");
        assert!(why.contains("more than one tsconfig.json"), "{why}");
    }

    /// Go: `go vet` of the files' packages, from the module root; a module
    /// below the root is named with `-C`.
    #[test]
    fn go_gets_go_vet_of_the_packages_when_go_is_installed() {
        let f = files(&[
            "pricing/price.go",
            "pricing/price_test.go",
            "cmd/app/main.go",
        ]);
        assert_eq!(
            command_for(&f, |_| None, &build(&["go.mod"], &["go"])).expect("command"),
            ["go", "vet", "./cmd/app", "./pricing"]
        );
        assert_eq!(
            command_for(
                &files(&["svc/pricing/price.go", "svc/main.go"]),
                |_| None,
                &build(&["svc/go.mod"], &["go"])
            )
            .expect("command"),
            ["go", "-C", "svc", "vet", ".", "./pricing"]
        );
        let why = command_for(&f, |_| None, &build(&["go.mod"], &[])).expect_err("no go");
        assert!(why.contains("`go` is not installed"), "{why}");
        let why = command_for(&f, |_| None, &build(&[], &["go"])).expect_err("no module");
        assert!(why.contains("no Go module"), "{why}");
    }

    /// Java: `javac` over the files with their source roots, only when no
    /// build tool is present; a Maven or Gradle project is unchecked.
    #[test]
    fn java_gets_javac_only_without_a_build_tool() {
        let f = files(&["src/shop/Cart.java", "src/shop/CartTest.java"]);
        let mut b = build(&[], &["javac"]);
        b.texts.insert(
            "src/shop/Cart.java".into(),
            "package shop;\nclass Cart {}\n".into(),
        );
        b.texts.insert(
            "src/shop/CartTest.java".into(),
            "package shop;\nclass CartTest {}\n".into(),
        );
        assert_eq!(
            command_for(&f, |_| None, &b).expect("command"),
            [
                "javac",
                "-proc:none",
                "-d",
                JAVAC_OUT,
                "-sourcepath",
                "src",
                "src/shop/Cart.java",
                "src/shop/CartTest.java"
            ]
        );
        b.present.insert("pom.xml".into(), true);
        let why = command_for(&f, |_| None, &b).expect_err("maven");
        assert!(
            why.contains("`pom.xml`") && why.contains("no compile check"),
            "{why}"
        );
        let mut b = build(&[], &["javac"]);
        b.texts.insert(
            "src/shop/Cart.java".into(),
            "package other;\nclass Cart {}\n".into(),
        );
        let why = command_for(&files(&["src/shop/Cart.java"]), |_| None, &b)
            .expect_err("package mismatch");
        assert!(why.contains("does not match its directory"), "{why}");
        let why = command_for(&f, |_| None, &build(&[], &[])).expect_err("no javac");
        assert!(why.contains("`javac` is not installed"), "{why}");
    }

    /// C#: `dotnet build --no-restore` of the one project above the files,
    /// only when its dependencies are restored at the base.
    #[test]
    fn csharp_gets_dotnet_build_only_when_restored() {
        let f = files(&["App/Pricing.cs", "App/Models/Cart.cs"]);
        let mut b = build(&["App/obj/project.assets.json"], &["dotnet"]);
        b.listings.insert(
            "App".into(),
            vec!["App.csproj".into(), "Pricing.cs".into(), "Models".into()],
        );
        assert_eq!(
            command_for(&f, |_| None, &b).expect("command"),
            ["dotnet", "build", "--no-restore", "App/App.csproj"]
        );
        b.present.clear();
        let why = command_for(&f, |_| None, &b).expect_err("not restored");
        assert!(why.contains("not restored"), "{why}");
        let why = command_for(&f, |_| None, &build(&[], &["dotnet"])).expect_err("no project");
        assert!(why.contains("no C# project"), "{why}");
    }

    /// The probes are read only for the languages that need them.
    #[test]
    fn config_probes_read_nothing_for_rust_and_python() {
        let (paths, dirs, programs) = config_probes(&files(&["a/src/lib.rs", "b/x.py"]));
        assert!(paths.is_empty() && dirs.is_empty() && programs.is_empty());
        let (paths, _, programs) = config_probes(&files(&["svc/a.go"]));
        assert!(paths.contains(&"svc/go.mod".to_owned()) && paths.contains(&"go.mod".to_owned()));
        assert_eq!(programs, ["go"]);
    }

    /// A producer file that names a placeholder only in text (a brief in a
    /// string literal, a doc comment) is finished code, not a scaffold: it
    /// stands in as the stub as current code, never as a scaffold.
    #[test]
    fn a_file_that_names_a_placeholder_only_in_text_is_not_a_scaffold() {
        let prose = "/// Doc: `todo!()` panics.\npub fn brief() -> &'static str {\n    \"`todo!()` panics; NotImplementedError; not implemented here\"\n}\n";
        assert!(!is_scaffold("discount_flat/src/lib.rs", prose));
        let mut f = facts();
        f.writes[0].1 = Some(prose.into());
        assert!(stub_of(&f).is_empty());
        assert_eq!(derived_stub(&f).1, StubSource::CurrentCode);
        // Real scaffolds in each form still are.
        for (path, src) in [
            ("a/src/lib.rs", "pub fn f() -> u8 { todo!() }\n"),
            (
                "a/src/lib.rs",
                "pub fn f() -> u8 { unimplemented!(\"later\") }\n",
            ),
            ("a/mod.py", "def f():\n    raise NotImplementedError\n"),
            (
                "a/index.ts",
                "export function f(): number {\n  throw new Error(\"Not implemented\");\n}\n",
            ),
        ] {
            assert!(is_scaffold(path, src), "{path}: {src}");
        }
    }

    #[test]
    fn declared_names_are_read_by_keyword() {
        assert_eq!(
            declared_names(
                "class Config:\n    pass\ndef parse_config(text):\n    raise NotImplementedError\n"
            ),
            ["Config", "parse_config"]
        );
        assert_eq!(
            declared_names("pub struct FlatDiscount;\nfn d() {}"),
            ["FlatDiscount", "d"]
        );
        // Go functions and methods (the receiver is not a name), C# and Java
        // records.
        assert_eq!(
            declared_names(
                "package p\n\nfunc LookupPrice() int { return 1 }\nfunc (c *Cart) CartTotal() int { return 0 }\ntype Cart struct{}\n"
            ),
            ["Cart", "LookupPrice", "CartTotal"]
        );
        assert_eq!(
            declared_names("public record PriceRow(int Cents);\n"),
            ["PriceRow"]
        );
        assert!(declared_names("var funcs = 1; myfunc(x)\n").is_empty());
    }

    #[test]
    fn test_paths_python_modules_and_cargo_packages_are_read_by_convention() {
        assert!(is_test_path("checkout/tests/integration.rs"));
        assert!(is_test_path("tests/test_library.py"));
        assert!(is_test_path("src/a.test.ts"));
        assert!(!is_test_path("src/testing.rs"));
        // The Java and C# class conventions and .NET test projects.
        assert!(is_test_path("App/PriceTests.cs"));
        assert!(is_test_path("App/PriceTest.cs"));
        assert!(is_test_path("App.Tests/Pricing.cs"));
        assert!(is_test_path("src/main/java/a/PriceTest.java"));
        assert!(is_test_path("Tests/AppTests/Pricing.swift"));
        assert!(is_test_path("lib/price_spec.rb"));
        assert!(!is_test_path("src/main/java/a/Contest.java"));
        assert!(!is_test_path("App/Latest.cs"));
        assert!(!is_test_path("App/Test.cs"), "a class named Test alone");
        assert!(!is_test_path("App/TESTS.cs"));
        assert_eq!(
            python_module("shapes/library.py").as_deref(),
            Some("shapes.library")
        );
        assert_eq!(python_module("pkg/__init__.py").as_deref(), Some("pkg"));
        assert_eq!(python_module("my-pkg/x.py"), None);
        assert_eq!(
            cargo_package("[package]\nname = \"tax_us\"\n[dependencies]\nname = 1\n").as_deref(),
            Some("tax_us")
        );
        assert_eq!(ancestors("a/b/c.rs"), ["a/b", "a", ""]);
        assert_eq!(
            compile_command(&["tests/test_library.py".into()], |_| None),
            Some(vec![
                kgraf_sandbox::python_program().into(),
                "-c".into(),
                "import tests.test_library".into()
            ])
        );
    }

    fn contract(id: &str, deps: &[&str], unimplemented: bool, tokens: &[&str]) -> ContractFacts {
        ContractFacts {
            id: id.into(),
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            unimplemented,
            tokens: tokens.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    fn test_file(path: &str, text: &str) -> TestFile {
        TestFile {
            path: path.into(),
            text: text.into(),
        }
    }

    /// The order-pricing shape: one test, which calls the outermost contract
    /// and so runs every producer below it. No test of one producer alone can
    /// pass against that producer's reference.
    fn order_pricing() -> Vec<ContractFacts> {
        vec![
            contract("c1", &[], true, &["discount_flat", "FlatDiscount"]),
            contract("c2", &[], true, &["discount_bulk", "BulkDiscount"]),
            contract("c3", &[], true, &["tax_us", "state_tax_rate"]),
            contract(
                "c4",
                &["c3"],
                true,
                &["shipping_standard", "StandardShipping"],
            ),
            contract(
                "c5",
                &["c1", "c2", "c3", "c4"],
                true,
                &["pricing_engine", "price_order"],
            ),
            contract("c6", &["c5"], true, &["checkout", "checkout_order"]),
        ]
    }

    #[test]
    fn a_test_that_reaches_every_producer_through_the_outermost_one_asks_nothing() {
        let tests = [test_file(
            "checkout/tests/integration.rs",
            "use checkout::checkout_order;\nfn t() { checkout_order(&o); }\n",
        )];
        for producer in ["c1", "c2", "c3", "c4", "c5"] {
            let BehaviorPlan::Skip(why) = behavior_plan(producer, &tests, &order_pricing()) else {
                panic!("{producer} is not asked about");
            };
            assert!(
                why.contains("the only tests that exercise")
                    && why.contains("checkout/tests/integration.rs")
                    && why.contains("c6")
                    && why.contains("unimplemented"),
                "{why}"
            );
        }
        let BehaviorPlan::Skip(why) = behavior_plan("c1", &tests, &order_pricing()) else {
            panic!("skipped");
        };
        assert!(
            why.contains("also need c2, c3, c4, c5, c6 unimplemented"),
            "the others it needs are named, the producer is not: {why}"
        );
    }

    #[test]
    fn no_test_reaching_the_producer_means_the_planner_is_asked() {
        // No test at all, and a test that reaches only another producer.
        assert_eq!(
            behavior_plan("c1", &[], &order_pricing()),
            BehaviorPlan::Ask
        );
        let tests = [test_file(
            "tax_us/tests/t.rs",
            "use tax_us::state_tax_rate;\n",
        )];
        assert_eq!(
            behavior_plan("c1", &tests, &order_pricing()),
            BehaviorPlan::Ask
        );
    }

    #[test]
    fn a_test_of_the_producer_alone_is_the_repositorys_and_nothing_is_asked() {
        let tests = [test_file(
            "tax_us/tests/t.rs",
            "use tax_us::state_tax_rate;\nfn t() { state_tax_rate(\"CA\"); }\n",
        )];
        let BehaviorPlan::Skip(why) = behavior_plan("c3", &tests, &order_pricing()) else {
            panic!("not asked");
        };
        assert!(
            why.contains("tax_us/tests/t.rs") && why.contains("already exercises c3 alone"),
            "{why}"
        );
    }

    /// A contract whose code already exists does not block a test: only an
    /// unimplemented one makes a reference of the producer alone fail.
    #[test]
    fn an_implemented_contract_a_test_also_reaches_does_not_block_it() {
        let mut contracts = order_pricing();
        for c in &mut contracts {
            if c.id != "c1" {
                c.unimplemented = false;
            }
        }
        let tests = [test_file(
            "checkout/tests/integration.rs",
            "use checkout::checkout_order;\n",
        )];
        let BehaviorPlan::Skip(why) = behavior_plan("c1", &tests, &contracts) else {
            panic!("a test of c1 through finished code exists");
        };
        assert!(why.contains("already exercises c1 alone"), "{why}");
    }

    #[test]
    fn use_tokens_name_the_crate_the_module_and_the_distinctive_items_only() {
        let writes = vec![(
            "discount-flat/src/lib.rs".to_owned(),
            Some("pub struct FlatDiscount;\nfn new() {}\nfn discount_cents() {}\n".to_owned()),
        )];
        let tokens = use_tokens(&writes, |_| Some("discount-flat".to_owned()));
        assert!(tokens.contains(&"discount_flat".to_owned()), "{tokens:?}");
        assert!(tokens.contains(&"FlatDiscount".to_owned()), "{tokens:?}");
        assert!(tokens.contains(&"discount_cents".to_owned()), "{tokens:?}");
        assert!(
            !tokens.contains(&"new".to_owned()),
            "a short plain name links every file to every contract: {tokens:?}"
        );
        let py = vec![("discount/flat.py".to_owned(), None)];
        assert_eq!(use_tokens(&py, |_| None), ["flat"]);
    }
}
