//! Reads the plain-text output of a Rust test binary (libtest, run with
//! `--test-threads=1`), line by line as it streams.
//!
//! Stable Rust has no machine-readable libtest format (`--format json` needs
//! `-Z unstable-options`), so the text is parsed. A test that starts a child
//! process can have the child's output land between `test name ... ` and its
//! result, so a result word on a later line of its own still closes the open
//! test.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed,
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TestOutcome {
    pub name: String,
    pub outcome: Outcome,
}

/// The `test result:` line's counts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub filtered_out: u64,
}

/// Streaming parser state for one binary's output.
#[derive(Debug, Default)]
pub struct Parser {
    pub tests: Vec<TestOutcome>,
    /// The `running N tests` count, when seen.
    pub announced: Option<u64>,
    pub summary: Option<Summary>,
    /// Failure text by test name, from the `---- name stdout ----` sections.
    pub failure_text: Vec<(String, String)>,
    /// The test whose result has not been printed yet.
    pub open: Option<String>,
    section: Option<(String, Vec<String>)>,
}

/// What one line changed, for the progress display.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Started(String),
    Finished(Outcome),
    Other,
}

/// Lines kept per failure section; the tail is what explains a failure.
const SECTION_LINES: usize = 60;

impl Parser {
    pub fn feed(&mut self, raw: &str) -> Event {
        let line = raw.trim_end_matches(['\r', '\n']);
        if let Some((name, lines)) = &mut self.section {
            // A failure section ends at the next section header or at the
            // `failures:` list that follows the last one.
            if !(line.starts_with("---- ") || line == "failures:") {
                lines.push(line.to_owned());
                if lines.len() > SECTION_LINES * 2 {
                    lines.drain(..SECTION_LINES);
                }
                return Event::Other;
            }
            let text = tail_lines(lines, SECTION_LINES);
            self.failure_text.push((std::mem::take(name), text));
            self.section = None;
        }
        if let Some(rest) = line.strip_prefix("---- ")
            && let Some(name) = rest
                .strip_suffix(" stdout ----")
                .or_else(|| rest.strip_suffix(" stderr ----"))
        {
            self.section = Some((name.to_owned(), Vec::new()));
            return Event::Other;
        }
        if let Some(rest) = line.strip_prefix("running ")
            && let Some(n) = rest
                .strip_suffix(" tests")
                .or_else(|| rest.strip_suffix(" test"))
            && let Ok(n) = n.parse()
        {
            self.announced = Some(n);
            return Event::Other;
        }
        if let Some(rest) = line.strip_prefix("test result: ") {
            self.summary = Some(parse_summary(rest));
            return Event::Other;
        }
        if let Some(rest) = line.strip_prefix("test ")
            && let Some((name, tail)) = rest.split_once(" ... ")
        {
            self.open = Some(name.to_owned());
            if let Some(outcome) = result_word(tail) {
                return self.close(outcome);
            }
            return Event::Started(name.to_owned());
        }
        if self.open.is_some()
            && let Some(outcome) = result_word(line)
        {
            return self.close(outcome);
        }
        Event::Other
    }

    fn close(&mut self, outcome: Outcome) -> Event {
        if let Some(name) = self.open.take() {
            self.tests.push(TestOutcome { name, outcome });
        }
        Event::Finished(outcome)
    }

    /// Call at the end of the output: a section still open is kept.
    pub fn finish(&mut self) {
        if let Some((name, lines)) = self.section.take() {
            let text = tail_lines(&lines, SECTION_LINES);
            self.failure_text.push((name, text));
        }
    }

    pub fn count(&self, outcome: Outcome) -> u64 {
        self.tests.iter().filter(|t| t.outcome == outcome).count() as u64
    }
}

/// `ok`, `FAILED`, `ignored` or `ignored, <reason>` at the end of a line.
fn result_word(text: &str) -> Option<Outcome> {
    let t = text.trim();
    if t == "ok" || t.ends_with(" ok") {
        Some(Outcome::Passed)
    } else if t == "FAILED" || t.ends_with(" FAILED") {
        Some(Outcome::Failed)
    } else if t == "ignored" || t.starts_with("ignored, ") || t.ends_with(" ignored") {
        Some(Outcome::Ignored)
    } else {
        None
    }
}

/// `ok. 10 passed; 1 failed; 2 ignored; 0 measured; 3 filtered out; finished in 1.2s`
fn parse_summary(rest: &str) -> Summary {
    let mut s = Summary::default();
    for part in rest.split(';') {
        let words: Vec<&str> = part.split_whitespace().collect();
        let Some(n) = words.iter().find_map(|w| w.parse::<u64>().ok()) else {
            continue;
        };
        if part.contains("filtered out") {
            s.filtered_out = n;
        } else if part.contains("passed") {
            s.passed = n;
        } else if part.contains("failed") {
            s.failed = n;
        } else if part.contains("ignored") {
            s.ignored = n;
        }
    }
    s
}

/// Count the tests a `--list` run prints (`name: test`).
pub fn count_listed(listing: &str) -> u64 {
    listing
        .lines()
        .filter(|l| l.trim_end().ends_with(": test"))
        .count() as u64
}

/// The last `n` lines, joined.
pub fn tail_lines(lines: &[String], n: usize) -> String {
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The last `n` lines of a text.
pub fn tail_text(text: &str, n: usize) -> String {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    tail_lines(&lines, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "\nrunning 4 tests\n\
test a::one ... ok\n\
test a::two ... FAILED\n\
test a::three ... ignored, needs Docker\n\
test a::four ... child said hello\n\
more child output\n\
ok\n\
\n\
failures:\n\
\n\
---- a::two stdout ----\n\
thread 'a::two' panicked at src/a.rs:9:5:\n\
assertion failed: x\n\
\n\
failures:\n\
    a::two\n\
\n\
test result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 7 filtered out; finished in 0.52s\n";

    fn parse(text: &str) -> Parser {
        let mut p = Parser::default();
        for l in text.lines() {
            p.feed(l);
        }
        p.finish();
        p
    }

    #[test]
    fn every_result_is_read_including_one_split_by_child_output() {
        let p = parse(OUTPUT);
        assert_eq!(p.announced, Some(4));
        let names: Vec<(&str, Outcome)> = p
            .tests
            .iter()
            .map(|t| (t.name.as_str(), t.outcome))
            .collect();
        assert_eq!(
            names,
            vec![
                ("a::one", Outcome::Passed),
                ("a::two", Outcome::Failed),
                ("a::three", Outcome::Ignored),
                ("a::four", Outcome::Passed),
            ]
        );
        assert!(p.open.is_none());
    }

    #[test]
    fn the_summary_and_the_failure_text_are_kept() {
        let p = parse(OUTPUT);
        assert_eq!(
            p.summary,
            Some(Summary {
                passed: 2,
                failed: 1,
                ignored: 1,
                filtered_out: 7
            })
        );
        assert_eq!(p.failure_text.len(), 1);
        assert_eq!(p.failure_text[0].0, "a::two");
        assert!(p.failure_text[0].1.contains("assertion failed: x"));
        assert!(!p.failure_text[0].1.contains("failures:"));
    }

    #[test]
    fn a_hang_leaves_the_running_test_open() {
        let p = parse("running 2 tests\ntest a::one ... ok\ntest a::slow ... ");
        assert_eq!(p.open.as_deref(), Some("a::slow"));
        assert_eq!(p.count(Outcome::Passed), 1);
    }

    #[test]
    fn listed_tests_are_counted() {
        let listing = "a::one: test\na::two: test\nbench_x: bench\n\n2 tests, 1 benchmark\n";
        assert_eq!(count_listed(listing), 2);
    }

    #[test]
    fn events_drive_the_progress_display() {
        let mut p = Parser::default();
        assert_eq!(p.feed("test x ... "), Event::Started("x".into()));
        assert_eq!(p.feed("ok"), Event::Finished(Outcome::Passed));
        assert_eq!(p.feed("ok"), Event::Other, "a stray ok closes nothing");
    }
}
