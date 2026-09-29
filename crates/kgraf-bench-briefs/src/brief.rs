//! The fixed brief suite: at least two briefs per [`TaskKind`], each with a
//! fixture repository under `fixtures/` and a mechanical [`Check`].
//!
//! A brief is versioned (`Brief::version`); the suite's hash
//! ([`suite_hash`]) is recorded with every bench result, so a result can
//! always be traced back to the exact briefs that produced it (a changed
//! brief changes the hash, never the version silently).

use kgraf_core::model_profile::TaskKind;
use std::path::{Path, PathBuf};

/// One script invocation a [`Check::Script`] must satisfy.
#[derive(Clone, Copy, Debug)]
pub struct ScriptCase {
    pub args: &'static [&'static str],
    pub expect_stdout: &'static str,
    pub expect_status: i32,
}

/// One fact a [`Check::Docs`] file must state.
#[derive(Clone, Copy, Debug)]
pub enum DocFact {
    /// A plain substring (no case-folding, so the brief must say exactly
    /// what it wants).
    Text(&'static str),
    /// A changelog entry, read structurally: some line carries `version` and
    /// `date` each as a whole token (so `10.2.0` is not `0.2.0` and a date
    /// inside a longer number does not count), and `word` (case-insensitive,
    /// as the start of a token: `crash`, `crashes`, `crashed`) appears on that
    /// line or a later one.
    Entry {
        version: &'static str,
        date: &'static str,
        word: &'static str,
    },
}

/// A deliberate defect a [`Check::CargoTestNamed`] test must catch: in a
/// copy of the attempt, `from` in `file` is replaced by `to`, and the named
/// test must then run and fail. A test that asserts nothing passes on the
/// broken copy and so proves nothing.
#[derive(Clone, Copy, Debug)]
pub struct Mutation {
    pub file: &'static str,
    pub from: &'static str,
    pub to: &'static str,
}

/// A mechanical satisfaction check. Never a model judge (silent
/// degradation risk: a model judging another model's work is not a
/// measurement, it is a second guess).
#[derive(Clone, Debug)]
pub enum Check {
    /// `cargo test` must exit 0 in the checked-out attempt, report no
    /// failure, and run at least `min_passed` tests (a worker that deletes
    /// the tests must not pass).
    CargoTest { min_passed: u64 },
    /// One named test in one test binary must have run and passed:
    /// `cargo test --test <test_bin> <test_name> -- --exact`. Cargo exits 0
    /// when no test matches, so the exit code is not enough: the output must
    /// carry `test <test_name> ... ok` and exactly one passed test in all.
    /// Each entry of `mutations` is applied to a copy in turn and the test
    /// must fail there.
    CargoTestNamed {
        test_bin: &'static str,
        test_name: &'static str,
        mutations: &'static [Mutation],
    },
    /// `cargo test` as in [`Check::CargoTest`], and in addition the source
    /// `file` must define `function` with a body that calls `helper` and holds
    /// none of `absent` (the old logic, inlined). Passing tests alone do not
    /// show that a refactor happened: a helper that nothing calls passes them
    /// too. The scan is tolerant (comments and string literals are ignored,
    /// whitespace does not matter) and is not a parser: it proves the call is
    /// written, not that it is reached.
    CargoTestUses {
        min_passed: u64,
        file: &'static str,
        function: &'static str,
        helper: &'static str,
        absent: &'static [&'static str],
    },
    /// Run `script` (a POSIX shell script) under `sh` for each case with the
    /// fixture's checked-out directory as the working directory; every case
    /// must match its expected stdout and exit status exactly.
    Script {
        script: &'static str,
        cases: &'static [ScriptCase],
    },
    /// `file` must exist, state every fact in `required`, and hold no more
    /// than `max_words` whitespace-separated words.
    Docs {
        file: &'static str,
        required: &'static [DocFact],
        max_words: usize,
    },
}

/// One brief in the fixed suite.
#[derive(Clone, Debug)]
pub struct Brief {
    /// Stable, kebab-case; also the fixture's directory name under
    /// `fixtures/`.
    pub id: &'static str,
    pub kind: TaskKind,
    /// Bumped whenever `instruction`, the fixture, or `check` changes in a
    /// way that could change whether a past result still applies.
    pub version: u32,
    /// The worker's instruction, in full (this is what `WorkerBrief`
    /// carries; the fixture's own doc comments repeat the constraint for the
    /// worker to read, but this string is the one place it is authoritative).
    pub instruction: &'static str,
    pub check: Check,
    /// Git pathspecs (relative to the repository root) the brief says the
    /// worker must not change. The check fails when the attempt's commit
    /// differs from the fixture commit on any of them.
    pub protected: &'static [&'static str],
}

impl Brief {
    /// The fixture's source directory (`fixtures/<id>`, under this crate).
    pub fn fixture_dir(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(self.id)
    }

    /// A canonical string this brief's identity is hashed from: everything
    /// that, if it changed, should change [`suite_hash`].
    fn canonical(&self) -> String {
        format!(
            "{}\x1f{}\x1f{}\x1f{:?}\x1f{}\x1f{:?}",
            self.id,
            self.kind.as_str(),
            self.version,
            self.check,
            self.instruction,
            self.protected
        )
    }
}

/// The fixed suite: at least two briefs per [`TaskKind`].
pub fn suite() -> Vec<Brief> {
    vec![
        // ---------------------------------------------------------- Interface
        Brief {
            id: "interface-duration",
            kind: TaskKind::Interface,
            version: 2,
            instruction: "This crate is `fx-interface-duration`. Add a public function with \
                exactly this signature: `pub fn parse_duration(input: &str) -> Result<u64, \
                String>`. It accepts an integer immediately followed by `s` (seconds) or `m` \
                (minutes) and returns the duration in seconds; anything else is an `Err` with a \
                short reason. Do not change `tests/consumer.rs`. Make only this change.",
            check: Check::CargoTest { min_passed: 2 },
            protected: &["tests/consumer.rs", "Cargo.toml"],
        },
        Brief {
            id: "interface-greeter",
            kind: TaskKind::Interface,
            version: 2,
            instruction: "This crate is `fx-interface-greeter`. Declare a public trait named \
                exactly `Greeter` with one required method: `fn greet(&self, name: &str) -> \
                String;`. Do not implement the trait and do not change `tests/consumer.rs`, \
                which implements and calls it. Make only this change.",
            check: Check::CargoTest { min_passed: 1 },
            protected: &["tests/consumer.rs", "Cargo.toml"],
        },
        // ------------------------------------------------------ Implementation
        Brief {
            id: "impl-fizzbuzz",
            kind: TaskKind::Implementation,
            version: 2,
            instruction: "This crate is `fx-impl-fizzbuzz`. Implement `pub fn fizzbuzz(n: u32) \
                -> String` in `src/lib.rs`: multiples of 3 return \"Fizz\", multiples of 5 \
                return \"Buzz\", multiples of both return \"FizzBuzz\", anything else returns \
                the number as a string. Do not change `tests/behavior.rs`.",
            check: Check::CargoTest { min_passed: 1 },
            protected: &["tests/behavior.rs", "Cargo.toml"],
        },
        Brief {
            id: "impl-counter",
            kind: TaskKind::Implementation,
            version: 2,
            instruction: "This crate is `fx-impl-counter`. Implement a public `Counter` type in \
                `src/lib.rs` with `pub fn new() -> Self`, `pub fn incr(&mut self)` and \
                `pub fn get(&self) -> u32`, starting at 0 and incrementing by 1 per call. Do not \
                change `tests/behavior.rs`.",
            check: Check::CargoTest { min_passed: 1 },
            protected: &["tests/behavior.rs", "Cargo.toml"],
        },
        // ------------------------------------------------------------- Tests
        Brief {
            id: "tests-sum",
            kind: TaskKind::Tests,
            version: 2,
            instruction: "This crate is `fx-tests-sum`; `src/lib.rs`'s `sum` is already correct \
                and must not change. Add a new file `tests/more_tests.rs` with a test function \
                named exactly `handles_empty_and_negative` that asserts `sum(&[]) == 0` and \
                `sum(&[-3, 5, -2]) == 0`.",
            check: Check::CargoTestNamed {
                test_bin: "more_tests",
                test_name: "handles_empty_and_negative",
                mutations: &[
                    Mutation {
                        file: "src/lib.rs",
                        from: "xs.iter().sum()",
                        to: "if xs.is_empty() { 1 } else { xs.iter().sum() }",
                    },
                    Mutation {
                        file: "src/lib.rs",
                        from: "xs.iter().sum()",
                        to: "xs.iter().map(|x| x.abs()).sum()",
                    },
                ],
            },
            protected: &["src/lib.rs", "Cargo.toml"],
        },
        Brief {
            id: "tests-reverse",
            kind: TaskKind::Tests,
            version: 2,
            instruction: "This crate is `fx-tests-reverse`; `src/lib.rs`'s `reverse_words` is \
                already correct and must not change. Add a new file `tests/more_tests.rs` with \
                a test function named exactly `reverses_multiple_words` that asserts \
                `reverse_words(\"a b c\") == \"c b a\"` and `reverse_words(\"\") == \"\"`.",
            check: Check::CargoTestNamed {
                test_bin: "more_tests",
                test_name: "reverses_multiple_words",
                mutations: &[
                    Mutation {
                        file: "src/lib.rs",
                        from: "s.split_whitespace().rev()",
                        to: "s.split_whitespace()",
                    },
                    Mutation {
                        file: "src/lib.rs",
                        from: "s.split_whitespace().rev().collect::<Vec<_>>().join(\" \")",
                        to: "if s.is_empty() { \"x\".to_owned() } else { s.split_whitespace().rev().collect::<Vec<_>>().join(\" \") }",
                    },
                ],
            },
            protected: &["src/lib.rs", "Cargo.toml"],
        },
        // ----------------------------------------------------------- Refactor
        Brief {
            id: "refactor-shipping",
            kind: TaskKind::Refactor,
            version: 3,
            instruction: "This crate is `fx-refactor-shipping`. `total_price` in `src/lib.rs` \
                computes a discount formula twice. Extract it into a new public function named \
                exactly `apply_discount(price: f64, pct_off: f64) -> f64` and call it from \
                `total_price` instead of repeating the formula; `total_price`'s behavior must \
                not change. Do not change `tests/behavior.rs` or `tests/structure.rs`.",
            check: Check::CargoTestUses {
                min_passed: 2,
                file: "src/lib.rs",
                function: "total_price",
                helper: "apply_discount",
                absent: &["/ 100.0"],
            },
            protected: &["tests/behavior.rs", "tests/structure.rs", "Cargo.toml"],
        },
        Brief {
            id: "refactor-parser",
            kind: TaskKind::Refactor,
            version: 3,
            instruction: "This crate is `fx-refactor-parser`. `tally_csv_ints` in `src/lib.rs` \
                parses each comma-separated field inline. Extract that per-field parsing into a \
                new public function named exactly `parse_int_field(field: &str) -> i64` (trim \
                the field, parse it, default to 0 on error) and call it for each field; \
                `tally_csv_ints`'s behavior must not change. Do not change `tests/behavior.rs` \
                or `tests/structure.rs`.",
            check: Check::CargoTestUses {
                min_passed: 2,
                file: "src/lib.rs",
                function: "tally_csv_ints",
                helper: "parse_int_field",
                absent: &[".parse"],
            },
            protected: &["tests/behavior.rs", "tests/structure.rs", "Cargo.toml"],
        },
        // ---------------------------------------------------------- Scripting
        Brief {
            id: "scripting-wordcount",
            kind: TaskKind::Scripting,
            version: 2,
            instruction: "Fix `run.sh` (POSIX `sh`) so that `sh run.sh <file>` prints, to \
                stdout and nothing else, the number of whitespace-separated words in the named \
                file, followed by a newline, and exits 0. Do not change `input.txt`.",
            check: Check::Script {
                script: "run.sh",
                cases: &[ScriptCase {
                    args: &["input.txt"],
                    expect_stdout: "6\n",
                    expect_status: 0,
                }],
            },
            protected: &["input.txt"],
        },
        Brief {
            id: "scripting-greet",
            kind: TaskKind::Scripting,
            version: 2,
            instruction: "Fix `run.sh` (POSIX `sh`) so that `sh run.sh NAME` prints exactly \
                `Hello, NAME!` followed by a newline to stdout and exits 0; with no argument it \
                prints nothing to stdout and exits with a nonzero status.",
            check: Check::Script {
                script: "run.sh",
                cases: &[
                    ScriptCase {
                        args: &["Ada"],
                        expect_stdout: "Hello, Ada!\n",
                        expect_status: 0,
                    },
                    ScriptCase {
                        args: &[],
                        expect_stdout: "",
                        expect_status: 1,
                    },
                ],
            },
            protected: &[],
        },
        // --------------------------------------------------------------- Docs
        Brief {
            id: "docs-api",
            kind: TaskKind::Docs,
            version: 2,
            instruction: "Create `docs/API.md` documenting a function with this exact \
                signature: `submit_order(order_id: u64, amount_cents: u64) -> Result<String, \
                String>`. State: the function's exact name and signature; that `amount_cents` \
                is in cents, never dollars; and that an `Err` means no order was created. Keep \
                the whole file to 120 words or fewer.",
            check: Check::Docs {
                file: "docs/API.md",
                required: &[
                    DocFact::Text(
                        "submit_order(order_id: u64, amount_cents: u64) -> Result<String, String>",
                    ),
                    DocFact::Text("cents"),
                    DocFact::Text("Err"),
                ],
                max_words: 120,
            },
            protected: &[],
        },
        Brief {
            id: "docs-changelog",
            kind: TaskKind::Docs,
            version: 2,
            instruction: "Create `CHANGES.md` with one entry: version `0.2.0`, dated \
                `2026-09-28`, summarizing that a crash on an empty input file was fixed. The \
                file must state the version number and the date together on one heading line, and the \
                word \"crash\" on that line or a later line. Keep the whole file to 60 words or \
                fewer.",
            check: Check::Docs {
                file: "CHANGES.md",
                required: &[DocFact::Entry {
                    version: "0.2.0",
                    date: "2026-09-28",
                    word: "crash",
                }],
                max_words: 60,
            },
            protected: &[],
        },
    ]
}

/// A blake3 hash (hex) of every brief's identity ([`Brief::canonical`]), in
/// suite order. Recorded with every bench result, so a result is always
/// traceable to the exact briefs that produced it.
pub fn suite_hash() -> String {
    let mut hasher = blake3::Hasher::new();
    for brief in suite() {
        hasher.update(brief.canonical().as_bytes());
        hasher.update(b"\x1e");
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_kind_has_at_least_two_briefs() {
        let briefs = suite();
        for kind in TaskKind::ALL {
            let n = briefs.iter().filter(|b| b.kind == kind).count();
            assert!(n >= 2, "{kind:?} has {n} brief(s)");
        }
    }

    #[test]
    fn every_brief_id_is_unique_and_has_a_fixture_directory() {
        let briefs = suite();
        let mut ids: Vec<&str> = briefs.iter().map(|b| b.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), briefs.len(), "duplicate brief id");
        for brief in &briefs {
            assert!(
                brief.fixture_dir().is_dir(),
                "{}: no fixture at {}",
                brief.id,
                brief.fixture_dir().display()
            );
        }
    }

    #[test]
    fn the_suite_hash_is_stable_and_changes_with_a_briefs_instruction() {
        let a = suite_hash();
        let b = suite_hash();
        assert_eq!(a, b);
        let mut changed = suite();
        changed[0].instruction = "a different instruction entirely";
        let mut hasher = blake3::Hasher::new();
        for brief in &changed {
            hasher.update(brief.canonical().as_bytes());
            hasher.update(b"\x1e");
        }
        assert_ne!(hasher.finalize().to_hex().to_string(), a);
    }
}
