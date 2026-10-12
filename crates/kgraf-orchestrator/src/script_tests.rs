//! A Python test file run as a script that never calls its tests.
//!
//! An interface's test command `python test_x.py` runs the file as a script.
//! A file that only defines `def test_...` functions (pytest style) or
//! `unittest.TestCase` classes, and never calls them, runs no assertion: it
//! exits 0 against the stub and against the reference alike. The vacuity
//! check then refuses the interface with "the test passes against the stub",
//! which names the symptom and not the cause, and a planner that reads it
//! makes the assertion stricter and keeps the same shape, so every ask of the
//! interface call can be spent on it (about 100 s per ask on a local model)
//! and no interface is accepted.
//!
//! When the intent is certain (every test is a plain function that takes no
//! arguments, or a `unittest.TestCase` class), kGraf appends the calls under
//! `if __name__ == "__main__":`, as it replaces a compile command that does
//! not build the files instead of asking again. Otherwise (a test that takes
//! arguments such as pytest fixtures, a decorated or async test, a pytest
//! class) the interface is refused with the cause and the remedy.
//!
//! The reading is line based over the module's top level (column 0). A file
//! whose top level could run its tests in any way kGraf does not read
//! (an `if`, a loop, a `with`, a `try`, a top-level `assert`, a line that
//! names a test, `globals()`, `unittest.main`, `pytest.main`) is left alone:
//! a wrong "runs nothing" would add calls the file already makes.

use crate::checks::InterfaceContract;

/// What kGraf does about a test file that never calls its tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScriptTests {
    /// The calls were appended: the file's index in `tests`, its new content,
    /// and the names now called.
    Repaired {
        index: usize,
        content: String,
        called: Vec<String>,
    },
    /// The calls cannot be added with certainty: the refusal, said to the
    /// planner.
    Refused(String),
}

/// The script a Python command runs, when the command runs one: the first
/// argument that is not an option of `python`, `python3` or `python3.N`.
/// `-m` and `-c` run no script file.
pub(crate) fn python_script(argv: &[String]) -> Option<&str> {
    let program = argv.first()?;
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe");
    let version = base.strip_prefix("python")?;
    if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let mut args = argv[1..].iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-m" | "-c" => return None,
            // Options that take the next argument as their value.
            "-W" | "-X" => {
                args.next();
            }
            s if s.starts_with('-') => {}
            s => return s.ends_with(".py").then_some(s),
        }
    }
    None
}

/// One test the module defines at its top level.
#[derive(Debug)]
struct Defined {
    name: String,
    /// Why kGraf cannot call it itself; `None` when it can (a plain function
    /// with no required argument, or a `unittest.TestCase` class).
    blocker: Option<String>,
    unittest: bool,
}

/// The tests a module defines at its top level, or `None` when its top level
/// may already run them.
fn uncalled_tests(text: &str) -> Option<Vec<Defined>> {
    let mut defined: Vec<Defined> = Vec::new();
    let mut statements: Vec<&str> = Vec::new();
    let mut in_string: Option<&str> = None;
    let mut decorated = false;
    for line in text.lines() {
        let starts_inside = in_string.is_some();
        for q in ["\"\"\"", "'''"] {
            let n = line.matches(q).count();
            if n % 2 == 1 {
                in_string = match in_string {
                    Some(open) if open == q => None,
                    Some(open) => Some(open),
                    None => Some(q),
                };
            }
        }
        if starts_inside || line.is_empty() || line.starts_with([' ', '\t', '#']) {
            continue;
        }
        if line.starts_with('@') {
            decorated = true;
            continue;
        }
        let was_decorated = std::mem::take(&mut decorated);
        let (is_async, def) = match line.strip_prefix("async def ") {
            Some(rest) => (true, Some(rest)),
            None => (false, line.strip_prefix("def ")),
        };
        if let Some(rest) = def {
            let name = ident(rest);
            if name.starts_with("test") {
                let blocker = if is_async {
                    Some(format!("`{name}` is async"))
                } else if was_decorated {
                    Some(format!("`{name}` is decorated"))
                } else {
                    match required_args(rest) {
                        Some(false) => None,
                        Some(true) => Some(format!("`{name}` takes arguments")),
                        None => Some(format!("the arguments of `{name}` are not on one line")),
                    }
                };
                defined.push(Defined {
                    name: name.to_owned(),
                    blocker,
                    unittest: false,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("class ") {
            let name = ident(rest);
            let bases = rest[name.len()..].split(':').next().unwrap_or("");
            if bases.contains("TestCase") {
                defined.push(Defined {
                    name: name.to_owned(),
                    blocker: was_decorated.then(|| format!("`{name}` is decorated")),
                    unittest: true,
                });
            } else if name.starts_with("Test") {
                defined.push(Defined {
                    name: name.to_owned(),
                    blocker: Some(format!("`{name}` is a pytest class")),
                    unittest: false,
                });
            }
            continue;
        }
        if line.starts_with("import ") || line.starts_with("from ") {
            continue;
        }
        statements.push(line);
    }
    if defined.is_empty() {
        return None;
    }
    let may_run = |s: &str| {
        let head = ident(s);
        matches!(
            head,
            "if" | "for" | "while" | "with" | "try" | "assert" | "async" | "match"
        ) || s.contains("globals()")
            || s.contains("unittest.main")
            || s.contains("pytest.main")
            || s.contains("main(")
            || defined.iter().any(|d| names_word(s, &d.name))
    };
    if statements.iter().any(|s| may_run(s)) {
        return None;
    }
    Some(defined)
}

/// The identifier at the start of `s`.
fn ident(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    &s[..end]
}

/// Whether `word` appears in `s` as a whole identifier.
fn names_word(s: &str, word: &str) -> bool {
    s.match_indices(word).any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + word.len()..].chars().next();
        let part = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        !part(before) && !part(after)
    })
}

/// Whether a `def` line's parameters include one with no default: `Some`
/// when the parameter list closes on the line, `None` otherwise.
fn required_args(rest: &str) -> Option<bool> {
    let open = rest.find('(')?;
    let close = open + rest[open..].find(')')?;
    let params = rest[open + 1..close].trim();
    if params.is_empty() {
        return Some(false);
    }
    Some(
        params
            .split(',')
            .map(str::trim)
            .any(|p| !(p.is_empty() || p.contains('=') || p.starts_with('*') || p == "/")),
    )
}

/// The repair or the refusal for `ic` when its test command runs a test file
/// of the answer's own as a script and the file never calls its tests;
/// `None` otherwise. `written(i)`: whether `ic.tests[i]` holds content the
/// answer wrote (a repository file named by path alone is never changed).
pub(crate) fn check(
    ic: &InterfaceContract,
    written: impl Fn(usize) -> bool,
) -> Option<ScriptTests> {
    let script = python_script(&ic.test)?.trim_start_matches("./");
    let index = ic
        .tests
        .iter()
        .position(|f| f.path.trim_start_matches("./") == script)?;
    if !written(index) {
        return None;
    }
    let file = &ic.tests[index];
    let defined = uncalled_tests(&file.content)?;
    let names: Vec<String> = defined.iter().map(|d| d.name.clone()).collect();
    let blockers: Vec<&str> = defined
        .iter()
        .filter_map(|d| d.blocker.as_deref())
        .collect();
    if !blockers.is_empty() {
        return Some(ScriptTests::Refused(format!(
            "interface `{}`: the test command `{}` runs `{}` as a script, and the file defines {} \
             without calling them, so no assertion runs and the test passes against any \
             producer; kGraf did not add the calls because {}; call the tests at the end of the \
             file under `if __name__ == \"__main__\":`, or run the file with a test runner the \
             project has",
            ic.id,
            ic.test.join(" "),
            file.path,
            names.join(", "),
            blockers.join(", ")
        )));
    }
    let mut content = file.content.clone();
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n\nif __name__ == \"__main__\":\n");
    let mut unittest = false;
    for d in &defined {
        if d.unittest {
            unittest = true;
        } else {
            content.push_str(&format!("    {}()\n", d.name));
        }
    }
    if unittest {
        content.push_str("    import unittest\n    unittest.main()\n");
    }
    Some(ScriptTests::Repaired {
        index,
        content,
        called: names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::FileOverlay;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn ic(test: &[&str], path: &str, content: &str) -> InterfaceContract {
        InterfaceContract {
            kind: crate::checks::InterfaceKind::Behavior,
            id: "i1".into(),
            version: 1,
            parties: argv(&["c1", "c2"]),
            symbols: argv(&["dotproduct"]),
            stub: vec![],
            wrong: vec![],
            reference: vec![],
            consumer: vec![],
            tests: vec![FileOverlay::new(path, content)],
            compile: vec![],
            test: argv(test),
            consulted: vec![],
            replaces: vec![],
        }
    }

    /// The test file of a second interface ask, verbatim.
    const RECORDED: &str = "import sys\nsys.path.insert(0, 'src')\ndef test_dotproduct():\n    \
from vectorops import dotproduct\n    assert 1 == dotproduct([1, 1], [0, 1])\n";

    #[test]
    fn the_script_is_the_first_argument_that_is_not_an_option() {
        assert_eq!(python_script(&argv(&["python", "t.py"])), Some("t.py"));
        assert_eq!(
            python_script(&argv(&["python3", "-u", "t.py"])),
            Some("t.py")
        );
        assert_eq!(
            python_script(&argv(&["/usr/bin/python3.12", "-W", "ignore", "t.py", "x"])),
            Some("t.py")
        );
        assert_eq!(
            python_script(&argv(&["python", "-m", "pytest", "t.py"])),
            None
        );
        assert_eq!(python_script(&argv(&["python", "-c", "import t"])), None);
        assert_eq!(python_script(&argv(&["pytest", "t.py"])), None);
        assert_eq!(python_script(&argv(&["pythonista", "t.py"])), None);
        assert_eq!(python_script(&argv(&["python", "data.txt"])), None);
    }

    #[test]
    fn the_recorded_file_gets_its_test_called_under_main() {
        let got = check(&ic(&["python", "t.py"], "t.py", RECORDED), |_| true);
        let Some(ScriptTests::Repaired {
            index,
            content,
            called,
        }) = got
        else {
            panic!("repaired: {got:?}");
        };
        assert_eq!(index, 0);
        assert_eq!(called, ["test_dotproduct"]);
        assert!(
            content.starts_with(RECORDED),
            "the answer's text is kept: {content}"
        );
        assert!(
            content.ends_with("\n\nif __name__ == \"__main__\":\n    test_dotproduct()\n"),
            "{content}"
        );
    }

    #[test]
    fn unittest_classes_get_unittest_main_after_the_functions() {
        let text = "import unittest\n\nclass T(unittest.TestCase):\n    def test_a(self):\n        \
self.assertEqual(1, 1)\n\ndef test_b():\n    assert True\n";
        let Some(ScriptTests::Repaired {
            content, called, ..
        }) = check(&ic(&["python3", "./t.py"], "t.py", text), |_| true)
        else {
            panic!("repaired");
        };
        assert_eq!(called, ["T", "test_b"]);
        assert!(
            content.ends_with(
                "if __name__ == \"__main__\":\n    test_b()\n    import unittest\n    unittest.main()\n"
            ),
            "{content}"
        );
    }

    #[test]
    fn a_test_kgraf_cannot_call_is_refused_with_the_cause() {
        for (text, cause) in [
            (
                "def test_a(tmp_path):\n    assert tmp_path\n",
                "`test_a` takes arguments",
            ),
            (
                "async def test_a():\n    assert True\n",
                "`test_a` is async",
            ),
            (
                "import pytest\n@pytest.mark.parametrize('x', [1])\ndef test_a(x):\n    assert x\n",
                "`test_a` is decorated",
            ),
            (
                "class TestA:\n    def test_a(self):\n        assert True\n",
                "`TestA` is a pytest class",
            ),
            ("def test_a(\n    x,\n):\n    assert x\n", "not on one line"),
        ] {
            let got = check(&ic(&["python", "t.py"], "t.py", text), |_| true);
            let Some(ScriptTests::Refused(why)) = got else {
                panic!("refused for {text:?}: {got:?}");
            };
            assert!(why.contains(cause), "{why}");
            assert!(
                why.contains("runs `t.py` as a script") && why.contains("no assertion runs"),
                "{why}"
            );
        }
    }

    #[test]
    fn a_file_that_runs_its_tests_or_may_is_left_alone() {
        for text in [
            "def test_a():\n    assert True\n\nif __name__ == '__main__':\n    test_a()\n",
            "def test_a():\n    assert True\n\ntest_a()\n",
            "def test_a():\n    assert True\n\nfor f in [test_a]:\n    f()\n",
            "import unittest\nclass T(unittest.TestCase):\n    def test_a(self):\n        pass\nunittest.main()\n",
            "def test_a():\n    assert True\n\nassert 1 == 1\n",
            "def test_a():\n    assert True\n\nrun_all(globals())\n",
            "import sys\nassert sys.version\n",
            "x = 1\n",
        ] {
            assert_eq!(
                check(&ic(&["python", "t.py"], "t.py", text), |_| true),
                None,
                "{text}"
            );
        }
    }

    #[test]
    fn only_a_script_run_of_a_file_the_answer_wrote_is_judged() {
        let other = ic(&["python", "-m", "pytest", "t.py"], "t.py", RECORDED);
        assert_eq!(
            check(&other, |_| true),
            None,
            "a test runner calls the tests"
        );
        let elsewhere = ic(&["python", "run.py"], "t.py", RECORDED);
        assert_eq!(
            check(&elsewhere, |_| true),
            None,
            "the script is not the test file"
        );
        let repository = ic(&["python", "t.py"], "t.py", RECORDED);
        assert_eq!(
            check(&repository, |_| false),
            None,
            "a repository file is never changed"
        );
    }

    #[test]
    fn a_docstring_is_not_read_as_code() {
        let text = "\"\"\"\ntest_a()\n\"\"\"\ndef test_a():\n    assert False\n";
        assert!(matches!(
            check(&ic(&["python", "t.py"], "t.py", text), |_| true),
            Some(ScriptTests::Repaired { .. })
        ));
    }
}
