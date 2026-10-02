//! A contract's acceptance check, narrowed to the tests of its own files.
//!
//! **The rule.** A contract is accepted on the tests that belong to its declared
//! write set; the project's whole test command judges the integrated result, as
//! the root quality gate (decision 0018), once every contract has landed.
//!
//! **Why.** The run's acceptance command (`--verify`) is usually the project's
//! whole suite (`python3 -m unittest discover -s tests`, `npm test`, `cargo test
//! --workspace`). A contract's write set is a slice of the work. Before its
//! siblings merge, the suite fails on their stubs, so no contract could pass its
//! own acceptance, and workers asked for writes outside their scope to fix
//! files that were other contracts' work.
//!
//! **How a test belongs to a file.** By the naming convention of the runner the
//! command names, read from the command and the tree; nothing is guessed from a
//! model:
//!
//! - `python -m unittest discover [-s DIR] [-t TOP]`: `test_<stem>.py` under the
//!   start directory, selected with `-p test_<stem>.py`;
//! - `pytest` / `python -m pytest` with options only: `test_<stem>.py` or
//!   `<stem>_test.py` anywhere, appended as a path;
//! - `node --test` with options only, and `npm test` whose `test` script is
//!   `node --test`, `jest` or `vitest`: `<stem>.test.<ext>`, `<stem>.spec.<ext>`,
//!   or `<stem>.<ext>` under a `test`, `tests` or `__tests__` directory;
//! - `cargo test` / `cargo nextest run` over a workspace: the package each
//!   declared file is in, selected with `-p <package>`.
//!
//! A declared file that is itself such a test selects itself. One command runs
//! per test file or package, joined with `&&`, so each part is a command the
//! worker may also run (see [`variants`]).
//!
//! **When the whole command runs.** An unrecognized command (a script, `make`,
//! anything with shell syntax or quotes), a command that already selects tests
//! (a path, a `-p` pattern, a package), or a write set with no test of its own:
//! the whole command runs, as before, and the reason is kept.
//!
//! **Known limit.** Cargo narrows to the package, not further: two contracts in
//! one crate still see each other's stubs fail that crate's tests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// What a contract's attempt is checked with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The run's command, unchanged; `why` says why it was not narrowed.
    Whole { why: String },
    /// The tests that belong to the contract's files.
    Narrowed {
        /// The parts, each a complete command; run joined with `&&`.
        parts: Vec<String>,
        /// The test files or packages selected, for the brief and the verdict.
        selected: Vec<String>,
    },
}

impl Scope {
    /// The command to run: the narrowed parts joined, or `whole`.
    pub(crate) fn command(&self, whole: &str) -> String {
        match self {
            Scope::Whole { .. } => whole.to_owned(),
            Scope::Narrowed { parts, .. } => parts.join(" && "),
        }
    }
}

/// Directories never searched for tests: version control, dependencies, build
/// output, environments.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".kgraf",
    "node_modules",
    "target",
    "build",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
];

/// Files looked at, at most, when searching for tests: a bound on a huge tree.
/// Past it the search stops and the whole command runs, saying so.
const MAX_FILES: usize = 50_000;

/// Script and module extensions a JavaScript or TypeScript test runner loads.
const JS_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// The runner a command names, when it is one this module can narrow.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Runner {
    /// `python -m unittest discover`, with its start directory.
    Unittest { start: String },
    /// `pytest` or `python -m pytest`, options only.
    Pytest,
    /// `node --test`, options only, or `npm test` running a file-taking runner.
    /// `npm` is true when the files go after `npm test --`.
    Node { npm: bool },
    /// `cargo test` or `cargo nextest run` over every package of a workspace;
    /// `drop` is the token that selects all packages (`--workspace`, `--all`),
    /// removed when narrowing, or empty for a virtual manifest's default.
    Cargo { drop: String },
}

/// Whether `command`, run in `root`, is a whole-suite command this module can
/// narrow. A run whose acceptance command is narrowable gets that command as a
/// root gate too, so the whole suite still judges the merged result.
pub(crate) fn narrowable(command: &str, root: &Path) -> bool {
    runner(command, root).is_ok()
}

/// `gates` with `verify` appended when `verify` is narrowable and not already
/// one of them, and whether it was appended. The whole acceptance command then
/// judges the merged result: per-contract checks ran only their own tests.
pub(crate) fn with_whole_gate(
    mut gates: Vec<String>,
    verify: &str,
    root: &Path,
) -> (Vec<String>, bool) {
    let present = gates.iter().any(|g| g.trim() == verify.trim());
    if present || !narrowable(verify, root) {
        return (gates, false);
    }
    gates.push(verify.trim().to_owned());
    (gates, true)
}

/// What `command` checks for a contract that declared `writes`, in the tree at
/// `root` (the attempt's worktree).
pub(crate) fn scope(command: &str, root: &Path, writes: &[&str]) -> Scope {
    let runner = match runner(command, root) {
        Ok(r) => r,
        Err(why) => return Scope::Whole { why },
    };
    let tokens: Vec<&str> = command.split_whitespace().collect();
    match &runner {
        Runner::Cargo { drop } => {
            let packages: BTreeSet<String> = writes
                .iter()
                .filter_map(|w| cargo_package_of(root, w))
                .collect();
            if packages.is_empty() {
                return Scope::Whole {
                    why: "none of the contract's files is in a cargo package".into(),
                };
            }
            let base: Vec<&str> = tokens.iter().copied().filter(|t| t != drop).collect();
            let parts = packages
                .iter()
                .map(|p| cargo_part(&base, p))
                .collect::<Vec<_>>();
            Scope::Narrowed {
                parts,
                selected: packages
                    .into_iter()
                    .map(|p| format!("package {p}"))
                    .collect(),
            }
        }
        _ => {
            let tests = match test_files(&runner, root) {
                Ok(t) => t,
                Err(why) => return Scope::Whole { why },
            };
            let mut selected = BTreeSet::new();
            for w in writes {
                selected.extend(tests_for_write(&runner, &tests, w));
            }
            if selected.is_empty() {
                return Scope::Whole {
                    why: "no test file belongs to the contract's files by the runner's naming"
                        .into(),
                };
            }
            let parts = selected
                .iter()
                .map(|t| file_part(&runner, &tokens, t))
                .collect();
            Scope::Narrowed {
                parts,
                selected: selected.into_iter().collect(),
            }
        }
    }
}

/// Every single-part command [`scope`] can produce for `command` in the tree at
/// `root` as it is now: one per test file, or one per cargo package. Added to
/// what a worker may run, so a worker can run the check its attempt is judged
/// by (decision 0084). A test file a worker adds later is checked by the
/// verifier but not in this list; the worker can still run the whole command.
pub(crate) fn variants(command: &str, root: &Path) -> Vec<String> {
    let Ok(runner) = runner(command, root) else {
        return Vec::new();
    };
    let tokens: Vec<&str> = command.split_whitespace().collect();
    match &runner {
        Runner::Cargo { drop } => {
            let base: Vec<&str> = tokens.iter().copied().filter(|t| t != drop).collect();
            cargo_packages(root)
                .iter()
                .map(|p| cargo_part(&base, p))
                .collect()
        }
        _ => test_files(&runner, root)
            .unwrap_or_default()
            .iter()
            .filter(|t| is_test_file(&runner, t))
            .map(|t| file_part(&runner, &tokens, t))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    }
}

/// The text a worker's brief carries about its check. A narrowed check names
/// its own commands. A whole check (a command kGraf cannot narrow, such as
/// `cargo build --workspace`) still names the command, so the worker knows what
/// it is judged by; the contract's own Acceptance line is advisory then.
pub(crate) fn brief(scope: &Scope, whole: &str) -> Option<String> {
    let Scope::Narrowed { parts, selected } = scope else {
        return Some(format!(
            "\n\nYour attempt is accepted when `{whole}` passes in your worktree. The \
             Acceptance line of your contract is advisory. `todo!()` panics or failing tests \
             in other contracts' unfinished code are not yours to fix; a compile error \
             anywhere that your change caused (for example by changing a public type's \
             shape) is yours."
        ));
    };
    Some(format!(
        "\n\nYour attempt is accepted when this passes in your worktree: `{}`. It runs the \
         tests that belong to your files ({}). The whole `{whole}` runs on the merged result \
         after every contract has landed; tests of files outside your write set may fail \
         until their own contracts merge, and they are not yours to fix.",
        parts.join(" && "),
        selected.join(", ")
    ))
}

// -- recognizing the command ---------------------------------------------------------------

fn runner(command: &str, root: &Path) -> Result<Runner, String> {
    let trimmed = command.trim();
    if trimmed.chars().any(is_shell_syntax) {
        return Err("the command has shell syntax or quotes, so it is run as written".into());
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    match tokens.as_slice() {
        [py, "-m", "unittest", rest @ ..] if is_python(py) => unittest(rest),
        [py, "-m", "pytest", rest @ ..] if is_python(py) => pytest(rest),
        ["pytest", rest @ ..] => pytest(rest),
        ["node", "--test", rest @ ..] => {
            options_only(rest)?;
            Ok(Runner::Node { npm: false })
        }
        ["npm", "test"] | ["npm", "t"] | ["npm", "run", "test"] => npm_test(root),
        ["cargo", "test", rest @ ..] | ["cargo", "nextest", "run", rest @ ..] => cargo(rest, root),
        _ => Err("the command is not a test runner kGraf can narrow to a contract's files".into()),
    }
}

fn is_shell_syntax(c: char) -> bool {
    matches!(
        c,
        ';' | '&' | '|' | '`' | '$' | '(' | ')' | '<' | '>' | '"' | '\'' | '\\' | '*' | '?' | '['
    )
}

fn is_python(token: &str) -> bool {
    let name = token.rsplit('/').next().unwrap_or(token);
    name == "python"
        || name == "python3"
        || name
            .strip_prefix("python3.")
            .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
}

fn unittest(rest: &[&str]) -> Result<Runner, String> {
    // Bare `python -m unittest` is discovery from the current directory.
    let args = match rest {
        [] => &[][..],
        ["discover", args @ ..] => args,
        _ => return Err("the unittest command already names what it runs".into()),
    };
    let mut start = ".".to_owned();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-s" | "--start-directory" => {
                start = args.get(i + 1).ok_or("`-s` has no value")?.to_string();
                i += 2;
            }
            "-t" | "--top-level-directory" => i += 2,
            "-p" | "--pattern" | "-k" => {
                return Err("the unittest command already selects its tests".into());
            }
            a if a.starts_with('-') => i += 1,
            // Positional start directory, pattern, top: a pattern or top is a
            // selection the user made.
            _ if i == 0 => {
                start = args[0].to_owned();
                i += 1;
            }
            _ => return Err("the unittest command already selects its tests".into()),
        }
    }
    Ok(Runner::Unittest { start })
}

/// Options that take the next token as their value.
const PYTEST_VALUE_OPTS: &[&str] = &[
    "-k",
    "-m",
    "-p",
    "-c",
    "-o",
    "-n",
    "-W",
    "--rootdir",
    "--maxfail",
    "--tb",
    "--confcutdir",
    "--basetemp",
];

fn pytest(rest: &[&str]) -> Result<Runner, String> {
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        if a == "-k" || a == "-m" {
            return Err("the pytest command already selects its tests".into());
        }
        if PYTEST_VALUE_OPTS.contains(&a) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Err("the pytest command already names what it runs".into());
        }
    }
    Ok(Runner::Pytest)
}

fn options_only(rest: &[&str]) -> Result<(), String> {
    if rest.iter().all(|a| a.starts_with('-')) {
        Ok(())
    } else {
        Err("the command already names what it runs".into())
    }
}

/// `npm test` narrows only when the `test` script is a runner that takes files
/// as arguments and has no shell syntax of its own.
fn npm_test(root: &Path) -> Result<Runner, String> {
    let text = std::fs::read_to_string(root.join("package.json"))
        .map_err(|e| format!("package.json could not be read: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("package.json is not JSON: {e}"))?;
    let script = json
        .get("scripts")
        .and_then(|s| s.get("test"))
        .and_then(|t| t.as_str())
        .ok_or("package.json has no `test` script")?;
    if script.chars().any(is_shell_syntax) {
        return Err("the `test` script has shell syntax, so it is run as written".into());
    }
    let tokens: Vec<&str> = script.split_whitespace().collect();
    let rest = match tokens.as_slice() {
        ["node", "--test", rest @ ..] => rest,
        ["jest", rest @ ..] | ["vitest", "run", rest @ ..] | ["vitest", rest @ ..] => rest,
        _ => {
            return Err(format!(
                "the `test` script `{script}` is not a runner kGraf can narrow"
            ));
        }
    };
    options_only(rest)?;
    Ok(Runner::Node { npm: true })
}

fn cargo(rest: &[&str], root: &Path) -> Result<Runner, String> {
    let selects = |a: &&str| {
        matches!(
            *a,
            "-p" | "--package" | "--exclude" | "--lib" | "--bins" | "--test" | "--tests"
        ) || a.starts_with("--package=")
            || a.starts_with("--exclude=")
            || a.starts_with("--test=")
            || a.starts_with("-p")
    };
    // Arguments after `--` go to the test binaries: a filter there is the
    // user's selection.
    let (opts, after) = match rest.iter().position(|a| *a == "--") {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &[][..]),
    };
    if opts.iter().any(selects) {
        return Err("the cargo command already selects its packages or tests".into());
    }
    if opts.iter().any(|a| !a.starts_with('-')) || after.iter().any(|a| !a.starts_with('-')) {
        return Err("the cargo command already filters its tests".into());
    }
    let drop = opts
        .iter()
        .find(|a| **a == "--workspace" || **a == "--all")
        .map(|a| a.to_string());
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| format!("Cargo.toml could not be read: {e}"))?;
    let doc: toml::Table = manifest
        .parse()
        .map_err(|e| format!("Cargo.toml is not TOML: {e}"))?;
    let virtual_manifest = !doc.contains_key("package");
    match drop {
        Some(drop) => Ok(Runner::Cargo { drop }),
        None if virtual_manifest => Ok(Runner::Cargo {
            drop: String::new(),
        }),
        None => Err("the cargo command tests one package already".into()),
    }
}

// -- finding tests -------------------------------------------------------------------------

/// Every file under the runner's search root, relative to `root`, with `/`.
fn test_files(runner: &Runner, root: &Path) -> Result<Vec<String>, String> {
    let base = match runner {
        Runner::Unittest { start } => root.join(start),
        _ => root.to_path_buf(),
    };
    let mut out = Vec::new();
    let mut stack = vec![base];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_FILES {
                return Err(format!(
                    "the tree has more than {MAX_FILES} files to search for tests"
                ));
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if !SKIP_DIRS.contains(&name.as_ref()) && !name.starts_with('.') {
                    stack.push(entry.path());
                }
            } else if kind.is_file()
                && let Ok(rel) = entry.path().strip_prefix(root)
            {
                out.push(slash(rel));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `name` without its last extension, and the extension.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext),
        _ => (name, ""),
    }
}

/// The stem a write names, for matching: a package's `__init__.py` or an
/// `index.ts` names its directory.
fn write_stem(write: &str) -> Option<(&str, &str)> {
    let (stem, ext) = split_ext(file_name(write));
    let stem = if stem == "__init__" || stem == "index" || stem == "mod" {
        write.rsplit('/').nth(1)?
    } else {
        stem
    };
    (!stem.is_empty()).then_some((stem, ext))
}

fn is_test_file(runner: &Runner, path: &str) -> bool {
    let name = file_name(path);
    match runner {
        Runner::Unittest { .. } => name.starts_with("test") && name.ends_with(".py"),
        Runner::Pytest => {
            name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        }
        Runner::Node { .. } => {
            let (stem, ext) = split_ext(name);
            JS_EXTS.contains(&ext)
                && (stem.ends_with(".test")
                    || stem.ends_with(".spec")
                    || path
                        .split('/')
                        .any(|d| d == "test" || d == "tests" || d == "__tests__"))
        }
        Runner::Cargo { .. } => false,
    }
}

/// The test files of `tests` that belong to `write`.
fn tests_for_write(runner: &Runner, tests: &[String], write: &str) -> Vec<String> {
    let write = write.trim_start_matches("./");
    if tests.iter().any(|t| t == write) && is_test_file(runner, write) {
        return vec![write.to_owned()];
    }
    let Some((stem, ext)) = write_stem(write) else {
        return Vec::new();
    };
    let names: Vec<String> = match runner {
        Runner::Unittest { .. } if ext == "py" => vec![format!("test_{stem}.py")],
        Runner::Pytest if ext == "py" => vec![format!("test_{stem}.py"), format!("{stem}_test.py")],
        Runner::Node { .. } if JS_EXTS.contains(&ext) => JS_EXTS
            .iter()
            .flat_map(|e| {
                [
                    format!("{stem}.test.{e}"),
                    format!("{stem}.spec.{e}"),
                    format!("{stem}.{e}"),
                ]
            })
            .collect(),
        _ => return Vec::new(),
    };
    tests
        .iter()
        .filter(|t| {
            let n = file_name(t);
            names.iter().any(|want| want == n)
                && is_test_file(runner, t)
                // `<stem>.<ext>` counts only inside a test directory, and is
                // never the write itself.
                && t.as_str() != write
        })
        .cloned()
        .collect()
}

fn file_part(runner: &Runner, tokens: &[&str], test: &str) -> String {
    match runner {
        Runner::Unittest { .. } => {
            let mut t: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();
            if t.len() == 3 {
                // Bare `python -m unittest`: discovery with its defaults.
                t.push("discover".into());
            }
            t.push("-p".into());
            t.push(file_name(test).to_owned());
            t.join(" ")
        }
        Runner::Node { npm: true } => format!("{} -- {test}", tokens.join(" ")),
        _ => format!("{} {test}", tokens.join(" ")),
    }
}

// -- cargo packages ------------------------------------------------------------------------

fn cargo_part(base: &[&str], package: &str) -> String {
    // `-p` goes right after the subcommand, before any `--`.
    let mut out: Vec<String> = Vec::new();
    let sub = if base.get(1) == Some(&"nextest") {
        3
    } else {
        2
    };
    out.extend(base.iter().take(sub).map(|s| s.to_string()));
    out.push("-p".into());
    out.push(package.to_owned());
    out.extend(base.iter().skip(sub).map(|s| s.to_string()));
    out.join(" ")
}

/// The package name in the manifest at `dir`, if it has one.
fn package_name(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let doc: toml::Table = text.parse().ok()?;
    doc.get("package")?.get("name")?.as_str().map(str::to_owned)
}

/// The package `write` belongs to: the nearest manifest with a `[package]`
/// from the file's directory up to `root`.
fn cargo_package_of(root: &Path, write: &str) -> Option<String> {
    let mut dir: PathBuf = Path::new(write.trim_start_matches("./"))
        .parent()?
        .to_path_buf();
    loop {
        if let Some(name) = package_name(&root.join(&dir)) {
            return Some(name);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Every package under `root` with a manifest, for [`variants`].
fn cargo_packages(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        if let Some(name) = package_name(&dir) {
            out.insert(name);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_FILES {
                return out;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.file_type().is_ok_and(|k| k.is_dir())
                && !SKIP_DIRS.contains(&name.as_ref())
                && !name.starts_with('.')
            {
                stack.push(entry.path());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("dir");
        for (path, text) in files {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, text).expect("write");
        }
        dir
    }

    fn narrowed(scope: &Scope) -> (Vec<String>, Vec<String>) {
        match scope {
            Scope::Narrowed { parts, selected } => (parts.clone(), selected.clone()),
            Scope::Whole { why } => panic!("expected a narrowed scope, got whole: {why}"),
        }
    }

    const PY: &[(&str, &str)] = &[
        ("textkit/__init__.py", ""),
        ("textkit/slug.py", ""),
        ("textkit/counts.py", ""),
        ("textkit/ranking.py", ""),
        ("tests/test_slug.py", ""),
        ("tests/test_counts.py", ""),
        ("tests/test_ranking.py", ""),
    ];

    #[test]
    fn unittest_discover_runs_the_test_module_of_each_declared_file() {
        let t = tree(PY);
        let cmd = "python3 -m unittest discover -s tests";
        let s = scope(cmd, t.path(), &["textkit/slug.py"]);
        let (parts, selected) = narrowed(&s);
        assert_eq!(
            parts,
            ["python3 -m unittest discover -s tests -p test_slug.py"]
        );
        assert_eq!(selected, ["tests/test_slug.py"]);
        let s = scope(cmd, t.path(), &["textkit/counts.py", "textkit/ranking.py"]);
        assert_eq!(
            s.command(cmd),
            "python3 -m unittest discover -s tests -p test_counts.py && \
             python3 -m unittest discover -s tests -p test_ranking.py"
        );
        assert!(narrowable(cmd, t.path()));
        let v = variants(cmd, t.path());
        assert_eq!(v.len(), 3, "{v:?}");
        assert!(v.contains(&parts[0]));
    }

    #[test]
    fn a_file_with_no_test_of_its_own_keeps_the_whole_command() {
        let t = tree(PY);
        let s = scope(
            "python3 -m unittest discover -s tests",
            t.path(),
            &["textkit/__init__.py"],
        );
        // `__init__.py` names its package `textkit`; there is no test_textkit.py.
        assert!(matches!(s, Scope::Whole { .. }), "{s:?}");
        let s = scope(
            "python3 -m unittest discover -s tests",
            t.path(),
            &["README.md"],
        );
        assert!(matches!(s, Scope::Whole { .. }), "{s:?}");
    }

    #[test]
    fn a_command_that_already_selects_or_uses_the_shell_is_left_whole() {
        let t = tree(PY);
        for cmd in [
            "python3 -m unittest discover -s tests -p test_x.py",
            "python3 -m unittest tests.test_slug",
            "python3 -m pytest tests/test_slug.py",
            "python3 -m pytest -k slug",
            "sh check.sh",
            "make test",
            "python3 -m unittest discover -s tests && echo ok",
            "pytest 'tests'",
        ] {
            assert!(!narrowable(cmd, t.path()), "{cmd}");
            assert!(
                matches!(
                    scope(cmd, t.path(), &["textkit/slug.py"]),
                    Scope::Whole { .. }
                ),
                "{cmd}"
            );
        }
    }

    #[test]
    fn pytest_appends_the_test_path_and_finds_both_naming_styles() {
        let t = tree(&[
            ("pkg/a.py", ""),
            ("pkg/b.py", ""),
            ("tests/test_a.py", ""),
            ("pkg/b_test.py", ""),
        ]);
        let s = scope("python3 -m pytest -q", t.path(), &["pkg/a.py", "pkg/b.py"]);
        let (parts, _) = narrowed(&s);
        assert_eq!(
            parts,
            [
                "python3 -m pytest -q pkg/b_test.py",
                "python3 -m pytest -q tests/test_a.py"
            ]
        );
        // A declared test file selects itself.
        let s = scope("pytest", t.path(), &["tests/test_a.py"]);
        assert_eq!(narrowed(&s).0, ["pytest tests/test_a.py"]);
    }

    const TS: &[(&str, &str)] = &[
        ("package.json", r#"{"scripts": {"test": "node --test"}}"#),
        ("src/chunk.ts", ""),
        ("src/clamp.ts", ""),
        ("src/groupby.ts", ""),
        ("test/chunk.test.ts", ""),
        ("test/clamp.test.ts", ""),
        ("test/groupby.test.ts", ""),
    ];

    #[test]
    fn npm_test_over_node_test_passes_the_test_file_after_the_separator() {
        let t = tree(TS);
        let s = scope("npm test", t.path(), &["src/chunk.ts"]);
        let (parts, selected) = narrowed(&s);
        assert_eq!(parts, ["npm test -- test/chunk.test.ts"]);
        assert_eq!(selected, ["test/chunk.test.ts"]);
        assert_eq!(
            narrowed(&scope("node --test", t.path(), &["src/clamp.ts"])).0,
            ["node --test test/clamp.test.ts"]
        );
        assert_eq!(variants("npm test", t.path()).len(), 3);
    }

    #[test]
    fn npm_test_with_a_script_kgraf_cannot_narrow_is_left_whole() {
        let t = tree(&[
            (
                "package.json",
                r#"{"scripts": {"test": "tsc && node --test"}}"#,
            ),
            ("src/chunk.ts", ""),
            ("test/chunk.test.ts", ""),
        ]);
        assert!(matches!(
            scope("npm test", t.path(), &["src/chunk.ts"]),
            Scope::Whole { .. }
        ));
        let t = tree(&[
            ("package.json", r#"{"scripts": {"test": "mocha"}}"#),
            ("src/chunk.ts", ""),
        ]);
        assert!(!narrowable("npm test", t.path()));
    }

    const RS: &[(&str, &str)] = &[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n"),
        ("crates/alpha/src/lib.rs", ""),
        (
            "crates/beta/Cargo.toml",
            "[package]\nname = \"beta-core\"\n",
        ),
        ("crates/beta/src/lib.rs", ""),
        ("crates/beta/src/deep/mod.rs", ""),
    ];

    #[test]
    fn cargo_test_over_a_workspace_runs_the_packages_of_the_declared_files() {
        let t = tree(RS);
        let cmd = "cargo test --workspace -- --nocapture";
        let s = scope(cmd, t.path(), &["crates/beta/src/deep/mod.rs"]);
        assert_eq!(narrowed(&s).0, ["cargo test -p beta-core -- --nocapture"]);
        let s = scope(
            cmd,
            t.path(),
            &["crates/alpha/src/lib.rs", "crates/beta/src/lib.rs"],
        );
        assert_eq!(
            s.command(cmd),
            "cargo test -p alpha -- --nocapture && cargo test -p beta-core -- --nocapture"
        );
        // A virtual manifest's plain `cargo test` covers every member too.
        assert_eq!(
            narrowed(&scope("cargo test", t.path(), &["crates/alpha/src/lib.rs"])).0,
            ["cargo test -p alpha"]
        );
        let mut v = variants(cmd, t.path());
        v.sort();
        assert_eq!(
            v,
            [
                "cargo test -p alpha -- --nocapture",
                "cargo test -p beta-core -- --nocapture"
            ]
        );
        for whole in [
            "cargo test -p alpha",
            "cargo test --workspace my_filter",
            "cargo build --workspace",
        ] {
            assert!(!narrowable(whole, t.path()), "{whole}");
        }
    }

    #[test]
    fn cargo_test_in_a_single_package_is_already_narrow() {
        let t = tree(&[
            ("Cargo.toml", "[package]\nname = \"one\"\n"),
            ("src/lib.rs", ""),
        ]);
        assert!(!narrowable("cargo test", t.path()));
    }

    #[test]
    fn a_narrowable_acceptance_command_becomes_a_root_gate_once() {
        let t = tree(PY);
        let suite = "python3 -m unittest discover -s tests";
        assert_eq!(
            with_whole_gate(Vec::new(), suite, t.path()),
            (vec![suite.to_owned()], true)
        );
        assert_eq!(
            with_whole_gate(vec![suite.to_owned()], suite, t.path()),
            (vec![suite.to_owned()], false)
        );
        assert_eq!(
            with_whole_gate(vec!["python3 -m pytest".into()], suite, t.path()),
            (vec!["python3 -m pytest".to_owned(), suite.to_owned()], true)
        );
        // Not narrowed, so each contract already ran it whole: no gate added.
        assert_eq!(
            with_whole_gate(Vec::new(), "sh check.sh", t.path()),
            (Vec::<String>::new(), false)
        );
    }

    #[test]
    fn the_brief_names_the_exact_command_and_the_whole_one() {
        let t = tree(PY);
        let whole = "python3 -m unittest discover -s tests";
        let s = scope(whole, t.path(), &["textkit/slug.py"]);
        let text = brief(&s, whole).expect("narrowed");
        assert!(
            text.contains("`python3 -m unittest discover -s tests -p test_slug.py`"),
            "{text}"
        );
        assert!(text.contains(whole), "{text}");
    }

    #[test]
    fn a_whole_check_brief_names_the_command_and_calls_acceptance_advisory() {
        let text = brief(
            &Scope::Whole { why: String::new() },
            "cargo build --workspace",
        )
        .expect("a whole check is stated too");
        assert!(text.contains("`cargo build --workspace` passes"), "{text}");
        assert!(text.contains("advisory"), "{text}");
        assert!(
            text.contains(
                "`todo!()` panics or failing tests in other contracts' unfinished code are \
                 not yours to fix; a compile error anywhere that your change caused (for \
                 example by changing a public type's shape) is yours."
            ),
            "{text}"
        );
        assert!(
            !text.contains("Failures in other contracts"),
            "a blanket exemption for other contracts' failures must not return: {text}"
        );
    }
}
