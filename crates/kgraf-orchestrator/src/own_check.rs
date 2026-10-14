//! Whether a contract's own check (decision 0100, `ContractSpec::check`)
//! tests anything: its exit status must be able to fail because a test
//! runner, a build, or an assertion over a file the contract writes failed.
//!
//! The command is read as `sh -c` reads it, as far as its exit status goes:
//! split at `;`, `&&`, `||`, `|`, `&` and newlines (quotes respected), then
//! folded left to right. `cargo test || true`, `cargo test; exit 0` and
//! `cargo test | tee log` (no `pipefail`) always pass, so they test nothing;
//! `cargo test && echo ok` fails when the tests fail, so it tests the work.
//! Syntax this reading does not follow (substitutions, subshells, groups,
//! `if`/`for`/`while`/`case`) is refused with that reason: the planner can
//! write plain commands. Novel code: the permission reader in `kgraf-tools`
//! lists a call's commands but drops the operators between them, which the
//! exit status depends on.

/// Programs that run tests or build code, with what a call of each must do
/// to count: `Sub` lists the subcommands that test or build (the first word
/// after the program that is not a flag); `Any` counts any call that is not
/// only a version or help request.
enum Runner {
    Sub(&'static [&'static str]),
    Any,
}

fn runner(base: &str) -> Option<Runner> {
    use Runner::{Any, Sub};
    Some(match base {
        "cargo" => Sub(&[
            "test", "build", "check", "nextest", "clippy", "bench", "t", "b", "c",
        ]),
        "go" => Sub(&["test", "build", "vet"]),
        "npm" | "pnpm" | "yarn" | "bun" => Sub(&["test", "t", "run", "run-script"]),
        "deno" => Sub(&["test", "check", "compile", "lint"]),
        "dotnet" => Sub(&["test", "build"]),
        "mvn" | "gradle" | "gradlew" => {
            Sub(&["test", "verify", "build", "check", "compile", "package"])
        }
        "zig" => Sub(&["build", "test", "build-exe", "build-lib"]),
        "swift" => Sub(&["build", "test"]),
        "mix" => Sub(&["test", "compile"]),
        "bundle" | "uv" | "poetry" | "npx" | "bunx" | "pnpx" => Sub(&["exec", "run", "x"]),
        "rustc" | "pytest" | "python" | "python3" | "node" | "make" | "cmake" | "ctest" | "tsc"
        | "jest" | "vitest" | "mocha" | "ruby" | "rspec" | "rake" | "javac" | "java" | "gcc"
        | "g++" | "clang" | "clang++" | "cc" | "php" | "phpunit" | "elixir" | "julia"
        | "Rscript" | "perl" | "prove" | "tox" | "nox" | "bats" | "shellcheck" => Any,
        _ => return None,
    })
}

/// Programs that assert something about a file's content: they count only
/// when they name a file the contract writes.
const ASSERTIONS: &[&str] = &["grep", "egrep", "fgrep", "rg", "diff", "cmp", "jq"];

/// Commands whose exit status is 0 whatever the work is, or that only list,
/// print or test for existence.
const ALWAYS_PASS: &[&str] = &[
    "true", ":", "echo", "printf", "ls", "cat", "pwd", "date", "sleep", "wc", "head", "tail",
    "stat", "find", "tee", "touch",
];

/// What one simple command does to the exit status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// It fails when the work is wrong.
    Real,
    /// It passes whatever the work is.
    Pass,
    /// It always fails.
    Fail,
    /// It may fail, but not because of the work (`cd`, `mkdir`, `export`).
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Seq,
    And,
    Or,
    Pipe,
}

/// Why `command` tests nothing, or `None` when its exit status can fail
/// because the work is wrong. `writes` are the files the contract writes.
pub fn vacuous(command: &str, writes: &[String]) -> Option<String> {
    let Some(segments) = split(command) else {
        return Some(
            "it uses shell syntax kGraf does not judge (a substitution, subshell, group or \
             control keyword); write plain commands joined by && or ;"
                .to_owned(),
        );
    };
    let mut errexit = false;
    let mut pipefail = false;
    // The status so far: whether a real check can make it fail, and
    // whether it always passes.
    let mut real = false;
    let mut always_pass = true;
    let mut first = true;
    for (op, text) in segments {
        let words = words(&text);
        if words.first().is_some_and(|w| w == "set") {
            errexit |= words.iter().any(|w| {
                w == "-e" || (w.starts_with('-') && !w.starts_with("--") && w.contains('e'))
            });
            pipefail |= words.windows(2).any(|p| p[0] == "-o" && p[1] == "pipefail");
        }
        let kind = kind(&words, writes);
        let (k_real, k_pass) = (kind == Kind::Real, kind == Kind::Pass);
        let op = match op {
            Op::Seq if errexit => Op::And,
            Op::Pipe if pipefail => Op::And,
            other => other,
        };
        if first {
            (real, always_pass) = (k_real, k_pass);
            first = false;
            continue;
        }
        match op {
            Op::Seq | Op::Pipe => (real, always_pass) = (k_real, k_pass),
            Op::And => {
                if always_pass {
                    (real, always_pass) = (k_real, k_pass);
                } else {
                    real |= k_real;
                    always_pass = false;
                }
            }
            Op::Or => {
                if !always_pass {
                    real = (real || k_real) && !k_pass;
                    always_pass = k_pass;
                }
            }
        }
    }
    if real {
        None
    } else if always_pass {
        Some("it always passes".to_owned())
    } else {
        Some(
            "its exit status does not depend on a test runner, a build, or an assertion over a \
             file the contract writes (a trailing `|| true`, `; true` or `exit 0`, a status taken \
             from a pipe's last command, or a version or help request does not count)"
                .to_owned(),
        )
    }
}

/// The command's simple commands with the operator before each (`Seq` for
/// the first). `None` when it holds syntax this reading does not follow.
fn split(command: &str) -> Option<Vec<(Op, String)>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut op = Op::Seq;
    let mut chars = command.chars().peekable();
    let (mut single, mut double) = (false, false);
    let push = |out: &mut Vec<(Op, String)>, cur: &mut String, op: Op| {
        if !cur.trim().is_empty() {
            out.push((op, std::mem::take(cur)));
        }
        cur.clear();
    };
    while let Some(c) = chars.next() {
        if single {
            single = c != '\'';
            cur.push(c);
            continue;
        }
        if c == '\\' {
            cur.push(c);
            if let Some(n) = chars.next() {
                cur.push(n);
            }
            continue;
        }
        if double {
            if c == '"' {
                double = false;
            } else if c == '`' || (c == '$' && chars.peek() == Some(&'(')) {
                return None;
            }
            cur.push(c);
            continue;
        }
        match c {
            '\'' => {
                single = true;
                cur.push(c);
            }
            '"' => {
                double = true;
                cur.push(c);
            }
            '`' | '(' | ')' | '{' | '}' => return None,
            '$' if chars.peek() == Some(&'(') => return None,
            ';' | '\n' => {
                push(&mut out, &mut cur, op);
                op = Op::Seq;
            }
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                push(&mut out, &mut cur, op);
                op = Op::And;
            }
            '&' => {
                push(&mut out, &mut cur, op);
                op = Op::Seq;
            }
            '|' if chars.peek() == Some(&'|') => {
                chars.next();
                push(&mut out, &mut cur, op);
                op = Op::Or;
            }
            '|' => {
                push(&mut out, &mut cur, op);
                op = Op::Pipe;
            }
            _ => cur.push(c),
        }
    }
    if single || double {
        return None;
    }
    push(&mut out, &mut cur, op);
    let keyword = |s: &str| {
        matches!(
            s.split_whitespace().next(),
            Some(
                "if" | "then"
                    | "else"
                    | "elif"
                    | "fi"
                    | "for"
                    | "while"
                    | "until"
                    | "do"
                    | "done"
                    | "case"
                    | "esac"
                    | "function"
            )
        )
    };
    if out.iter().any(|(_, s)| keyword(s)) {
        return None;
    }
    Some(out)
}

/// A simple command's words, quotes removed, leading assignments and
/// `env`/`command`/`exec` prefixes dropped.
fn words(text: &str) -> Vec<String> {
    let mut words =
        shlex::split(text).unwrap_or_else(|| text.split_whitespace().map(str::to_owned).collect());
    while words.first().is_some_and(|w| {
        matches!(w.as_str(), "env" | "command" | "exec" | "time" | "nice")
            || (w.contains('=') && !w.starts_with('-') && !w.starts_with('='))
    }) {
        words.remove(0);
    }
    words
}

fn kind(words: &[String], writes: &[String]) -> Kind {
    let Some(program) = words.first() else {
        return Kind::Other;
    };
    if program == "!" {
        return match kind(&words[1..], writes) {
            Kind::Pass => Kind::Fail,
            Kind::Fail => Kind::Pass,
            k => k,
        };
    }
    let base = program.rsplit('/').next().unwrap_or(program);
    let args = &words[1..];
    match base {
        "exit" => {
            return if args.first().is_none_or(|a| a == "0") {
                Kind::Pass
            } else {
                Kind::Fail
            };
        }
        "false" => return Kind::Fail,
        "test" | "[" => return Kind::Pass,
        _ => {}
    }
    if ALWAYS_PASS.contains(&base) {
        return Kind::Pass;
    }
    if ASSERTIONS.contains(&base) {
        return if names_a_write(args, writes) {
            Kind::Real
        } else {
            Kind::Other
        };
    }
    if args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--version" | "-V" | "--help" | "-h" | "version" | "help"
        )
    }) {
        return Kind::Other;
    }
    match runner(base) {
        Some(Runner::Any) => Kind::Real,
        Some(Runner::Sub(subs)) => {
            let sub = args
                .iter()
                .find(|a| !a.starts_with('-') && !a.starts_with('+'));
            match sub {
                // A wrapper (`uv run`, `npx`, `bundle exec`): judged by what it runs.
                Some(s)
                    if matches!(base, "bundle" | "uv" | "poetry") && subs.contains(&s.as_str()) =>
                {
                    let at = args.iter().position(|a| a == s).map_or(0, |i| i + 1);
                    kind(&args[at..], writes)
                }
                Some(_) if matches!(base, "npx" | "bunx" | "pnpx") => {
                    let at = args.iter().position(|a| !a.starts_with('-')).unwrap_or(0);
                    kind(&args[at..], writes)
                }
                // `npm run build`: a script whose name says it tests or builds.
                Some(s) if matches!(s.as_str(), "run" | "run-script") => {
                    let script = args
                        .iter()
                        .skip_while(|a| *a != s)
                        .skip(1)
                        .find(|a| !a.starts_with('-'));
                    if script.is_some_and(|n| {
                        ["test", "build", "check", "lint", "typecheck", "compile"]
                            .iter()
                            .any(|k| n.contains(k))
                    }) {
                        Kind::Real
                    } else {
                        Kind::Other
                    }
                }
                Some(s) if subs.contains(&s.as_str()) => Kind::Real,
                _ => Kind::Other,
            }
        }
        None => Kind::Other,
    }
}

fn names_a_write(args: &[String], writes: &[String]) -> bool {
    args.iter().any(|w| {
        let w = w.trim_start_matches("./");
        writes.iter().any(|f| {
            let f = f.trim_start_matches("./");
            w == f || w.ends_with(&format!("/{f}"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::vacuous;

    fn writes() -> Vec<String> {
        vec!["src/a.rs".to_owned()]
    }

    #[test]
    fn a_check_whose_status_cannot_fail_on_the_work_is_vacuous() {
        for c in [
            "true",
            ":",
            "exit 0",
            "echo ok",
            "test -f src/a.rs",
            "test -e src/a.rs",
            "ls src",
            "cat src/a.rs",
            "cargo test || true",
            "cargo test || :",
            "cargo test; true",
            "cargo test; exit 0",
            "cargo test\nexit 0",
            "cargo test | tee log",
            "true || cargo test",
            "cargo --version",
            "cargo version",
            "go version",
            "python3 --version",
            "npm --version",
            "npm run start",
            "grep -q x README.md",
            "cargo test $(echo x)",
            "cargo test `echo x`",
            "(cargo test)",
            "if cargo test; then true; fi",
            "cargo test; cd ..",
        ] {
            assert!(vacuous(c, &writes()).is_some(), "{c:?} counted as a check");
        }
    }

    #[test]
    fn a_check_whose_status_fails_on_the_work_counts() {
        for c in [
            "cargo test -p a",
            "cargo build",
            "cargo test && echo ok",
            "cd sub && cargo test",
            "cargo test || exit 1",
            "set -e; cargo test; echo done",
            "set -o pipefail; cargo test | tee log",
            "python3 -m pytest tests/test_a.py",
            "npm test",
            "npm run build",
            "go test ./...",
            "uv run pytest",
            "npx jest",
            "RUST_LOG=info cargo test",
            "grep -q 'fn double' src/a.rs",
            "! grep -q TODO src/a.rs",
            "test -f src/a.rs && cargo build",
            "./node_modules/.bin/jest",
            "jq -e '.ok == true' src/a.rs",
        ] {
            assert_eq!(vacuous(c, &writes()), None, "{c:?} refused");
        }
    }
}
