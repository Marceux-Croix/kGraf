//! The build and test allowlist's commands for the other languages
//! (decision 0084, amended 2026-10-06): npm, pnpm and yarn scripts and the
//! common JavaScript test runners, pytest, the go command, Maven, Gradle and
//! the .NET CLI.
//!
//! **Why.** The allowlist named cargo only, so a worker on a JavaScript,
//! Python, Go, JVM or .NET project could not run its own tests without an
//! approval, and an unattended worker's approval is a denial. Its correct
//! work then failed verification it could never check first.
//!
//! **What the rules keep out.** Every install, publish, fetch and run-anything
//! subcommand is not listed (`npm install`, `npm publish`, `npm exec`,
//! `pnpm add`, `yarn dlx`, `go get`, `go run`, `mvn deploy`, a Maven plugin
//! goal such as `exec:java`, `dotnet add`, `dotnet run`), so it stays with the
//! command policy, which asks. Options that change what program runs, where
//! the tool reads its configuration or which network source it uses are not
//! listed either: npm's config options before `--` (`--script-shell`,
//! `--userconfig`, `--registry`), go's `-exec`, `-toolexec`, `-modfile` and
//! `-overlay`, Maven's `-s`, `-gs` and arbitrary `-D` properties, Gradle's
//! `-I`, `-D`, `-P` and `--project-dir`, the .NET CLI's `-p:` properties,
//! `--source` and `--output`. A path argument stays inside the worktree.
//!
//! **What they allow freely.** Arguments a test runner takes after its
//! script (`npm test -- tests/a.test.js`), pytest's own arguments, and a
//! test binary's arguments after go's `-args`: they select and report tests
//! of code the worker wrote, which runs anyway. As for cargo, the rules are
//! not the security boundary; the sandbox is (writes confined, network off,
//! kGraf's state unreadable), with the verifier's scope check behind it.
//!
//! Written new, as the cargo rules were: each tool's documented options,
//! read for this list.

use super::build::{Opt, check_options, filter_value, jobs_value, name_value, word_value};

/// A language's build and test commands, as one allowlist entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolCommand {
    /// `npm test`, `npm run <script>`, the same under pnpm and yarn, and
    /// `npx` with a test runner or compiler.
    NodeScripts,
    /// `pytest`, `python -m pytest`, `python -m unittest`.
    Pytest,
    /// `go build`, `go test`, `go vet`.
    Go,
    /// `mvn` with the build lifecycle's phases up to `verify`.
    Maven,
    /// `gradle` with the test and compile tasks.
    Gradle,
    /// `dotnet build`, `dotnet test`.
    Dotnet,
}

impl ToolCommand {
    /// The configuration spelling (`buildCommands` in `worker.json`).
    pub fn spelling(self) -> &'static str {
        match self {
            Self::NodeScripts => "npm test",
            Self::Pytest => "pytest",
            Self::Go => "go test",
            Self::Maven => "mvn test",
            Self::Gradle => "gradle test",
            Self::Dotnet => "dotnet test",
        }
    }

    /// What one entry covers, for a message or a tool description.
    pub fn summary(self) -> &'static str {
        match self {
            Self::NodeScripts => {
                "npm test, npm run SCRIPT (test, build, lint, check, typecheck, compile, \
                 coverage, or one starting test:, build:, lint:, check:, typecheck:), the \
                 same with pnpm and yarn, and npx jest|vitest|mocha|tsc|ava|eslint|tap|c8|nyc, \
                 with any arguments after the script's `--`; not install, ci, add, exec, \
                 publish or npm's config options"
            }
            Self::Pytest => "pytest, python -m pytest, python3 -m pytest, python -m unittest",
            Self::Go => {
                "go build, go test and go vet on packages in the worktree, with -v, -run, \
                 -count, -race, -short, -cover, -timeout, -tags, -bench and the like; not \
                 -exec, -toolexec, -modfile or -overlay"
            }
            Self::Maven => {
                "mvn clean|validate|compile|test-compile|test|verify with -o, -q, -B, -pl, \
                 -am, -T, -Dtest=NAME and -DskipTests; not install, deploy, plugin goals, \
                 -s or other -D properties"
            }
            Self::Gradle => {
                "gradle test|check|build|assemble|classes|testClasses|compileJava|\
                 compileTestJava|compileKotlin|compileTestKotlin|clean with --offline, -q, \
                 --tests, -x, --info, --stacktrace, --continue; not -I, -D, -P or \
                 --project-dir"
            }
            Self::Dotnet => {
                "dotnet build and dotnet test with -c, -f, --filter, --no-restore, \
                 --no-build, -v and a project in the worktree; not -p:, --source or --output"
            }
        }
    }

    pub(super) const ALL: [Self; 6] = [
        Self::NodeScripts,
        Self::Pytest,
        Self::Go,
        Self::Maven,
        Self::Gradle,
        Self::Dotnet,
    ];

    /// The entry a configuration spelling names.
    pub(super) fn parse(entry: &str) -> Option<Self> {
        let words: Vec<&str> = entry.split_whitespace().collect();
        Self::ALL
            .into_iter()
            .find(|t| t.spelling().split(' ').eq(words.iter().copied()))
    }
}

/// How an argv stands against the tool rules: `None` when it is no tool
/// command they know (the policy decides), else the entry it falls under
/// and whether its arguments pass.
pub(super) fn classify(argv: &[String]) -> Option<(ToolCommand, Result<(), String>)> {
    let program = argv.first()?.as_str();
    let rest = &argv[1..];
    match program {
        "npm" => node_script(rest, Manager::Npm).map(|r| (ToolCommand::NodeScripts, r)),
        "pnpm" => node_script(rest, Manager::Pnpm).map(|r| (ToolCommand::NodeScripts, r)),
        "yarn" => node_script(rest, Manager::Yarn).map(|r| (ToolCommand::NodeScripts, r)),
        "npx" => npx(rest).map(|r| (ToolCommand::NodeScripts, r)),
        "pytest" | "py.test" => Some((ToolCommand::Pytest, plain_args(rest))),
        "python" | "python3" => match rest {
            [m, module, args @ ..] if m == "-m" && (module == "pytest" || module == "unittest") => {
                Some((ToolCommand::Pytest, plain_args(args)))
            }
            _ => None,
        },
        "go" => go(rest).map(|r| (ToolCommand::Go, r)),
        "mvn" => Some((ToolCommand::Maven, maven(rest))),
        "gradle" => Some((ToolCommand::Gradle, gradle(rest))),
        "dotnet" => dotnet(rest).map(|r| (ToolCommand::Dotnet, r)),
        _ => None,
    }
}

/// No argument holds a control character (a terminal escape in a log).
fn plain_args(args: &[String]) -> Result<(), String> {
    match args.iter().find(|a| a.chars().any(char::is_control)) {
        Some(a) => Err(format!("{a:?} holds a control character")),
        None => Ok(()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Manager {
    Npm,
    Pnpm,
    Yarn,
}

/// The script names a worker may run: building, checking and testing.
fn script_allowed(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "test",
        "build",
        "lint",
        "check",
        "typecheck",
        "type-check",
        "compile",
        "coverage",
        "tsc",
    ];
    const PREFIXES: &[&str] = &["test:", "build:", "lint:", "check:", "typecheck:"];
    EXACT.contains(&name)
        || PREFIXES
            .iter()
            .any(|p| name.len() > p.len() && name.starts_with(p))
}

/// The manager's own options a script run may carry before its `--`.
fn manager_options(manager: Manager) -> &'static [(&'static str, Opt)] {
    match manager {
        Manager::Npm => &[
            ("--silent", Opt::Flag),
            ("-s", Opt::Flag),
            ("--if-present", Opt::Flag),
            ("--workspaces", Opt::Flag),
            ("-ws", Opt::Flag),
            ("--workspace", Opt::Value(name_value)),
            ("-w", Opt::Value(name_value)),
        ],
        Manager::Pnpm => &[
            ("--silent", Opt::Flag),
            ("-s", Opt::Flag),
            ("--if-present", Opt::Flag),
            ("--recursive", Opt::Flag),
            ("-r", Opt::Flag),
            ("--filter", Opt::Value(filter_value)),
            ("-F", Opt::Value(filter_value)),
        ],
        Manager::Yarn => &[("--silent", Opt::Flag), ("-s", Opt::Flag)],
    }
}

/// `npm test`, `npm run SCRIPT`, and the same under pnpm and yarn (whose
/// `pnpm SCRIPT` and `yarn SCRIPT` shorthand is accepted for an allowed
/// script name, none of which is a builtin). `None` for any other
/// subcommand.
fn node_script(args: &[String], manager: Manager) -> Option<Result<(), String>> {
    let (own, _passed) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    let word = own.first()?.as_str();
    let after = match word {
        "test" | "t" | "tst" if manager == Manager::Npm => &own[1..],
        "test" => &own[1..],
        "run" | "run-script" => {
            let script = own.get(1)?;
            if !script_allowed(script) {
                return None;
            }
            &own[2..]
        }
        script if manager != Manager::Npm && script_allowed(script) => &own[1..],
        _ => return None,
    };
    Some(check_options(
        after,
        &[manager_options(manager)],
        &[],
        0,
        filter_value,
    ))
}

/// `npx RUNNER ...` for a test runner or compiler that is a project
/// dependency. Any option before the runner (`--yes`, `--package`) is not
/// listed: those install.
fn npx(args: &[String]) -> Option<Result<(), String>> {
    const RUNNERS: &[&str] = &[
        "jest", "vitest", "mocha", "tsc", "ava", "eslint", "tap", "c8", "nyc",
    ];
    let runner = args.first()?;
    if !RUNNERS.contains(&runner.as_str()) {
        return None;
    }
    Some(plain_args(&args[1..]))
}

/// A relative path inside the worktree: no root, no `~`, no `..`.
fn worktree_path(v: &str) -> Result<(), String> {
    let path = std::path::Path::new(v);
    if v.is_empty() || path.is_absolute() || v.starts_with('~') {
        return Err("must be a path relative to the worktree".into());
    }
    if path.components().any(|c| {
        !matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err("must stay inside the worktree (no `..`)".into());
    }
    Ok(())
}

/// A go package pattern: a path inside the worktree (`./...`, `./pkg`) or an
/// import path (`example.com/m/pkg`), never an absolute path or `..`.
fn go_package(v: &str) -> Result<(), String> {
    if v.starts_with('.') {
        let trimmed = v.strip_suffix("/...").unwrap_or(v);
        return worktree_path(if trimmed == "..." { "." } else { trimmed });
    }
    if v.starts_with('/') || v.contains("..") && !v.ends_with("/...") || v.contains("/../") {
        return Err("expected a package of the worktree".into());
    }
    Ok(())
}

fn go_mod_value(v: &str) -> Result<(), String> {
    match v {
        "readonly" | "vendor" | "mod" => Ok(()),
        _ => Err("expected readonly, vendor or mod".into()),
    }
}

fn duration_value(v: &str) -> Result<(), String> {
    if !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        Ok(())
    } else {
        Err("expected a duration".into())
    }
}

/// `go build`, `go test`, `go vet`. Go spells long options with one dash
/// and takes `-name=value` too.
fn go(args: &[String]) -> Option<Result<(), String>> {
    const COMMON: &[(&str, Opt)] = &[
        ("-v", Opt::Flag),
        ("-x", Opt::Flag),
        ("-n", Opt::Flag),
        ("-a", Opt::Flag),
        ("-race", Opt::Flag),
        ("-trimpath", Opt::Flag),
        ("-tags", Opt::Value(filter_value)),
        ("-p", Opt::Value(jobs_value)),
        ("-mod", Opt::Value(go_mod_value)),
    ];
    const TEST: &[(&str, Opt)] = &[
        ("-short", Opt::Flag),
        ("-cover", Opt::Flag),
        ("-failfast", Opt::Flag),
        ("-json", Opt::Flag),
        ("-benchmem", Opt::Flag),
        ("-run", Opt::Value(filter_value)),
        ("-skip", Opt::Value(filter_value)),
        ("-bench", Opt::Value(filter_value)),
        ("-benchtime", Opt::Value(duration_value)),
        ("-count", Opt::Value(jobs_value)),
        ("-cpu", Opt::Value(filter_value)),
        ("-parallel", Opt::Value(jobs_value)),
        ("-timeout", Opt::Value(duration_value)),
        ("-shuffle", Opt::Value(word_value)),
        ("-covermode", Opt::Value(word_value)),
        ("-coverpkg", Opt::Value(filter_value)),
        ("-coverprofile", Opt::Value(worktree_path)),
    ];
    const BUILD: &[(&str, Opt)] = &[("-o", Opt::Value(worktree_path))];
    let (sub, rest) = args.split_first()?;
    let lists: Vec<&[(&str, Opt)]> = match sub.as_str() {
        "test" => vec![COMMON, TEST],
        "build" => vec![COMMON, BUILD],
        "vet" => vec![COMMON],
        _ => return None,
    };
    // `go test ... -args ARGS`: everything after goes to the test binary.
    let own = match rest.iter().position(|a| a == "-args") {
        Some(i) if sub == "test" => &rest[..i],
        _ => rest,
    };
    // `-name=value` as two words, the shape the shared checker reads.
    let mut split: Vec<String> = Vec::new();
    for a in own {
        match a.split_once('=') {
            Some((name, value)) if a.starts_with('-') && !a.starts_with("--") => {
                split.push(name.to_owned());
                split.push(value.to_owned());
            }
            _ => split.push(a.clone()),
        }
    }
    Some(check_options(&split, &lists, &[], usize::MAX, go_package))
}

/// The Maven properties a worker may set: test selection and skipping.
const MAVEN_PROPERTIES: &[&str] = &[
    "test",
    "it.test",
    "skipTests",
    "maven.test.skip",
    "surefire.failIfNoSpecifiedTests",
    "failIfNoTests",
];

fn maven_property(v: &str) -> Result<(), String> {
    let key = v.split_once('=').map_or(v, |(k, _)| k);
    if MAVEN_PROPERTIES.contains(&key) {
        Ok(())
    } else {
        Err(format!(
            "only these properties may be set: {}",
            MAVEN_PROPERTIES.join(", ")
        ))
    }
}

fn maven_phase(v: &str) -> Result<(), String> {
    match v {
        "clean" | "validate" | "compile" | "test-compile" | "test" | "verify" => Ok(()),
        _ => Err("only clean, validate, compile, test-compile, test and verify are allowed".into()),
    }
}

/// `mvn PHASE...` with the lifecycle up to `verify`; `install`, `deploy`
/// and plugin goals are refused by name.
fn maven(args: &[String]) -> Result<(), String> {
    const OPTIONS: &[(&str, Opt)] = &[
        ("-o", Opt::Flag),
        ("--offline", Opt::Flag),
        ("-q", Opt::Flag),
        ("--quiet", Opt::Flag),
        ("-B", Opt::Flag),
        ("--batch-mode", Opt::Flag),
        ("-e", Opt::Flag),
        ("--errors", Opt::Flag),
        ("-X", Opt::Flag),
        ("--debug", Opt::Flag),
        ("-ntp", Opt::Flag),
        ("--no-transfer-progress", Opt::Flag),
        ("-fae", Opt::Flag),
        ("--fail-at-end", Opt::Flag),
        ("-ff", Opt::Flag),
        ("--fail-fast", Opt::Flag),
        ("-am", Opt::Flag),
        ("--also-make", Opt::Flag),
        ("-amd", Opt::Flag),
        ("--also-make-dependents", Opt::Flag),
        ("-pl", Opt::Value(filter_value)),
        ("--projects", Opt::Value(filter_value)),
        ("-T", Opt::Value(word_value)),
        ("--threads", Opt::Value(word_value)),
        ("-f", Opt::Value(worktree_path)),
        ("--file", Opt::Value(worktree_path)),
        ("-D", Opt::Value(maven_property)),
    ];
    if !args.iter().any(|a| !a.starts_with('-')) {
        return Err("name a phase: compile, test or verify".into());
    }
    check_options(args, &[OPTIONS], &["-D", "-T"], usize::MAX, maven_phase)
}

fn gradle_task(v: &str) -> Result<(), String> {
    const TASKS: &[&str] = &[
        "test",
        "check",
        "build",
        "assemble",
        "classes",
        "testClasses",
        "compileJava",
        "compileTestJava",
        "compileKotlin",
        "compileTestKotlin",
        "clean",
    ];
    // `:sub:test` runs the task of one subproject.
    let last = v.rsplit(':').next().unwrap_or(v);
    if TASKS.contains(&last) {
        Ok(())
    } else {
        Err(format!(
            "only these tasks are allowed: {}",
            TASKS.join(", ")
        ))
    }
}

fn console_value(v: &str) -> Result<(), String> {
    match v {
        "plain" | "rich" | "auto" | "verbose" => Ok(()),
        _ => Err("expected plain, rich, auto or verbose".into()),
    }
}

/// `gradle TASK...` with the test and compile tasks.
fn gradle(args: &[String]) -> Result<(), String> {
    const OPTIONS: &[(&str, Opt)] = &[
        ("--offline", Opt::Flag),
        ("-q", Opt::Flag),
        ("--quiet", Opt::Flag),
        ("-i", Opt::Flag),
        ("--info", Opt::Flag),
        ("-s", Opt::Flag),
        ("--stacktrace", Opt::Flag),
        ("--continue", Opt::Flag),
        ("--parallel", Opt::Flag),
        ("--no-daemon", Opt::Flag),
        ("--build-cache", Opt::Flag),
        ("--rerun-tasks", Opt::Flag),
        ("--console", Opt::Value(console_value)),
        ("--warning-mode", Opt::Value(word_value)),
        ("--tests", Opt::Value(filter_value)),
        ("-x", Opt::Value(gradle_task)),
        ("--exclude-task", Opt::Value(gradle_task)),
    ];
    if !args.iter().any(|a| !a.starts_with('-')) {
        return Err("name a task: test, check or build".into());
    }
    check_options(args, &[OPTIONS], &[], usize::MAX, gradle_task)
}

/// A .NET project, solution or folder in the worktree.
fn dotnet_project(v: &str) -> Result<(), String> {
    worktree_path(v)
}

/// `dotnet build`, `dotnet test`.
fn dotnet(args: &[String]) -> Option<Result<(), String>> {
    const OPTIONS: &[(&str, Opt)] = &[
        ("--no-restore", Opt::Flag),
        ("--no-build", Opt::Flag),
        ("--nologo", Opt::Flag),
        ("-c", Opt::Value(word_value)),
        ("--configuration", Opt::Value(word_value)),
        ("-f", Opt::Value(filter_value)),
        ("--framework", Opt::Value(filter_value)),
        ("-v", Opt::Value(word_value)),
        ("--verbosity", Opt::Value(word_value)),
        ("--filter", Opt::Value(filter_value)),
    ];
    let (sub, rest) = args.split_first()?;
    if sub != "build" && sub != "test" {
        return None;
    }
    Some(check_options(rest, &[OPTIONS], &[], 1, dotnet_project))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[track_caller]
    fn allowed(words: &[&str]) {
        match classify(&argv(words)) {
            Some((_, Ok(()))) => {}
            other => panic!("{words:?}: {other:?}"),
        }
    }

    #[track_caller]
    fn refused(words: &[&str], needle: &str) {
        match classify(&argv(words)) {
            Some((_, Err(why))) => assert!(why.contains(needle), "{words:?}: {why}"),
            other => panic!("{words:?}: expected a refusal, got {other:?}"),
        }
    }

    #[track_caller]
    fn not_listed(words: &[&str]) {
        assert!(classify(&argv(words)).is_none(), "{words:?}");
    }

    #[test]
    fn node_scripts_run_but_installs_and_publishes_are_not_listed() {
        allowed(&["npm", "test"]);
        allowed(&["npm", "t"]);
        allowed(&["npm", "test", "--", "--runInBand", "tests/a.test.js"]);
        allowed(&["npm", "run", "build"]);
        allowed(&["npm", "run", "test:unit", "--silent"]);
        allowed(&["npm", "run", "lint", "-w", "web"]);
        allowed(&["pnpm", "test"]);
        allowed(&["pnpm", "build"]);
        allowed(&["pnpm", "run", "typecheck", "--filter", "web"]);
        allowed(&["yarn", "test"]);
        allowed(&["yarn", "lint"]);
        allowed(&["npx", "jest", "--ci", "src"]);
        allowed(&["npx", "tsc", "--noEmit"]);
        for words in [
            &["npm", "install"][..],
            &["npm", "ci"],
            &["npm", "publish"],
            &["npm", "exec", "jest"],
            &["npm", "run", "deploy"],
            &["npm", "run", "postinstall"],
            &["pnpm", "add", "left-pad"],
            &["pnpm", "install"],
            &["yarn", "add", "x"],
            &["yarn", "dlx", "x"],
            &["npx", "--yes", "jest"],
            &["npx", "cowsay"],
        ] {
            not_listed(words);
        }
        refused(
            &["npm", "test", "--script-shell=/tmp/sh"],
            "not on the allowlist",
        );
        refused(
            &["npm", "test", "--registry", "http://x"],
            "not on the allowlist",
        );
    }

    #[test]
    fn pytest_in_its_spellings() {
        allowed(&["pytest"]);
        allowed(&["pytest", "-q", "tests/test_a.py::test_b", "-k", "name"]);
        allowed(&["python", "-m", "pytest", "-x"]);
        allowed(&["python3", "-m", "pytest"]);
        allowed(&["python3", "-m", "unittest", "discover"]);
        not_listed(&["python3", "-c", "print(1)"]);
        not_listed(&["python3", "-m", "pip", "install", "x"]);
        not_listed(&["python3", "script.py"]);
    }

    #[test]
    fn go_builds_tests_and_vets_the_worktree_only() {
        allowed(&["go", "test", "./..."]);
        allowed(&[
            "go",
            "test",
            "-run",
            "TestX",
            "-count=1",
            "-race",
            "./pkg/...",
        ]);
        allowed(&["go", "test", "./...", "-args", "-anything", "goes"]);
        allowed(&["go", "build", "-o", "bin/app", "."]);
        allowed(&["go", "vet", "./..."]);
        allowed(&["go", "test", "example.com/m/pkg"]);
        refused(
            &["go", "test", "-exec", "/tmp/x", "./..."],
            "not on the allowlist",
        );
        refused(
            &["go", "build", "-toolexec=/tmp/x", "."],
            "not on the allowlist",
        );
        refused(
            &["go", "test", "-modfile=/tmp/go.mod"],
            "not on the allowlist",
        );
        // An absolute path needs a drive letter on Windows.
        let abs = |p: &str| {
            if cfg!(windows) {
                format!("C:{p}")
            } else {
                p.to_owned()
            }
        };
        refused(
            &["go", "build", "-o", &abs("/usr/local/bin/x")],
            "relative to the worktree",
        );
        refused(&["go", "test", "../other/..."], "inside the worktree");
        not_listed(&["go", "get", "x"]);
        not_listed(&["go", "run", "."]);
        not_listed(&["go", "mod", "download"]);
    }

    #[test]
    fn maven_runs_the_lifecycle_to_verify_and_nothing_further() {
        allowed(&["mvn", "test"]);
        allowed(&["mvn", "-o", "-q", "verify"]);
        allowed(&["mvn", "-Dtest=FooTest", "test", "-pl", "core", "-am"]);
        allowed(&["mvn", "clean", "compile", "-T", "4"]);
        refused(&["mvn", "install"], "only clean");
        refused(&["mvn", "deploy"], "only clean");
        refused(&["mvn", "exec:java"], "only clean");
        refused(&["mvn", "-s", "/tmp/s.xml", "test"], "not on the allowlist");
        refused(
            &["mvn", "-Dmaven.repo.local=/tmp/r", "test"],
            "only these properties",
        );
        refused(&["mvn", "-q"], "name a phase");
    }

    #[test]
    fn gradle_runs_test_and_compile_tasks() {
        allowed(&["gradle", "test"]);
        allowed(&["gradle", ":core:test", "--offline", "--tests", "a.B"]);
        allowed(&["gradle", "build", "-x", "test", "--console=plain"]);
        refused(&["gradle", "publish"], "only these tasks");
        refused(
            &["gradle", "-I", "init.gradle", "test"],
            "not on the allowlist",
        );
        refused(
            &["gradle", "-Dorg.gradle.x=1", "test"],
            "not on the allowlist",
        );
        refused(
            &["gradle", "--project-dir", "/x", "test"],
            "not on the allowlist",
        );
    }

    #[test]
    fn dotnet_builds_and_tests_a_project_in_the_worktree() {
        allowed(&["dotnet", "test"]);
        allowed(&["dotnet", "build", "src/App.csproj", "-c", "Release"]);
        allowed(&[
            "dotnet",
            "test",
            "--no-restore",
            "--filter",
            "Category=Unit",
        ]);
        refused(&["dotnet", "build", "-p:Foo=bar"], "not on the allowlist");
        refused(
            &["dotnet", "test", "--source", "http://x"],
            "not on the allowlist",
        );
        refused(
            &["dotnet", "build", "-o", "/tmp/out"],
            "not on the allowlist",
        );
        let abs = |p: &str| {
            if cfg!(windows) {
                format!("C:{p}")
            } else {
                p.to_owned()
            }
        };
        refused(
            &["dotnet", "build", &abs("/abs/App.csproj")],
            "relative to the worktree",
        );
        not_listed(&["dotnet", "run"]);
        not_listed(&["dotnet", "add", "package", "x"]);
    }

    #[test]
    fn configuration_spellings_parse() {
        for t in ToolCommand::ALL {
            assert_eq!(ToolCommand::parse(t.spelling()), Some(t));
        }
        assert_eq!(ToolCommand::parse("npm install"), None);
    }
}
