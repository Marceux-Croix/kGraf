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

/// A mechanical satisfaction check. Never a model judge (silent
/// degradation risk: a model judging another model's work is not a
/// measurement, it is a second guess).
#[derive(Clone, Debug)]
pub enum Check {
    /// `cargo test` must exit 0 in the checked-out attempt.
    CargoTest,
    /// One named test in one test binary must exist and pass:
    /// `cargo test --test <test_bin> <test_name> -- --exact`. A missing test
    /// binary or test name is a normal cargo failure, not a harness error.
    CargoTestNamed {
        test_bin: &'static str,
        test_name: &'static str,
    },
    /// Run `script` (a POSIX shell script) under `sh` for each case with the
    /// fixture's checked-out directory as the working directory; every case
    /// must match its expected stdout and exit status exactly.
    Script {
        script: &'static str,
        cases: &'static [ScriptCase],
    },
    /// `file` must exist, contain every string in `required` (a plain
    /// substring match; no case-folding, so the brief must say exactly what
    /// it wants), and hold no more than `max_words` whitespace-separated
    /// words.
    Docs {
        file: &'static str,
        required: &'static [&'static str],
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
            "{}\x1f{}\x1f{}\x1f{:?}\x1f{}",
            self.id,
            self.kind.as_str(),
            self.version,
            self.check,
            self.instruction
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
            version: 1,
            instruction: "This crate is `fx-interface-duration`. Add a public function with \
                exactly this signature: `pub fn parse_duration(input: &str) -> Result<u64, \
                String>`. It accepts an integer immediately followed by `s` (seconds) or `m` \
                (minutes) and returns the duration in seconds; anything else is an `Err` with a \
                short reason. Do not change `tests/consumer.rs`. Make only this change.",
            check: Check::CargoTest,
        },
        Brief {
            id: "interface-greeter",
            kind: TaskKind::Interface,
            version: 1,
            instruction: "This crate is `fx-interface-greeter`. Declare a public trait named \
                exactly `Greeter` with one required method: `fn greet(&self, name: &str) -> \
                String;`. Do not implement the trait and do not change `tests/consumer.rs`, \
                which implements and calls it. Make only this change.",
            check: Check::CargoTest,
        },
        // ------------------------------------------------------ Implementation
        Brief {
            id: "impl-fizzbuzz",
            kind: TaskKind::Implementation,
            version: 1,
            instruction: "This crate is `fx-impl-fizzbuzz`. Implement `pub fn fizzbuzz(n: u32) \
                -> String` in `src/lib.rs`: multiples of 3 return \"Fizz\", multiples of 5 \
                return \"Buzz\", multiples of both return \"FizzBuzz\", anything else returns \
                the number as a string. Do not change `tests/behavior.rs`.",
            check: Check::CargoTest,
        },
        Brief {
            id: "impl-counter",
            kind: TaskKind::Implementation,
            version: 1,
            instruction: "This crate is `fx-impl-counter`. Implement a public `Counter` type in \
                `src/lib.rs` with `pub fn new() -> Self`, `pub fn incr(&mut self)` and \
                `pub fn get(&self) -> u32`, starting at 0 and incrementing by 1 per call. Do not \
                change `tests/behavior.rs`.",
            check: Check::CargoTest,
        },
        // ------------------------------------------------------------- Tests
        Brief {
            id: "tests-sum",
            kind: TaskKind::Tests,
            version: 1,
            instruction: "This crate is `fx-tests-sum`; `src/lib.rs`'s `sum` is already correct \
                and must not change. Add a new file `tests/more_tests.rs` with a test function \
                named exactly `handles_empty_and_negative` that asserts `sum(&[]) == 0` and \
                `sum(&[-3, 5, -2]) == 0`.",
            check: Check::CargoTestNamed {
                test_bin: "more_tests",
                test_name: "handles_empty_and_negative",
            },
        },
        Brief {
            id: "tests-reverse",
            kind: TaskKind::Tests,
            version: 1,
            instruction: "This crate is `fx-tests-reverse`; `src/lib.rs`'s `reverse_words` is \
                already correct and must not change. Add a new file `tests/more_tests.rs` with \
                a test function named exactly `reverses_multiple_words` that asserts \
                `reverse_words(\"a b c\") == \"c b a\"` and `reverse_words(\"\") == \"\"`.",
            check: Check::CargoTestNamed {
                test_bin: "more_tests",
                test_name: "reverses_multiple_words",
            },
        },
        // ----------------------------------------------------------- Refactor
        Brief {
            id: "refactor-shipping",
            kind: TaskKind::Refactor,
            version: 1,
            instruction: "This crate is `fx-refactor-shipping`. `total_price` in `src/lib.rs` \
                computes a discount formula twice. Extract it into a new public function named \
                exactly `apply_discount(price: f64, pct_off: f64) -> f64` and call it from \
                `total_price` instead of repeating the formula; `total_price`'s behavior must \
                not change. Do not change `tests/behavior.rs` or `tests/structure.rs`.",
            check: Check::CargoTest,
        },
        Brief {
            id: "refactor-parser",
            kind: TaskKind::Refactor,
            version: 1,
            instruction: "This crate is `fx-refactor-parser`. `tally_csv_ints` in `src/lib.rs` \
                parses each comma-separated field inline. Extract that per-field parsing into a \
                new public function named exactly `parse_int_field(field: &str) -> i64` (trim \
                the field, parse it, default to 0 on error) and call it for each field; \
                `tally_csv_ints`'s behavior must not change. Do not change `tests/behavior.rs` \
                or `tests/structure.rs`.",
            check: Check::CargoTest,
        },
        // ---------------------------------------------------------- Scripting
        Brief {
            id: "scripting-wordcount",
            kind: TaskKind::Scripting,
            version: 1,
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
        },
        Brief {
            id: "scripting-greet",
            kind: TaskKind::Scripting,
            version: 1,
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
        },
        // --------------------------------------------------------------- Docs
        Brief {
            id: "docs-api",
            kind: TaskKind::Docs,
            version: 1,
            instruction: "Create `docs/API.md` documenting a function with this exact \
                signature: `submit_order(order_id: u64, amount_cents: u64) -> Result<String, \
                String>`. State: the function's exact name and signature; that `amount_cents` \
                is in cents, never dollars; and that an `Err` means no order was created. Keep \
                the whole file to 120 words or fewer.",
            check: Check::Docs {
                file: "docs/API.md",
                required: &[
                    "submit_order(order_id: u64, amount_cents: u64) -> Result<String, String>",
                    "cents",
                    "Err",
                ],
                max_words: 120,
            },
        },
        Brief {
            id: "docs-changelog",
            kind: TaskKind::Docs,
            version: 1,
            instruction: "Create `CHANGES.md` with one entry: version `0.2.0`, dated \
                `2026-09-28`, summarizing that a crash on an empty input file was fixed. The \
                file must state the version number, the date, and the word \"crash\". Keep the \
                whole file to 60 words or fewer.",
            check: Check::Docs {
                file: "CHANGES.md",
                required: &["0.2.0", "2026-09-28", "crash"],
                max_words: 60,
            },
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
