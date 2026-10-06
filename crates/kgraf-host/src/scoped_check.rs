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
//!   declared file is in, selected with `-p <package>`;
//! - `go test ./...` with options only, `go.mod` at the root: the package of
//!   each declared `.go` file (`go test ./<dir>`); a file in a nested module
//!   is not narrowed;
//! - `rspec` / `bundle exec rspec` with options only: `<stem>_spec.rb`;
//! - `mvn` / `./mvnw` with a test goal and no selection: the test classes
//!   `<Stem>Test`, `<Stem>Tests`, `<Stem>IT`, `Test<Stem>` under
//!   `src/test/`, selected with `-Dtest=<Class>` (and `-pl <module> -am` when
//!   the root `pom.xml` lists the module);
//! - `gradle test` / `./gradlew test` with no selection: the same classes,
//!   selected with `:<project>:test --tests <fully.qualified.Class>` when
//!   `settings.gradle` names the project;
//! - `npm test --workspaces`, or `npm test` whose script is that: the
//!   workspace each declared file is in, `npm test --workspace=<dir>`, when
//!   that workspace has a `test` script (or `--if-present` is given).
//!
//! Each narrowed form runs only what a part of the whole command runs, so it
//! cannot fail where the whole command, on the same tree, would pass for the
//! same tests.
//!
//! A declared file that is itself such a test selects itself. One command runs
//! per test file or package, joined with `&&`, so each part is a command the
//! worker may also run (see [`variants`]).
//!
//! **When the whole command runs.** An unrecognized command (a script, `make`,
//! anything with shell syntax or quotes), a command that already selects tests
//! (a path, a `-p` pattern, a package), or a write set with no test of its own:
//! the whole command runs, as before, and the reason is kept. A whole command
//! may then fail only because other open contracts of the plan are not merged;
//! the verifier defers it to the root gate when it fails at the dispatch base
//! too (`verifier.rs`, decision 0018 amended 2026-10-02). The acceptance
//! command is a root gate in every case ([`with_whole_gate`]).
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
    /// `go test ./...` with options only, in a module whose `go.mod` is at
    /// the root: narrowed to the packages of the contract's files.
    Go,
    /// `rspec` or `bundle exec rspec`, options only: `<stem>_spec.rb`.
    Rspec,
    /// `mvn` (or `./mvnw`) with a goal that runs tests and no selection:
    /// narrowed to the test classes of the contract's files.
    Maven,
    /// `gradle test` (or `./gradlew test`) with no selection: narrowed to
    /// `:<project>:test --tests <class>` per test class.
    Gradle,
    /// `npm test --workspaces`, or `npm test` whose script is that: narrowed
    /// to `npm test --workspace=<dir>` per workspace of the contract's files.
    /// `extra` is the options kept (`--if-present`).
    NpmWorkspaces { extra: Vec<String> },
}

/// Value options of `go test` that take the next token.
const GO_VALUE_OPTS: &[&str] = &[
    "-timeout",
    "-count",
    "-p",
    "-parallel",
    "-cpu",
    "-tags",
    "-covermode",
    "-coverprofile",
    "-coverpkg",
];

/// Maven goals that run the tests.
const MAVEN_TEST_GOALS: &[&str] = &["test", "verify", "install", "package"];

/// Extensions of JVM test sources Maven and Gradle compile.
const JVM_EXTS: &[&str] = &["java", "kt", "groovy", "scala"];

/// Whether `command`, run in `root`, is a whole-suite command this module can
/// narrow.
pub(crate) fn narrowable(command: &str, root: &Path) -> bool {
    runner(command, root).is_ok()
}

/// `gates` with `verify` appended when it is not blank and not already one of
/// them, and whether it was appended. The whole acceptance command then judges
/// the merged result: a narrowed per-contract check ran only its own tests,
/// and a whole one may have been deferred to this gate for a contract whose
/// tree lacked other contracts' work (decision 0018, amended 2026-10-02).
pub(crate) fn with_whole_gate(mut gates: Vec<String>, verify: &str) -> (Vec<String>, bool) {
    let verify = verify.trim();
    let present = gates.iter().any(|g| g.trim() == verify);
    if present || verify.is_empty() {
        return (gates, false);
    }
    gates.push(verify.to_owned());
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
        Runner::Go => {
            let packages: BTreeSet<String> = writes
                .iter()
                .filter_map(|w| go_package_of(root, w))
                .collect();
            if packages.is_empty() {
                return Scope::Whole {
                    why: "none of the contract's files is a Go file of the root module".into(),
                };
            }
            Scope::Narrowed {
                parts: packages.iter().map(|p| go_part(&tokens, p)).collect(),
                selected: packages
                    .into_iter()
                    .map(|p| format!("package {p}"))
                    .collect(),
            }
        }
        Runner::NpmWorkspaces { extra } => {
            let dirs = workspace_dirs(root);
            let mut chosen: BTreeSet<&str> = BTreeSet::new();
            for w in writes {
                let w = w.trim_start_matches("./");
                if let Some(d) = dirs
                    .iter()
                    .filter(|d| w.starts_with(&format!("{d}/")))
                    .max_by_key(|d| d.len())
                {
                    chosen.insert(d);
                }
            }
            // A workspace with no `test` script fails `npm test --workspace`
            // unless the command allows it (`--if-present`).
            let if_present = extra.iter().any(|a| a == "--if-present");
            chosen.retain(|d| if_present || has_test_script(root, d));
            if chosen.is_empty() {
                return Scope::Whole {
                    why: "none of the contract's files is in a workspace with a test script".into(),
                };
            }
            Scope::Narrowed {
                parts: chosen.iter().map(|d| workspace_part(extra, d)).collect(),
                selected: chosen.iter().map(|d| format!("workspace {d}")).collect(),
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
            let parts: Vec<(String, String)> = selected
                .into_iter()
                .filter_map(|t| file_part(&runner, &tokens, &t, root).map(|p| (p, t)))
                .collect();
            if parts.is_empty() {
                return Scope::Whole {
                    why: "no test file belongs to the contract's files by the runner's naming"
                        .into(),
                };
            }
            let (parts, selected) = parts.into_iter().unzip();
            Scope::Narrowed { parts, selected }
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
        Runner::Go => test_files(&runner, root)
            .unwrap_or_default()
            .iter()
            .filter(|t| t.ends_with("_test.go"))
            .filter_map(|t| go_package_of(root, t))
            .map(|p| go_part(&tokens, &p))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        Runner::NpmWorkspaces { extra } => workspace_dirs(root)
            .iter()
            .map(|d| workspace_part(extra, d))
            .collect(),
        _ => test_files(&runner, root)
            .unwrap_or_default()
            .iter()
            .filter(|t| is_test_file(&runner, t))
            .filter_map(|t| file_part(&runner, &tokens, t, root))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    }
}

/// The Go package of `write` as `go test` names it from the root (`./dir`,
/// `.` for the root), when it is a `.go` file of the root module: no `go.mod`
/// between its directory and the root, since a nested module is not a package
/// of the root module and `go test ./dir` would fail there.
fn go_package_of(root: &Path, write: &str) -> Option<String> {
    let write = write.trim_start_matches("./");
    if !write.ends_with(".go") {
        return None;
    }
    let dir = write.rsplit_once('/').map_or("", |(d, _)| d);
    let mut up = PathBuf::from(dir);
    while !up.as_os_str().is_empty() {
        if root.join(&up).join("go.mod").is_file() {
            return None;
        }
        if !up.pop() {
            break;
        }
    }
    Some(if dir.is_empty() {
        ".".to_owned()
    } else {
        format!("./{dir}")
    })
}

fn go_part(tokens: &[&str], package: &str) -> String {
    tokens
        .iter()
        .map(|t| if *t == "./..." { package } else { t })
        .collect::<Vec<_>>()
        .join(" ")
}

fn workspace_part(extra: &[String], dir: &str) -> String {
    let mut out = vec![
        "npm".to_owned(),
        "test".to_owned(),
        format!("--workspace={dir}"),
    ];
    out.extend(extra.iter().cloned());
    out.join(" ")
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
        ["npm", "test" | "t", rest @ ..] | ["npm", "run", "test", rest @ ..] => {
            npm_workspaces(rest, root)
        }
        ["cargo", "test", rest @ ..] | ["cargo", "nextest", "run", rest @ ..] => cargo(rest, root),
        ["go", "test", rest @ ..] => go_test(rest, root),
        ["rspec", rest @ ..] | ["bundle", "exec", "rspec", rest @ ..] => rspec(rest),
        [tool, rest @ ..] if is_tool(tool, "mvn") || is_tool(tool, "mvnw") => maven(rest, root),
        [tool, rest @ ..] if is_tool(tool, "gradle") || is_tool(tool, "gradlew") => {
            gradle(rest, root)
        }
        _ => Err("the command is not a test runner kGraf can narrow to a contract's files".into()),
    }
}

/// Whether `token` runs `name`: the bare name or a relative path to it
/// (`./mvnw`).
fn is_tool(token: &str, name: &str) -> bool {
    token == name || token.strip_prefix("./") == Some(name)
}

fn rspec(rest: &[&str]) -> Result<Runner, String> {
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        let flag = a.split_once('=').map_or(a, |(f, _)| f);
        if matches!(
            flag,
            "-e" | "--example" | "-t" | "--tag" | "-P" | "--pattern"
        ) {
            return Err("the rspec command already selects its examples".into());
        }
        if matches!(
            a,
            "-f" | "--format" | "-r" | "--require" | "-o" | "--out" | "--seed" | "--order"
        ) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Err("the rspec command already names what it runs".into());
        }
    }
    Ok(Runner::Rspec)
}

fn go_test(rest: &[&str], root: &Path) -> Result<Runner, String> {
    let mut all = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        let flag = a.split_once('=').map_or(a, |(f, _)| f);
        if matches!(flag, "-run" | "-skip" | "-bench" | "-list" | "-fuzz") {
            return Err("the go test command already selects its tests".into());
        }
        if a == "./..." {
            all = true;
            i += 1;
        } else if GO_VALUE_OPTS.contains(&a) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Err("the go test command already names its packages".into());
        }
    }
    if !all {
        return Err("the go test command does not test every package (`./...`)".into());
    }
    if !root.join("go.mod").is_file() {
        return Err("no go.mod at the repository root".into());
    }
    Ok(Runner::Go)
}

fn maven(rest: &[&str], root: &Path) -> Result<Runner, String> {
    let selects = |a: &&str| {
        matches!(
            *a,
            "-pl" | "--projects" | "-f" | "--file" | "-rf" | "--resume-from"
        ) || a.starts_with("-Dtest=")
            || a.starts_with("-Dit.test=")
    };
    if rest.iter().any(selects) {
        return Err("the maven command already selects its modules or tests".into());
    }
    if !rest.iter().any(|a| MAVEN_TEST_GOALS.contains(a)) {
        return Err("the maven command runs no goal that runs the tests".into());
    }
    if rest
        .iter()
        .any(|a| !a.starts_with('-') && !MAVEN_TEST_GOALS.contains(a) && *a != "clean")
    {
        return Err("the maven command runs a goal kGraf does not narrow".into());
    }
    if !root.join("pom.xml").is_file() {
        return Err("no pom.xml at the repository root".into());
    }
    Ok(Runner::Maven)
}

fn gradle(rest: &[&str], root: &Path) -> Result<Runner, String> {
    if rest
        .iter()
        .any(|a| *a == "--tests" || a.starts_with("--tests=") || a.contains(':'))
    {
        return Err("the gradle command already selects its projects or tests".into());
    }
    let tasks: Vec<&&str> = rest.iter().filter(|a| !a.starts_with('-')).collect();
    if tasks.as_slice() != [&"test"] {
        return Err("the gradle command runs tasks other than `test`".into());
    }
    if rest
        .iter()
        .any(|a| a.starts_with('-') && !a.starts_with("--"))
    {
        // A short option may take a value (`-x check`, `-P key`), which would
        // read as a task.
        return Err("the gradle command has short options kGraf does not read".into());
    }
    let settings = gradle_settings(root);
    if settings.contains("projectDir") {
        return Err("settings.gradle moves a project's directory".into());
    }
    Ok(Runner::Gradle)
}

/// The text of `settings.gradle` or `settings.gradle.kts`; empty when there
/// is none (a single-project build).
fn gradle_settings(root: &Path) -> String {
    ["settings.gradle", "settings.gradle.kts"]
        .iter()
        .find_map(|n| std::fs::read_to_string(root.join(n)).ok())
        .unwrap_or_default()
}

/// `npm test --workspaces` (or `-ws`), with options only.
fn npm_workspaces(rest: &[&str], root: &Path) -> Result<Runner, String> {
    if !rest.iter().any(|a| *a == "--workspaces" || *a == "-ws") {
        return Err("the npm command is not a test runner kGraf can narrow".into());
    }
    options_only(rest)?;
    if rest
        .iter()
        .any(|a| *a == "--workspace" || a.starts_with("--workspace=") || *a == "-w")
    {
        return Err("the npm command already selects its workspaces".into());
    }
    if workspace_dirs(root).is_empty() {
        return Err("package.json lists no workspace directory".into());
    }
    Ok(Runner::NpmWorkspaces {
        extra: rest
            .iter()
            .filter(|a| **a != "--workspaces" && **a != "-ws")
            .map(|a| a.to_string())
            .collect(),
    })
}

/// The workspace directories the root `package.json` lists that exist and
/// hold a `package.json`: each explicit entry, and each directory under a
/// `<prefix>/*` entry. Other globs are not expanded.
fn workspace_dirs(root: &Path) -> Vec<String> {
    let Some(json) = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return Vec::new();
    };
    let list = json.get("workspaces").and_then(|w| {
        w.as_array()
            .or_else(|| w.get("packages").and_then(|p| p.as_array()))
    });
    let mut out: BTreeSet<String> = BTreeSet::new();
    for entry in list.into_iter().flatten().filter_map(|e| e.as_str()) {
        let entry = entry.trim_start_matches("./").trim_end_matches('/');
        if let Some(prefix) = entry.strip_suffix("/*") {
            if prefix.contains('*') {
                continue;
            }
            if let Ok(dirs) = std::fs::read_dir(root.join(prefix)) {
                for d in dirs.flatten() {
                    out.insert(format!("{prefix}/{}", d.file_name().to_string_lossy()));
                }
            }
        } else if !entry.contains('*') {
            out.insert(entry.to_owned());
        }
    }
    out.into_iter()
        .filter(|d| root.join(d).join("package.json").is_file())
        .collect()
}

/// Whether the workspace at `dir` has a `test` script.
fn has_test_script(root: &Path, dir: &str) -> bool {
    std::fs::read_to_string(root.join(dir).join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|j| j.get("scripts").and_then(|s| s.get("test")).is_some())
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
        ["npm", "test" | "t", rest @ ..] | ["npm", "run", "test", rest @ ..] => {
            return npm_workspaces(rest, root);
        }
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
        Runner::Rspec => name.ends_with("_spec.rb"),
        Runner::Maven | Runner::Gradle => {
            let (stem, ext) = split_ext(name);
            JVM_EXTS.contains(&ext)
                && jvm_test_split(path).is_some()
                && (stem.ends_with("Test")
                    || stem.ends_with("Tests")
                    || stem.ends_with("IT")
                    || stem.starts_with("Test"))
        }
        Runner::Cargo { .. } | Runner::Go | Runner::NpmWorkspaces { .. } => false,
    }
}

/// A JVM test source split at its `src/test/<language>/` directory: the
/// module directory before it (`""` for the root project) and the class path
/// after it (`shop/CartTest.java`).
fn jvm_test_split(path: &str) -> Option<(&str, &str)> {
    let (module, rest) = if let Some(rest) = path.strip_prefix("src/test/") {
        ("", rest)
    } else {
        let at = path.find("/src/test/")?;
        (&path[..at], &path[at + "/src/test/".len()..])
    };
    let (_, class) = rest.split_once('/')?;
    Some((module, class))
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
        Runner::Rspec if ext == "rb" => vec![format!("{stem}_spec.rb")],
        Runner::Maven | Runner::Gradle if JVM_EXTS.contains(&ext) => JVM_EXTS
            .iter()
            .flat_map(|e| {
                [
                    format!("{stem}Test.{e}"),
                    format!("{stem}Tests.{e}"),
                    format!("{stem}IT.{e}"),
                    format!("Test{stem}.{e}"),
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

/// The command that runs `test` alone, or `None` when the runner cannot
/// select it from the root (a Gradle project kGraf cannot name).
fn file_part(runner: &Runner, tokens: &[&str], test: &str, root: &Path) -> Option<String> {
    Some(match runner {
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
        Runner::Maven => maven_part(tokens, test, root)?,
        Runner::Gradle => gradle_part(tokens, test, root)?,
        _ => format!("{} {test}", tokens.join(" ")),
    })
}

/// `mvn ... -Dtest=<Class> -Dsurefire.failIfNoSpecifiedTests=false`, with
/// `-pl <module> -am` when the root `pom.xml` lists the module. `-am` builds
/// the modules it depends on, which a module alone cannot resolve until they
/// are installed; the test filter keeps their tests from running, and the
/// surefire flag keeps a module with no matching test from failing.
fn maven_part(tokens: &[&str], test: &str, root: &Path) -> Option<String> {
    let (module, class) = jvm_test_split(test)?;
    let (class, _) = split_ext(file_name(class));
    let mut out: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
    if !module.is_empty() {
        let pom = std::fs::read_to_string(root.join("pom.xml")).unwrap_or_default();
        if pom.contains(&format!("<module>{module}</module>")) {
            out.extend(["-pl".to_owned(), module.to_owned(), "-am".to_owned()]);
        }
    }
    out.push(format!("-Dtest={class}"));
    out.push("-Dsurefire.failIfNoSpecifiedTests=false".to_owned());
    Some(out.join(" "))
}

/// `gradle :<project>:test --tests <fully.qualified.Class>`: the project
/// path is the module directory with `/` as `:`, and must be named in
/// `settings.gradle` (a single-project build at the root needs none). A
/// project's `test` task builds what it depends on and runs no other
/// project's tests.
fn gradle_part(tokens: &[&str], test: &str, root: &Path) -> Option<String> {
    let (module, class) = jvm_test_split(test)?;
    let (class, _) = split_ext(class);
    let fqcn = class.replace('/', ".");
    let task = if module.is_empty() {
        "test".to_owned()
    } else {
        let project = module.replace('/', ":");
        let settings = gradle_settings(root);
        let named = [
            format!("'{project}'"),
            format!("\"{project}\""),
            format!("':{project}'"),
            format!("\":{project}\""),
        ];
        if !named.iter().any(|n| settings.contains(n)) {
            return None;
        }
        format!(":{project}:test")
    };
    let mut out: Vec<String> = tokens
        .iter()
        .map(|t| {
            if *t == "test" {
                task.clone()
            } else {
                t.to_string()
            }
        })
        .collect();
    out.push("--tests".to_owned());
    out.push(fqcn);
    Some(out.join(" "))
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
    fn the_acceptance_command_becomes_a_root_gate_once() {
        let suite = "python3 -m unittest discover -s tests";
        assert_eq!(
            with_whole_gate(Vec::new(), suite),
            (vec![suite.to_owned()], true)
        );
        assert_eq!(
            with_whole_gate(vec![suite.to_owned()], suite),
            (vec![suite.to_owned()], false)
        );
        assert_eq!(
            with_whole_gate(vec!["python3 -m pytest".into()], suite),
            (vec!["python3 -m pytest".to_owned(), suite.to_owned()], true)
        );
        // Not narrowed, it still judges the merged result: a contract may
        // have had it deferred to this gate.
        assert_eq!(
            with_whole_gate(Vec::new(), "sh check.sh"),
            (vec!["sh check.sh".to_owned()], true)
        );
        // A blank command checks nothing and is never a gate.
        assert_eq!(
            with_whole_gate(Vec::new(), "  "),
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

    /// `go test ./...` narrows to the packages of the contract's Go files; a
    /// file in a nested module, or a command that already selects, keeps
    /// the whole command.
    #[test]
    fn go_test_narrows_to_the_packages_of_the_contracts_files() {
        let t = tree(&[
            ("go.mod", "module example.com/shop\n"),
            ("price/price.go", ""),
            ("price/price_test.go", ""),
            ("cart/cart.go", ""),
            ("main.go", ""),
            ("tools/go.mod", "module example.com/tools\n"),
            ("tools/gen/gen.go", ""),
        ]);
        let cmd = "go test -count=1 ./...";
        let (parts, selected) = narrowed(&scope(cmd, t.path(), &["price/price.go", "main.go"]));
        assert_eq!(parts, ["go test -count=1 .", "go test -count=1 ./price"]);
        assert_eq!(selected, ["package .", "package ./price"]);
        assert!(matches!(
            scope(cmd, t.path(), &["tools/gen/gen.go"]),
            Scope::Whole { .. }
        ));
        assert!(!narrowable("go test -run TestX ./...", t.path()));
        assert!(!narrowable("go test ./price", t.path()));
        assert_eq!(variants(cmd, t.path()), ["go test -count=1 ./price"]);
        let bare = tree(&[("price/price.go", "")]);
        assert!(!narrowable(cmd, bare.path()), "no go.mod at the root");
    }

    #[test]
    fn rspec_narrows_to_the_spec_of_each_file() {
        let t = tree(&[
            ("lib/cart.rb", ""),
            ("lib/price.rb", ""),
            ("spec/cart_spec.rb", ""),
            ("spec/price_spec.rb", ""),
        ]);
        for cmd in ["rspec", "bundle exec rspec --format progress"] {
            let (parts, selected) = narrowed(&scope(cmd, t.path(), &["lib/cart.rb"]));
            assert_eq!(parts, [format!("{cmd} spec/cart_spec.rb")]);
            assert_eq!(selected, ["spec/cart_spec.rb"]);
        }
        assert!(!narrowable("rspec spec/cart_spec.rb", t.path()));
    }

    #[test]
    fn maven_narrows_to_the_test_classes_and_the_listed_module() {
        let t = tree(&[
            ("pom.xml", "<modules><module>core</module></modules>"),
            ("core/pom.xml", ""),
            ("core/src/main/java/shop/Cart.java", ""),
            ("core/src/test/java/shop/CartTest.java", ""),
            ("src/main/java/app/App.java", ""),
            ("src/test/java/app/AppTests.java", ""),
        ]);
        let cmd = "mvn -q test";
        let (parts, selected) = narrowed(&scope(
            cmd,
            t.path(),
            &[
                "core/src/main/java/shop/Cart.java",
                "src/main/java/app/App.java",
            ],
        ));
        assert_eq!(
            parts,
            [
                "mvn -q test -pl core -am -Dtest=CartTest -Dsurefire.failIfNoSpecifiedTests=false",
                "mvn -q test -Dtest=AppTests -Dsurefire.failIfNoSpecifiedTests=false"
            ]
        );
        assert_eq!(
            selected,
            [
                "core/src/test/java/shop/CartTest.java",
                "src/test/java/app/AppTests.java"
            ]
        );
        assert!(narrowable("./mvnw verify", t.path()));
        assert!(!narrowable("mvn -pl core test", t.path()));
        assert!(!narrowable("mvn test -Dtest=CartTest", t.path()));
        assert!(!narrowable("mvn compile", t.path()), "no test goal");
    }

    #[test]
    fn gradle_narrows_to_the_project_test_task_of_each_class() {
        let t = tree(&[
            ("settings.gradle", "include 'core'\n"),
            ("core/build.gradle", ""),
            ("core/src/main/kotlin/shop/Cart.kt", ""),
            ("core/src/test/kotlin/shop/CartTest.kt", ""),
            ("extra/src/main/java/x/Thing.java", ""),
            ("extra/src/test/java/x/ThingTest.java", ""),
        ]);
        let cmd = "./gradlew test";
        let (parts, _) = narrowed(&scope(
            cmd,
            t.path(),
            &["core/src/main/kotlin/shop/Cart.kt"],
        ));
        assert_eq!(parts, ["./gradlew :core:test --tests shop.CartTest"]);
        // A project settings.gradle does not name: never guessed.
        assert!(matches!(
            scope(cmd, t.path(), &["extra/src/main/java/x/Thing.java"]),
            Scope::Whole { .. }
        ));
        assert!(!narrowable("gradle check", t.path()));
        assert!(!narrowable("gradle :core:test", t.path()));
        assert!(!narrowable("gradle test --tests x", t.path()));
        let moved = tree(&[(
            "settings.gradle",
            "include 'core'\nproject(':core').projectDir = file('lib')\n",
        )]);
        assert!(!narrowable(cmd, moved.path()));
    }

    #[test]
    fn npm_workspaces_narrow_to_the_workspace_of_each_file() {
        let t = tree(&[
            (
                "package.json",
                r#"{"workspaces":["packages/*","tools"],"scripts":{"test":"npm test --workspaces"}}"#,
            ),
            (
                "packages/cart/package.json",
                r#"{"name":"cart","scripts":{"test":"node --test"}}"#,
            ),
            ("packages/cart/index.js", ""),
            ("packages/price/package.json", r#"{"name":"price"}"#),
            ("packages/price/index.js", ""),
            ("tools/package.json", r#"{"scripts":{"test":"x"}}"#),
        ]);
        for cmd in ["npm test", "npm test --workspaces"] {
            let (parts, selected) = narrowed(&scope(cmd, t.path(), &["packages/cart/index.js"]));
            assert_eq!(parts, ["npm test --workspace=packages/cart"], "{cmd}");
            assert_eq!(selected, ["workspace packages/cart"]);
            // A workspace with no test script would fail `--workspace`: kept whole.
            assert!(
                matches!(
                    scope(cmd, t.path(), &["packages/price/index.js"]),
                    Scope::Whole { .. }
                ),
                "{cmd}"
            );
        }
        let (parts, _) = narrowed(&scope(
            "npm test --workspaces --if-present",
            t.path(),
            &["packages/price/index.js"],
        ));
        assert_eq!(parts, ["npm test --workspace=packages/price --if-present"]);
        assert!(!narrowable("npm test --workspace=tools", t.path()));
    }
}
