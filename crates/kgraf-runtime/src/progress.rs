//! Progress-gated round budgets (decision 0096).
//!
//! The round budget is a checkpoint, not a wall. A worker that is producing
//! results keeps going in blocks of the same size; a worker that is not is
//! stopped. Progress is measured from what happened in the worktree and in the
//! worker's own tool results, never from what the model says about its work.
//!
//! **Where a checkpoint falls.** A block is the brief's round budget `B`. The
//! first checkpoint is measured between rounds when `R = min(3, B - 1)` rounds of
//! the block are left: the point where the step budget would warn the model that
//! three rounds remain ([`crate::step::WARN_AT_ROUNDS_LEFT`]). Measuring there
//! keeps a run that ends exactly as before (the warning, then the final answer
//! round), and a run that goes on never hears a warning that was not true. A
//! block that is granted adds `B` rounds, so later checkpoints fall every `B`
//! rounds.
//!
//! **The signals, all measured since the previous checkpoint:**
//!
//! - **(a) Net file change.** Every file the worker wrote, git lists as dirty,
//!   or the previous checkpoint recorded is hashed (blake3). A file counts when
//!   its content differs from the previous checkpoint's record. A file the
//!   previous checkpoint did not record is compared with its content just
//!   before the block's first write to it, or, when no such capture exists, by
//!   whether git lists it as dirty now. A file written and then put back counts
//!   as no change.
//! - **(b) Checks improving.** A command a worker ran (the `shell` tool, or any
//!   tool whose input names a `command` or `cmd`) is scored from its output:
//!   pass and fail counts for `cargo test` and `cargo nextest`, pytest,
//!   unittest, `node --test`, jest and vitest; error counts for cargo builds and
//!   `tsc`; otherwise its exit status. The command's latest score in the block
//!   is compared with its score at the previous checkpoint, or with its first
//!   score in the block when it had none.
//! - **(c) No sloppy pattern.** In the block: one identical error (digits
//!   ignored) from a non-command tool [`REPEATED_ERROR_LIMIT`] times; at least
//!   [`FAILED_WRITE_LIMIT`] failed write calls and more failed than succeeded;
//!   files written and put back with no net change; one identical call with an
//!   identical, unmasked result [`REPEATED_CALL_LIMIT`] times (the stall
//!   detector's call digest).
//!
//! A block is progressing when (a) or (b) holds and (c) finds nothing.
//!
//! **The first block is measured harder.** Its (a) compares with the empty
//! start, so any early write would count, and a worker that wrote its files in
//! round 2 and then rewrote them with its test run flat at the same failure
//! was extended block after block. In the first block (a) counts only files
//! that differ from their content at the block's midpoint (the worker captures
//! it), and when the block ran a test or build command, none of its commands
//! improved and every one of them ended failing, file changes alone do not
//! count: the measure lists them as `files_discounted`. A command that ended
//! passing leaves (a) as it is. With no midpoint capture (a resumed attempt, a
//! block too short to have one) (a) falls back to the compare with the start.
//!
//! **Verdicts** (one per checkpoint):
//!
//! ```text
//!   progressing, below the ceiling ---------------------> Continue (+B rounds)
//!   progressing, at the ceiling ------------------------> Ceiling  (the old wall)
//!   not progressing, no productive checkpoint yet ------> EndNoProgress (the old wall)
//!   not progressing, the previous one was productive ---> Grace (+B rounds)
//!   not progressing, the previous one was not ----------> EndStuck (BudgetExhausted now)
//! ```
//!
//! A worker that never measured progress meets its round budget as before: the
//! warning, then the final answer round. A worker that was productive and then
//! shows no progress at two checkpoints in a row ends at once with
//! `BudgetExhausted`, without an answer round: an answer round would let model
//! text turn a stuck attempt into a completion.
//!
//! Written new. Searched: mini-swe-agent (a fixed step limit), SWE-agent (a cost
//! limit and an autosubmit), OpenHands (a fixed iteration limit with a stuck
//! detector over repeated actions), codex-rs and goose (turn limits). None
//! extends a budget from measured worktree and test progress.

use crate::event::WorkerEvent;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The default number of checkpoints a worker may reach: at most this many
/// blocks of rounds. The last checkpoint ends the run with the old wall's
/// final answer round, whatever it measures.
pub const DEFAULT_MAX_CHECKPOINTS: u32 = 10;
/// One identical error from a non-command tool, this many times in a block,
/// is a sloppy pattern.
pub const REPEATED_ERROR_LIMIT: u32 = 3;
/// Failed write calls in a block, at least this many and more than the
/// write calls that succeeded, are a sloppy pattern.
pub const FAILED_WRITE_LIMIT: u32 = 3;
/// One identical call with an identical, unmasked result, this many times in a
/// block, is a sloppy pattern.
pub const REPEATED_CALL_LIMIT: u32 = 3;

/// How a check run was scored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// A test runner's pass and fail counts.
    Tests,
    /// A compiler's error count (`failed`); `passed` is unused.
    BuildErrors,
    /// Only the exit status could be read.
    ExitStatus,
}

/// One check run's score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckScore {
    pub kind: CheckKind,
    pub passed: u64,
    /// Failed tests (errors included), or compiler errors.
    pub failed: u64,
    pub exit_ok: bool,
}

impl CheckScore {
    /// Whether this score is better than `before`, the same command's earlier
    /// score.
    pub fn improves_on(&self, before: &CheckScore) -> bool {
        use CheckKind::{BuildErrors, ExitStatus, Tests};
        match (before.kind, self.kind) {
            (Tests, Tests) => {
                (self.failed < before.failed && self.passed >= before.passed)
                    || (self.failed <= before.failed && self.passed > before.passed)
            }
            (BuildErrors, BuildErrors) => self.failed < before.failed,
            // The build now gets as far as running tests.
            (BuildErrors, Tests) => true,
            (Tests, BuildErrors) => false,
            (ExitStatus, _) | (_, ExitStatus) => !before.exit_ok && self.exit_ok,
        }
    }

    /// Whether the run ended failing: failed tests or compiler errors, or a
    /// nonzero exit.
    fn failing(&self) -> bool {
        match self.kind {
            CheckKind::Tests | CheckKind::BuildErrors => self.failed > 0 || !self.exit_ok,
            CheckKind::ExitStatus => !self.exit_ok,
        }
    }

    fn describe(&self) -> String {
        match self.kind {
            CheckKind::Tests => format!("{} passed, {} failed", self.passed, self.failed),
            CheckKind::BuildErrors => format!("{} errors", self.failed),
            CheckKind::ExitStatus if self.exit_ok => "exit 0".to_owned(),
            CheckKind::ExitStatus => "nonzero exit".to_owned(),
        }
    }
}

/// The command a call ran, as one line, when the call runs a command: the
/// `shell` tool's argv as the tool reads it, or a `command`/`cmd` string.
pub fn command_of(tool: &str, input: &serde_json::Value) -> Option<String> {
    if tool == "shell" {
        return kgraf_tools::command_argv(input).map(|argv| argv.join(" "));
    }
    ["command", "cmd"].iter().find_map(|key| {
        input
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
    })
}

/// The key a command's scores are kept under: a digest, so the record
/// carries no command text; and a short label (its first two words,
/// persist-scrubbed) to show.
fn command_key(command: &str) -> (String, String) {
    let digest = blake3::hash(command.as_bytes()).to_hex()[..16].to_owned();
    let label = command
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let label = kgraf_secrets::scrub_persist(&label).into_owned();
    (digest, label)
}

fn re(pattern: &str) -> regex::Regex {
    // The patterns are constants in this file; a bad one is a build defect
    // the unit tests below catch.
    regex::Regex::new(pattern).unwrap_or_else(|e| panic!("bad check pattern {pattern}: {e}"))
}

static EXIT_STATUS: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^exit status: (-?\d+)"));
static CARGO_TEST: LazyLock<regex::Regex> =
    LazyLock::new(|| re(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed"));
static NEXTEST: LazyLock<regex::Regex> =
    LazyLock::new(|| re(r"\d+ tests? run: (\d+) passed(?:[^\n]*?, (\d+) failed)?"));
static PYTEST_LINE: LazyLock<regex::Regex> = LazyLock::new(|| {
    re(
        r"(?m)^[=\s]*((?:\d+ (?:passed|failed|errors?|skipped|xfailed|xpassed|warnings?|deselected)(?:, )?)+) in [\d.]+s",
    )
});
static COUNT_WORD: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(\d+) ([a-z]+)"));
static UNITTEST_RAN: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^Ran (\d+) tests? in "));
static UNITTEST_FAILED: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^FAILED \(([^)]*)\)"));
static UNITTEST_OK: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^OK\b"));
static NODE_PASS: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^(?:#|ℹ) pass (\d+)"));
static NODE_FAIL: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^(?:#|ℹ) fail (\d+)"));
static JS_TESTS: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^\s*Tests:?\s+(.*\d.*)$"));
static RUST_ERROR: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^error(?:\[E\d+\])?: (.*)$"));
static CARGO_FINISHED: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^\s*Finished `?[a-z]"));
static TSC_FOUND: LazyLock<regex::Regex> = LazyLock::new(|| re(r"Found (\d+) errors?"));

fn number(m: Option<regex::Match<'_>>) -> u64 {
    m.and_then(|m| m.as_str().parse().ok()).unwrap_or(0)
}

/// Score a command's output. Test counts first (a test run's output can hold
/// compiler lines), then compiler errors, then the exit status (the shell
/// tool's `exit status: N` line, or the result's error flag).
pub fn check_score(output: &str, is_error: bool) -> CheckScore {
    let exit_ok = match EXIT_STATUS.captures(output) {
        Some(c) => c.get(1).is_some_and(|m| m.as_str() == "0"),
        None => !is_error,
    };
    let tests = |passed: u64, failed: u64| CheckScore {
        kind: CheckKind::Tests,
        passed,
        failed,
        exit_ok,
    };
    // cargo test: one summary per test binary.
    let mut found = false;
    let (mut passed, mut failed) = (0, 0);
    for c in CARGO_TEST.captures_iter(output) {
        found = true;
        passed += number(c.get(1));
        failed += number(c.get(2));
    }
    if found {
        return tests(passed, failed);
    }
    if let Some(c) = NEXTEST.captures(output) {
        return tests(number(c.get(1)), number(c.get(2)));
    }
    if let Some(c) = PYTEST_LINE.captures_iter(output).last() {
        let (mut passed, mut failed) = (0, 0);
        for w in COUNT_WORD.captures_iter(c.get(1).map_or("", |m| m.as_str())) {
            let n = number(w.get(1));
            match w.get(2).map_or("", |m| m.as_str()) {
                "passed" => passed += n,
                "failed" | "error" | "errors" => failed += n,
                _ => {}
            }
        }
        return tests(passed, failed);
    }
    if let Some(c) = UNITTEST_RAN.captures(output) {
        let ran = number(c.get(1));
        let mut failed = 0;
        let mut skipped = 0;
        if let Some(f) = UNITTEST_FAILED.captures(output) {
            for part in f.get(1).map_or("", |m| m.as_str()).split(',') {
                let (name, n) = part.trim().split_once('=').unwrap_or(("", "0"));
                let n: u64 = n.trim().parse().unwrap_or(0);
                match name {
                    "failures" | "errors" => failed += n,
                    "skipped" | "expected failures" => skipped += n,
                    _ => {}
                }
            }
        } else if !UNITTEST_OK.is_match(output) {
            // Ran, and neither verdict line: not a unittest summary.
            return CheckScore {
                kind: CheckKind::ExitStatus,
                passed: 0,
                failed: 0,
                exit_ok,
            };
        }
        return tests(ran.saturating_sub(failed + skipped), failed);
    }
    if let (Some(p), Some(f)) = (NODE_PASS.captures(output), NODE_FAIL.captures(output)) {
        return tests(number(p.get(1)), number(f.get(1)));
    }
    if let Some(c) = JS_TESTS.captures_iter(output).last() {
        let line = c.get(1).map_or("", |m| m.as_str());
        if line.contains("passed") || line.contains("failed") {
            let (mut passed, mut failed) = (0, 0);
            for w in COUNT_WORD.captures_iter(line) {
                let n = number(w.get(1));
                match w.get(2).map_or("", |m| m.as_str()) {
                    "passed" => passed += n,
                    "failed" => failed += n,
                    _ => {}
                }
            }
            return tests(passed, failed);
        }
    }
    let errors = RUST_ERROR
        .captures_iter(output)
        .filter(|c| {
            let rest = c.get(1).map_or("", |m| m.as_str());
            !rest.starts_with("could not compile") && !rest.starts_with("aborting due to")
        })
        .count() as u64;
    if errors > 0 {
        return CheckScore {
            kind: CheckKind::BuildErrors,
            passed: 0,
            failed: errors,
            exit_ok,
        };
    }
    if let Some(c) = TSC_FOUND.captures(output) {
        return CheckScore {
            kind: CheckKind::BuildErrors,
            passed: 0,
            failed: number(c.get(1)),
            exit_ok,
        };
    }
    if exit_ok && CARGO_FINISHED.is_match(output) {
        return CheckScore {
            kind: CheckKind::BuildErrors,
            passed: 0,
            failed: 0,
            exit_ok,
        };
    }
    CheckScore {
        kind: CheckKind::ExitStatus,
        passed: 0,
        failed: 0,
        exit_ok,
    }
}

/// A sloppy pattern found in a block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "pattern", rename_all = "snake_case")]
pub enum SloppyPattern {
    /// One identical error from `tool`, `times` times.
    RepeatedError { tool: String, times: u32 },
    /// Write calls that failed, against those that succeeded.
    FailedWrites { failed: u32, succeeded: u32 },
    /// Files written in the block and back at their earlier content, with no
    /// net change anywhere.
    WriteThenRevert { paths: Vec<PathBuf> },
    /// One identical call of `tool` with an identical result, `times` times.
    RepeatedCalls { tool: String, times: u32 },
}

/// What one checkpoint measured over its block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressMeasure {
    /// Model rounds in the block.
    pub rounds: u32,
    /// Files whose content differs from the previous checkpoint's (in the
    /// first block: from their content at the block's midpoint).
    pub files_changed: Vec<PathBuf>,
    /// Files that changed but were not counted, because the first block ran
    /// a test or build command that ended failing and none improved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_discounted: Vec<PathBuf>,
    /// Commands whose score improved, each as `label: before -> after`.
    pub checks_improved: Vec<String>,
    pub sloppy: Vec<SloppyPattern>,
}

impl ProgressMeasure {
    /// (a) or (b), and not (c).
    pub fn progressing(&self) -> bool {
        (!self.files_changed.is_empty() || !self.checks_improved.is_empty())
            && self.sloppy.is_empty()
    }
}

/// What a checkpoint decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointVerdict {
    /// Progressing: another block.
    Continue,
    /// Not progressing, after a productive checkpoint: one more block.
    Grace,
    /// Not progressing, and no checkpoint ever was: the old wall (the
    /// warning and the final answer round).
    EndNoProgress,
    /// Not progressing at two checkpoints in a row: the attempt ends now,
    /// `BudgetExhausted`, without an answer round.
    EndStuck,
    /// The checkpoint ceiling: the old wall, whatever was measured.
    Ceiling,
}

impl CheckpointVerdict {
    /// Whether the verdict grants another block of rounds.
    pub fn grants(self) -> bool {
        matches!(self, Self::Continue | Self::Grace)
    }
}

/// One checkpoint, as the result and the transcript report it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 1-based.
    pub number: u32,
    /// Model rounds used when it was measured.
    pub at_round: u32,
    pub measure: ProgressMeasure,
    pub verdict: CheckpointVerdict,
    /// Rounds added (zero when the verdict ends the run).
    pub rounds_granted: u32,
}

impl Checkpoint {
    /// One line for a person.
    pub fn render(&self) -> String {
        let mut parts = vec![format!(
            "checkpoint {} at round {}: {:?}",
            self.number, self.at_round, self.verdict
        )];
        parts.push(format!(
            "{} file(s) changed",
            self.measure.files_changed.len()
        ));
        if !self.measure.files_discounted.is_empty() {
            parts.push(format!(
                "{} changed file(s) not counted: the checks ran failing and none improved",
                self.measure.files_discounted.len()
            ));
        }
        if !self.measure.checks_improved.is_empty() {
            parts.push(format!(
                "checks improved: {}",
                self.measure.checks_improved.join("; ")
            ));
        }
        if !self.measure.sloppy.is_empty() {
            parts.push(format!("sloppy: {:?}", self.measure.sloppy));
        }
        if self.rounds_granted > 0 {
            parts.push(format!("{} rounds granted", self.rounds_granted));
        }
        parts.join(", ")
    }
}

/// What the next checkpoint compares against: each tracked file's content
/// digest (`None`: absent), and each command's latest score by key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointBaseline {
    pub files: BTreeMap<PathBuf, Option<String>>,
    pub checks: BTreeMap<String, CheckScore>,
}

/// The checkpoints a worker reached, on its result (decision 0096).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundBlocks {
    /// Rounds per block: the brief's round budget.
    pub block_rounds: u32,
    /// The most checkpoints the worker could reach.
    pub ceiling: u32,
    pub checkpoints: Vec<Checkpoint>,
}

impl RoundBlocks {
    /// Whether the worker ran past its planned round budget: the contract was
    /// bigger than planned.
    pub fn extended(&self) -> bool {
        self.checkpoints.iter().any(|c| c.verdict.grants())
    }

    /// The verdict that ended the run, when a checkpoint did.
    pub fn ended_by(&self) -> Option<CheckpointVerdict> {
        self.checkpoints
            .last()
            .map(|c| c.verdict)
            .filter(|v| !v.grants())
    }
}

/// The digest of a file's content, `None` when it is absent. A directory or
/// an unreadable entry digests as a marker, so a change between the two is
/// still seen.
pub fn file_digest(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_dir() {
        return Some("dir".to_owned());
    }
    match std::fs::read(path) {
        Ok(bytes) => Some(blake3::hash(&bytes).to_hex().to_string()),
        Err(e) => Some(format!("unreadable: {}", e.kind())),
    }
}

/// Digest every path under `root` (paths are relative to it, or absolute).
pub fn digests(root: &Path, paths: &BTreeSet<PathBuf>) -> BTreeMap<PathBuf, Option<String>> {
    paths
        .iter()
        .map(|p| (p.clone(), file_digest(&root.join(p))))
        .collect()
}

/// What a call's tool declares and what the masker does, for the block log.
pub(crate) trait CallFacts {
    /// The paths the call declares it writes (empty for a call that declares
    /// none, a command included).
    fn written_paths(&self, tool: &str, input: &serde_json::Value) -> Vec<PathBuf>;
    /// Whether the masker hides this result once it leaves the window.
    fn hides(&self, output: &str, is_error: bool) -> bool;
}

/// Whether a result says the call did nothing, or nothing reliable: denied,
/// refused, invalid, never run, or cut off by a steer.
fn did_not_run(output: &str) -> bool {
    output.starts_with(crate::worker::DENIED_PREFIX)
        || output.starts_with(crate::worker::REFUSED_PREFIX)
        || output.starts_with(crate::worker::INVALID_CALL_PREFIX)
        || output.starts_with("ERROR: not run")
        || output.starts_with(crate::worker::STEER_INTERRUPTED)
        || output.starts_with(crate::rebuild::INTERRUPTED_RESULT)
}

/// The text an error compares by: whitespace runs as one space, every digit
/// as `#`, so line numbers and counts do not make one error two.
fn error_shape(output: &str) -> String {
    output
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect()
}

/// What happened in the block so far, from the worker's own events.
#[derive(Clone, Debug, Default)]
struct BlockLog {
    rounds_at_start: u32,
    started: HashMap<String, (String, serde_json::Value)>,
    /// Error shape digest -> (tool, count).
    errors: HashMap<String, (String, u32)>,
    /// Call digest plus result digest -> (tool, count).
    calls: HashMap<String, (String, u32)>,
    failed_writes: u32,
    ok_writes: u32,
    written: BTreeSet<PathBuf>,
    /// (key, label, score), in run order.
    checks: Vec<(String, String, CheckScore)>,
    /// Content of a path just before the block's first write to it, for a
    /// path the baseline does not record.
    preimages: BTreeMap<PathBuf, Option<String>>,
    /// Content digests of the tracked paths at the first block's midpoint.
    midpoint: Option<BTreeMap<PathBuf, Option<String>>>,
}

impl BlockLog {
    fn observe(&mut self, event: &WorkerEvent, facts: &dyn CallFacts) {
        match event {
            WorkerEvent::ToolCallStarted {
                call_id,
                tool,
                input,
                ..
            } => {
                self.started
                    .insert(call_id.clone(), (tool.clone(), input.clone()));
            }
            WorkerEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
            } => {
                let Some((tool, input)) = self.started.remove(call_id) else {
                    return;
                };
                if did_not_run(output) {
                    return;
                }
                if let Some(command) = command_of(&tool, &input) {
                    let (key, label) = command_key(&command);
                    self.checks
                        .push((key, label, check_score(output, *is_error)));
                } else if *is_error {
                    let digest =
                        blake3::hash(format!("{tool}\u{0}{}", error_shape(output)).as_bytes())
                            .to_hex()
                            .to_string();
                    self.errors.entry(digest).or_insert((tool.clone(), 0)).1 += 1;
                }
                let paths = facts.written_paths(&tool, &input);
                if !paths.is_empty() {
                    if *is_error {
                        self.failed_writes += 1;
                    } else {
                        self.ok_writes += 1;
                        self.written.extend(paths);
                    }
                }
                if !facts.hides(output, *is_error) {
                    let digest = format!(
                        "{}:{}",
                        crate::stall::call_digest(&tool, &input),
                        blake3::hash(output.as_bytes()).to_hex()
                    );
                    self.calls.entry(digest).or_insert((tool, 0)).1 += 1;
                }
            }
            _ => {}
        }
    }
}

/// The progress gate of one worker.
#[derive(Clone, Debug)]
pub(crate) struct ProgressGate {
    block: u32,
    ceiling: u32,
    /// Rounds used at which the next checkpoint is measured.
    next_at: u32,
    checkpoints: Vec<Checkpoint>,
    baseline: CheckpointBaseline,
    log: BlockLog,
}

impl ProgressGate {
    /// A gate for blocks of `block` rounds (the brief's round budget, at least
    /// 2) and at most `ceiling` checkpoints (0 is read as 1: the old wall).
    pub(crate) fn new(block: u32, ceiling: u32) -> Self {
        let block = block.max(crate::brief::MIN_STEP_BUDGET);
        let reserve = crate::step::WARN_AT_ROUNDS_LEFT.min(block - 1);
        Self {
            block,
            ceiling: ceiling.max(1),
            next_at: block - reserve,
            checkpoints: Vec::new(),
            baseline: CheckpointBaseline::default(),
            log: BlockLog::default(),
        }
    }

    /// Rebuild from a stored transcript: the checkpoints it records, then the
    /// events after the last one, as the live loop observed them. A capture of
    /// a file's content before a write is not stored, so a file first written
    /// in the interrupted block, with no git status to read, counts as changed.
    pub(crate) fn resume(&mut self, events: &[WorkerEvent], facts: &dyn CallFacts) {
        let last = events
            .iter()
            .rposition(|e| matches!(e, WorkerEvent::Checkpoint { .. }));
        for event in events {
            if let WorkerEvent::Checkpoint {
                checkpoint,
                baseline,
            } = event
            {
                self.record(checkpoint.as_ref().clone(), baseline.as_ref().clone());
            }
        }
        let after = last.map_or(0, |i| i + 1);
        let mut rounds = self.checkpoints.last().map_or(0, |c| c.at_round);
        for event in &events[after..] {
            if matches!(event, WorkerEvent::ModelCall { .. }) {
                rounds += 1;
            }
            self.log.observe(event, facts);
        }
        // The rounds of the interrupted block are counted from its own
        // `ModelCall`s; the log's start is the checkpoint's round.
        self.log.rounds_at_start = rounds.saturating_sub(
            events[after..]
                .iter()
                .filter(|e| matches!(e, WorkerEvent::ModelCall { .. }))
                .count() as u32,
        );
    }

    fn record(&mut self, checkpoint: Checkpoint, baseline: CheckpointBaseline) {
        self.next_at += checkpoint.rounds_granted;
        self.log = BlockLog {
            rounds_at_start: checkpoint.at_round,
            ..BlockLog::default()
        };
        self.baseline = baseline;
        self.checkpoints.push(checkpoint);
    }

    /// Rounds per block.
    pub(crate) fn block(&self) -> u32 {
        self.block
    }

    /// Rounds the checkpoints so far added to the brief's budget.
    pub(crate) fn rounds_granted(&self) -> u32 {
        self.checkpoints.iter().map(|c| c.rounds_granted).sum()
    }

    /// Whether a checkpoint ended the run's extension: no more are measured.
    pub(crate) fn ended(&self) -> bool {
        self.checkpoints.last().is_some_and(|c| !c.verdict.grants())
    }

    /// Whether a checkpoint is due before the next round.
    pub(crate) fn due(&self, used: u32) -> bool {
        !self.ended() && used >= self.next_at
    }

    pub(crate) fn observe(&mut self, event: &WorkerEvent, facts: &dyn CallFacts) {
        self.log.observe(event, facts);
    }

    /// The round at which the first block's midpoint is captured.
    fn midpoint_round(&self) -> u32 {
        (self.next_at / 2).max(1)
    }

    /// Whether the first block's midpoint capture is due: the first block only,
    /// once, after its midpoint and before its checkpoint.
    pub(crate) fn midpoint_due(&self, used: u32) -> bool {
        self.checkpoints.is_empty()
            && self.log.midpoint.is_none()
            && used >= self.midpoint_round()
            && used < self.next_at
    }

    pub(crate) fn capture_midpoint(&mut self, digests: BTreeMap<PathBuf, Option<String>>) {
        self.log.midpoint = Some(digests);
    }

    /// Paths whose content before a write must be captured: declared write
    /// paths the baseline does not record and the block has not captured.
    pub(crate) fn needs_preimage(&self, path: &Path) -> bool {
        !self.baseline.files.contains_key(path) && !self.log.preimages.contains_key(path)
    }

    pub(crate) fn capture_preimage(&mut self, path: PathBuf, digest: Option<String>) {
        self.log.preimages.entry(path).or_insert(digest);
    }

    /// Every path the next measurement digests: the baseline's, the block's
    /// captures and writes, and `changed` (the worker's changed paths).
    pub(crate) fn tracked(&self, changed: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
        let mut paths: BTreeSet<PathBuf> = self.baseline.files.keys().cloned().collect();
        paths.extend(self.log.preimages.keys().cloned());
        paths.extend(self.log.written.iter().cloned());
        paths.extend(changed.iter().cloned());
        paths
    }

    /// Measure the block from `now` (digests of every tracked path, dirty ones
    /// included) and `dirty` (what git lists as changed, when it could be
    /// read). Pure: the caller reads the files.
    pub(crate) fn measure(
        &self,
        used: u32,
        now: &BTreeMap<PathBuf, Option<String>>,
        dirty: Option<&BTreeSet<PathBuf>>,
    ) -> (ProgressMeasure, CheckpointBaseline) {
        let mut files_changed = Vec::new();
        let mut since_midpoint = Vec::new();
        let first_block = self.checkpoints.is_empty();
        let mut unchanged = BTreeSet::new();
        for (path, digest) in now {
            let changed = match self.baseline.files.get(path) {
                Some(before) => before != digest,
                None => match self.log.preimages.get(path) {
                    Some(before) => before != digest,
                    // Not recorded at the last checkpoint, so not dirty then:
                    // changed only if git lists it now. With no git status,
                    // counted as changed rather than guessed unchanged.
                    None => dirty.is_none_or(|d| d.contains(path)),
                },
            };
            if changed {
                files_changed.push(path.clone());
            } else {
                unchanged.insert(path.clone());
            }
            // The first block's (a): a net change from the start that also
            // differs from the midpoint capture or, for a path first written
            // after it, from its content before that write.
            if let (true, Some(mid)) = (first_block, &self.log.midpoint) {
                let moved = match mid.get(path).or_else(|| self.log.preimages.get(path)) {
                    Some(before) => before != digest,
                    None => changed,
                };
                if moved && changed {
                    since_midpoint.push(path.clone());
                }
            }
        }
        // `files_changed` keeps the compare with the block's start for the
        // revert check below; `counted` is what (a) counts.
        let mut counted = if first_block && self.log.midpoint.is_some() {
            since_midpoint
        } else {
            files_changed.clone()
        };
        // (b): each command's latest score against its reference.
        let mut checks = self.baseline.checks.clone();
        let mut checks_improved = Vec::new();
        // Latest score of each test or build command run in the block.
        let mut latest_scores: Vec<CheckScore> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for (key, _, _) in &self.log.checks {
            if !seen.contains(&key.as_str()) {
                seen.push(key);
            }
        }
        for key in seen {
            let runs: Vec<&(String, String, CheckScore)> = self
                .log
                .checks
                .iter()
                .filter(|(k, _, _)| k == key)
                .collect();
            let Some((_, label, latest)) = runs.last().copied() else {
                continue;
            };
            let reference = match self.baseline.checks.get(key) {
                Some(score) => Some(score),
                None if runs.len() > 1 => runs.first().map(|(_, _, s)| s),
                None => None,
            };
            if let Some(reference) = reference
                && latest.improves_on(reference)
            {
                checks_improved.push(format!(
                    "{label}: {} -> {}",
                    reference.describe(),
                    latest.describe()
                ));
            }
            if latest.kind != CheckKind::ExitStatus {
                latest_scores.push(*latest);
            }
            checks.insert(key.to_owned(), *latest);
        }
        // The first block: file changes alone do not count while the block's
        // test and build commands all ended failing and none improved.
        let mut files_discounted = Vec::new();
        if first_block
            && checks_improved.is_empty()
            && !latest_scores.is_empty()
            && latest_scores.iter().all(CheckScore::failing)
        {
            files_discounted = std::mem::take(&mut counted);
        }
        // (c).
        let mut sloppy = Vec::new();
        let mut repeated: Vec<&(String, u32)> = self
            .log
            .errors
            .values()
            .filter(|(_, n)| *n >= REPEATED_ERROR_LIMIT)
            .collect();
        repeated.sort();
        for (tool, times) in repeated {
            sloppy.push(SloppyPattern::RepeatedError {
                tool: tool.clone(),
                times: *times,
            });
        }
        if self.log.failed_writes >= FAILED_WRITE_LIMIT
            && self.log.failed_writes > self.log.ok_writes
        {
            sloppy.push(SloppyPattern::FailedWrites {
                failed: self.log.failed_writes,
                succeeded: self.log.ok_writes,
            });
        }
        let reverted: Vec<PathBuf> = self
            .log
            .written
            .iter()
            .filter(|p| unchanged.contains(*p))
            .cloned()
            .collect();
        if !reverted.is_empty() && files_changed.is_empty() {
            sloppy.push(SloppyPattern::WriteThenRevert { paths: reverted });
        }
        let mut calls: Vec<&(String, u32)> = self
            .log
            .calls
            .values()
            .filter(|(_, n)| *n >= REPEATED_CALL_LIMIT)
            .collect();
        calls.sort();
        for (tool, times) in calls {
            sloppy.push(SloppyPattern::RepeatedCalls {
                tool: tool.clone(),
                times: *times,
            });
        }
        let measure = ProgressMeasure {
            rounds: used.saturating_sub(self.log.rounds_at_start),
            files_changed: counted,
            files_discounted,
            checks_improved,
            sloppy,
        };
        let baseline = CheckpointBaseline {
            files: now.clone(),
            checks,
        };
        (measure, baseline)
    }

    /// Decide on a measured block, record the checkpoint and start the next
    /// block. Returns the checkpoint and the baseline it leaves.
    pub(crate) fn decide(
        &mut self,
        used: u32,
        measure: ProgressMeasure,
        baseline: CheckpointBaseline,
    ) -> (Checkpoint, CheckpointBaseline) {
        let number = self.checkpoints.len() as u32 + 1;
        let previous_productive = self
            .checkpoints
            .last()
            .is_some_and(|c| c.verdict == CheckpointVerdict::Continue);
        let verdict = if measure.progressing() {
            if number >= self.ceiling {
                CheckpointVerdict::Ceiling
            } else {
                CheckpointVerdict::Continue
            }
        } else if previous_productive {
            if number >= self.ceiling {
                CheckpointVerdict::Ceiling
            } else {
                CheckpointVerdict::Grace
            }
        } else if self
            .checkpoints
            .iter()
            .any(|c| c.verdict == CheckpointVerdict::Continue)
        {
            CheckpointVerdict::EndStuck
        } else {
            CheckpointVerdict::EndNoProgress
        };
        let checkpoint = Checkpoint {
            number,
            at_round: used,
            measure,
            verdict,
            rounds_granted: if verdict.grants() { self.block } else { 0 },
        };
        self.record(checkpoint.clone(), baseline.clone());
        (checkpoint, baseline)
    }

    /// The report for the result, when any checkpoint was reached.
    pub(crate) fn report(&self) -> Option<RoundBlocks> {
        (!self.checkpoints.is_empty()).then(|| RoundBlocks {
            block_rounds: self.block,
            ceiling: self.ceiling,
            checkpoints: self.checkpoints.clone(),
        })
    }
}

/// The notice a worker gets at a grace checkpoint.
pub fn grace_notice(block: u32) -> String {
    format!(
        "kGraf measured no progress since the last checkpoint: no file changed on net and no \
         check improved, or the same failure repeated. You have {block} more rounds. If the \
         next checkpoint measures no progress either, this attempt ends."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Facts;
    impl CallFacts for Facts {
        fn written_paths(&self, tool: &str, input: &serde_json::Value) -> Vec<PathBuf> {
            if tool == "write" {
                vec![PathBuf::from(input["path"].as_str().unwrap_or_default())]
            } else {
                Vec::new()
            }
        }
        fn hides(&self, _: &str, _: bool) -> bool {
            false
        }
    }

    fn call(
        log: &mut ProgressGate,
        id: &str,
        tool: &str,
        input: serde_json::Value,
        out: &str,
        err: bool,
    ) {
        log.observe(
            &WorkerEvent::ToolCallStarted {
                call_id: id.into(),
                tool: tool.into(),
                input,
                signature: None,
            },
            &Facts,
        );
        log.observe(
            &WorkerEvent::ToolCallFinished {
                call_id: id.into(),
                output: out.into(),
                is_error: err,
            },
            &Facts,
        );
    }

    #[test]
    fn scores_each_runner() {
        let s = check_score(
            "exit status: 101\nstdout:\ntest result: FAILED. 3 passed; 2 failed; 0 ignored\n\
             test result: ok. 4 passed; 0 failed; 0 ignored",
            true,
        );
        assert_eq!(
            (s.kind, s.passed, s.failed, s.exit_ok),
            (CheckKind::Tests, 7, 2, false)
        );
        let s = check_score("===== 2 failed, 5 passed, 1 error in 0.31s =====", true);
        assert_eq!((s.passed, s.failed), (5, 3));
        let s = check_score("3 passed in 0.02s", false);
        assert_eq!((s.kind, s.passed, s.failed), (CheckKind::Tests, 3, 0));
        let s = check_score(
            "Ran 6 tests in 0.001s\n\nFAILED (failures=1, errors=1)",
            true,
        );
        assert_eq!((s.passed, s.failed), (4, 2));
        let s = check_score("Ran 6 tests in 0.001s\n\nOK", false);
        assert_eq!((s.passed, s.failed), (6, 0));
        let s = check_score("# tests 4\n# pass 3\n# fail 1\n", true);
        assert_eq!((s.passed, s.failed), (3, 1));
        let s = check_score("ℹ tests 4\nℹ pass 4\nℹ fail 0\n", false);
        assert_eq!((s.passed, s.failed), (4, 0));
        let s = check_score("Tests:       1 failed, 5 passed, 6 total", true);
        assert_eq!((s.passed, s.failed), (5, 1));
        let s = check_score("      Tests  2 failed | 4 passed (6)", true);
        assert_eq!((s.passed, s.failed), (4, 2));
        let s = check_score(
            "exit status: 101\nstderr:\nerror[E0308]: mismatched types\nerror: cannot find value\n\
             error: could not compile `x` (lib) due to 2 previous errors",
            true,
        );
        assert_eq!((s.kind, s.failed), (CheckKind::BuildErrors, 2));
        let s = check_score("exit status: 0\nstderr:\n    Finished `dev` profile", false);
        assert_eq!((s.kind, s.failed), (CheckKind::BuildErrors, 0));
        let s = check_score("src/a.ts:1:1 - error TS2322\nFound 3 errors.", true);
        assert_eq!((s.kind, s.failed), (CheckKind::BuildErrors, 3));
        let s = check_score("exit status: 0\nstdout:\nhello", false);
        assert_eq!((s.kind, s.exit_ok), (CheckKind::ExitStatus, true));
        let s = check_score("exit status: 2\nstdout:\nhello", false);
        assert!(!s.exit_ok, "the status line outranks the error flag");
    }

    #[test]
    fn improvement_compares_like_with_like() {
        let t = |p, f| CheckScore {
            kind: CheckKind::Tests,
            passed: p,
            failed: f,
            exit_ok: f == 0,
        };
        let b = |f| CheckScore {
            kind: CheckKind::BuildErrors,
            passed: 0,
            failed: f,
            exit_ok: f == 0,
        };
        let x = |ok| CheckScore {
            kind: CheckKind::ExitStatus,
            passed: 0,
            failed: 0,
            exit_ok: ok,
        };
        assert!(t(5, 0).improves_on(&t(3, 2)));
        assert!(t(4, 0).improves_on(&t(4, 1)), "a failing test removed");
        assert!(t(6, 1).improves_on(&t(5, 1)));
        assert!(!t(5, 1).improves_on(&t(5, 1)));
        assert!(
            !t(2, 1).improves_on(&t(5, 2)),
            "fewer failures, fewer passes"
        );
        assert!(b(1).improves_on(&b(3)));
        assert!(!b(3).improves_on(&b(3)));
        assert!(t(0, 4).improves_on(&b(1)), "it builds and the tests run");
        assert!(!b(0).improves_on(&t(1, 0)));
        assert!(x(true).improves_on(&x(false)));
        assert!(!x(true).improves_on(&x(true)));
    }

    #[test]
    fn a_shell_call_is_a_command_and_a_patch_is_not() {
        assert_eq!(
            command_of("shell", &json!({"command": ["cargo", "test"]})).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            command_of("terminal", &json!({"command": "pytest  -q"})).as_deref(),
            Some("pytest -q")
        );
        assert_eq!(command_of("apply_patch", &json!({"patch": "x"})), None);
    }

    fn gate() -> ProgressGate {
        ProgressGate::new(10, 10)
    }

    fn files(pairs: &[(&str, Option<&str>)]) -> BTreeMap<PathBuf, Option<String>> {
        pairs
            .iter()
            .map(|(p, d)| (PathBuf::from(p), d.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn the_first_checkpoint_falls_where_the_warning_would() {
        assert_eq!(ProgressGate::new(30, 10).next_at, 27);
        assert_eq!(ProgressGate::new(4, 10).next_at, 1);
        assert_eq!(ProgressGate::new(2, 10).next_at, 1);
    }

    #[test]
    fn a_net_change_is_progress_and_a_revert_is_not() {
        let mut g = gate();
        g.capture_preimage("a.rs".into(), Some("v0".into()));
        call(&mut g, "1", "write", json!({"path": "a.rs"}), "ok", false);
        let (m, base) = g.measure(7, &files(&[("a.rs", Some("v1"))]), None);
        assert_eq!(m.files_changed, vec![PathBuf::from("a.rs")]);
        assert!(m.progressing());
        g.decide(7, m, base);
        // Next block: written twice, back to v1.
        call(
            &mut g,
            "2",
            "write",
            json!({"path": "a.rs", "n": 1}),
            "ok",
            false,
        );
        call(
            &mut g,
            "3",
            "write",
            json!({"path": "a.rs", "n": 2}),
            "ok",
            false,
        );
        let (m, _) = g.measure(17, &files(&[("a.rs", Some("v1"))]), None);
        assert!(m.files_changed.is_empty());
        assert_eq!(
            m.sloppy,
            vec![SloppyPattern::WriteThenRevert {
                paths: vec!["a.rs".into()]
            }]
        );
        assert!(!m.progressing());
        assert_eq!(m.rounds, 10);
    }

    #[test]
    fn an_untracked_path_without_a_capture_reads_the_git_status() {
        let g = gate();
        let now = files(&[("gen.txt", Some("x")), ("lock", Some("y"))]);
        let dirty: BTreeSet<PathBuf> = [PathBuf::from("gen.txt")].into();
        let (m, _) = g.measure(5, &now, Some(&dirty));
        assert_eq!(m.files_changed, vec![PathBuf::from("gen.txt")]);
        let (m, _) = g.measure(5, &now, None);
        assert_eq!(m.files_changed.len(), 2, "no status: counted as changed");
    }

    #[test]
    fn an_improving_check_is_progress_without_a_file_change() {
        let mut g = gate();
        let cmd = json!({"command": ["cargo", "test"]});
        call(
            &mut g,
            "1",
            "shell",
            cmd.clone(),
            "exit status: 101\ntest result: FAILED. 1 passed; 3 failed;",
            true,
        );
        call(
            &mut g,
            "2",
            "shell",
            cmd.clone(),
            "exit status: 101\ntest result: FAILED. 3 passed; 1 failed;",
            true,
        );
        let (m, base) = g.measure(7, &BTreeMap::new(), None);
        assert_eq!(
            m.checks_improved,
            vec!["cargo test: 1 passed, 3 failed -> 3 passed, 1 failed"]
        );
        assert!(m.progressing());
        g.decide(7, m, base);
        // One run in the next block, compared with the checkpoint's record.
        call(
            &mut g,
            "3",
            "shell",
            cmd.clone(),
            "exit status: 101\ntest result: FAILED. 3 passed; 1 failed;",
            true,
        );
        let (m, _) = g.measure(17, &BTreeMap::new(), None);
        assert!(m.checks_improved.is_empty());
        call(
            &mut g,
            "4",
            "shell",
            cmd,
            "exit status: 0\ntest result: ok. 4 passed; 0 failed;",
            false,
        );
        let (m, _) = g.measure(17, &BTreeMap::new(), None);
        assert_eq!(m.checks_improved.len(), 1);
    }

    #[test]
    fn sloppy_patterns_veto_progress() {
        let mut g = gate();
        g.capture_preimage("b.rs".into(), None);
        call(&mut g, "0", "write", json!({"path": "b.rs"}), "ok", false);
        for n in 0..3 {
            call(
                &mut g,
                &format!("p{n}"),
                "write",
                json!({"path": "a.rs", "hunk": n}),
                &format!("ERROR: context not found at line {n}"),
                true,
            );
        }
        let (m, _) = g.measure(7, &files(&[("b.rs", Some("new"))]), None);
        assert_eq!(m.files_changed.len(), 1);
        assert!(m.sloppy.contains(&SloppyPattern::RepeatedError {
            tool: "write".into(),
            times: 3
        }));
        assert!(m.sloppy.contains(&SloppyPattern::FailedWrites {
            failed: 3,
            succeeded: 1
        }));
        assert!(!m.progressing());
    }

    #[test]
    fn identical_calls_with_identical_results_are_sloppy() {
        let mut g = gate();
        for n in 0..3 {
            call(
                &mut g,
                &format!("r{n}"),
                "read",
                json!({"path": "a"}),
                "same",
                false,
            );
        }
        let (m, _) = g.measure(7, &BTreeMap::new(), None);
        assert_eq!(
            m.sloppy,
            vec![SloppyPattern::RepeatedCalls {
                tool: "read".into(),
                times: 3
            }]
        );
    }

    #[test]
    fn denied_and_unrun_calls_are_not_observed() {
        let mut g = gate();
        for n in 0..4 {
            call(
                &mut g,
                &format!("d{n}"),
                "write",
                json!({"path": "a.rs"}),
                &format!("{}hook: no", crate::worker::DENIED_PREFIX),
                true,
            );
        }
        let (m, _) = g.measure(7, &BTreeMap::new(), None);
        assert!(m.sloppy.is_empty());
    }

    fn measure(progress: bool) -> ProgressMeasure {
        ProgressMeasure {
            rounds: 10,
            files_changed: if progress {
                vec!["a".into()]
            } else {
                Vec::new()
            },
            ..ProgressMeasure::default()
        }
    }

    fn verdicts(seq: &[bool], ceiling: u32) -> Vec<CheckpointVerdict> {
        let mut g = ProgressGate::new(10, ceiling);
        let mut out = Vec::new();
        for (i, p) in seq.iter().enumerate() {
            if g.ended() {
                break;
            }
            let (c, _) = g.decide(
                i as u32 * 10 + 7,
                measure(*p),
                CheckpointBaseline::default(),
            );
            out.push(c.verdict);
        }
        out
    }

    #[test]
    fn verdicts_follow_the_grace_rule_and_the_ceiling() {
        use CheckpointVerdict::*;
        assert_eq!(verdicts(&[false, true], 10), vec![EndNoProgress]);
        assert_eq!(
            verdicts(&[true, false, true, false, false, true], 10),
            vec![Continue, Grace, Continue, Grace, EndStuck]
        );
        assert_eq!(
            verdicts(&[true, true, true], 3),
            vec![Continue, Continue, Ceiling]
        );
        assert_eq!(
            verdicts(&[true, true], 1),
            vec![Ceiling],
            "a ceiling of 1 is the old wall"
        );
        assert_eq!(verdicts(&[true, true], 0), vec![Ceiling]);
    }

    #[test]
    fn granted_rounds_move_the_next_checkpoint_by_a_block() {
        let mut g = ProgressGate::new(10, 10);
        assert!(!g.due(6));
        assert!(g.due(7));
        g.decide(7, measure(true), CheckpointBaseline::default());
        assert_eq!(g.rounds_granted(), 10);
        assert!(!g.due(16));
        assert!(g.due(17));
    }

    #[test]
    fn a_resume_restores_the_checkpoints_and_the_open_block() {
        let mut live = ProgressGate::new(10, 10);
        let (c, b) = live.decide(7, measure(true), CheckpointBaseline::default());
        let mut events = vec![WorkerEvent::Checkpoint {
            checkpoint: Box::new(c),
            baseline: Box::new(b),
        }];
        for n in 0..3 {
            events.push(WorkerEvent::ModelCall {
                call: kgraf_core::CallId::new(format!("c{n}")),
            });
            events.push(WorkerEvent::ToolCallStarted {
                call_id: format!("r{n}"),
                tool: "read".into(),
                input: json!({"path": "a"}),
                signature: None,
            });
            events.push(WorkerEvent::ToolCallFinished {
                call_id: format!("r{n}"),
                output: "same".into(),
                is_error: false,
            });
        }
        let mut resumed = ProgressGate::new(10, 10);
        resumed.resume(&events, &Facts);
        assert_eq!(resumed.rounds_granted(), 10);
        assert!(resumed.due(17));
        let (m, _) = resumed.measure(17, &BTreeMap::new(), None);
        assert_eq!(m.rounds, 10);
        assert_eq!(m.sloppy.len(), 1, "the open block's calls were replayed");
    }

    /// A first block of block size 10: the checkpoint falls at round 7 and the
    /// midpoint at round 3.
    fn first_block_gate() -> ProgressGate {
        let mut g = ProgressGate::new(10, 10);
        // `a.rs` and `b.rs` did not exist at the start of the block.
        g.capture_preimage("a.rs".into(), None);
        g.capture_preimage("b.rs".into(), None);
        g
    }

    const FAILING: &str = "exit status: 101\ntest result: FAILED. 1 passed; 39 failed;";

    #[test]
    fn the_midpoint_is_captured_once_in_the_first_block_only() {
        let mut g = ProgressGate::new(10, 10);
        assert!(!g.midpoint_due(2));
        assert!(g.midpoint_due(3));
        assert!(g.midpoint_due(6));
        assert!(!g.midpoint_due(7), "the checkpoint is due then");
        g.capture_midpoint(BTreeMap::new());
        assert!(!g.midpoint_due(4), "once");
        let (c, b) = g.decide(7, ProgressMeasure::default(), CheckpointBaseline::default());
        let _ = (c, b);
        assert!(!g.midpoint_due(12), "later blocks are measured as before");
    }

    /// Greenfield shape: files written in round 2, then churn, with the test
    /// run flat at the same failure. Counting every file that differs from the
    /// empty start called this progress and extended it.
    #[test]
    fn a_first_block_with_a_flat_failing_check_does_not_count_file_changes() {
        let mut g = first_block_gate();
        let cmd = json!({"command": ["cargo", "test"]});
        call(&mut g, "1", "write", json!({"path": "a.rs"}), "ok", false);
        g.capture_midpoint(files(&[("a.rs", Some("v1"))]));
        call(&mut g, "2", "shell", cmd.clone(), FAILING, true);
        call(
            &mut g,
            "3",
            "write",
            json!({"path": "a.rs", "n": 2}),
            "ok",
            false,
        );
        call(&mut g, "4", "shell", cmd, FAILING, true);
        let (m, _) = g.measure(7, &files(&[("a.rs", Some("v2"))]), None);
        assert!(m.files_changed.is_empty(), "{m:?}");
        assert_eq!(m.files_discounted, vec![PathBuf::from("a.rs")]);
        assert!(m.checks_improved.is_empty());
        assert!(!m.progressing());
        let (c, _) = g.decide(7, m, CheckpointBaseline::default());
        assert_eq!(c.verdict, CheckpointVerdict::EndNoProgress);
        assert!(c.render().contains("not counted"), "{}", c.render());
    }

    #[test]
    fn a_first_block_with_a_failing_check_that_improved_is_progress() {
        let mut g = first_block_gate();
        let cmd = json!({"command": ["cargo", "test"]});
        call(&mut g, "1", "write", json!({"path": "a.rs"}), "ok", false);
        call(&mut g, "2", "shell", cmd.clone(), FAILING, true);
        g.capture_midpoint(files(&[("a.rs", Some("v1"))]));
        call(
            &mut g,
            "3",
            "write",
            json!({"path": "a.rs", "n": 2}),
            "ok",
            false,
        );
        call(
            &mut g,
            "4",
            "shell",
            cmd,
            "exit status: 101\ntest result: FAILED. 20 passed; 20 failed;",
            true,
        );
        let (m, _) = g.measure(7, &files(&[("a.rs", Some("v2"))]), None);
        assert!(m.progressing(), "{m:?}");
        assert_eq!(m.files_changed, vec![PathBuf::from("a.rs")]);
        assert!(m.files_discounted.is_empty());
    }

    #[test]
    fn a_first_block_whose_check_ends_passing_keeps_its_file_changes() {
        let mut g = first_block_gate();
        let cmd = json!({"command": ["cargo", "test"]});
        g.capture_midpoint(files(&[("a.rs", Some("v1"))]));
        call(
            &mut g,
            "1",
            "write",
            json!({"path": "a.rs", "n": 2}),
            "ok",
            false,
        );
        call(
            &mut g,
            "2",
            "shell",
            cmd,
            "exit status: 0\ntest result: ok. 40 passed; 0 failed;",
            false,
        );
        let (m, _) = g.measure(7, &files(&[("a.rs", Some("v2"))]), None);
        assert!(m.progressing(), "{m:?}");
    }

    #[test]
    fn a_first_block_counts_only_files_that_changed_since_its_midpoint() {
        let mut g = first_block_gate();
        call(&mut g, "1", "write", json!({"path": "a.rs"}), "ok", false);
        g.capture_midpoint(files(&[("a.rs", Some("v1"))]));
        call(&mut g, "2", "write", json!({"path": "b.rs"}), "ok", false);
        // `a.rs` is as it was at the midpoint; `b.rs` was first written after it.
        let now = files(&[("a.rs", Some("v1")), ("b.rs", Some("w1"))]);
        let (m, _) = g.measure(7, &now, None);
        assert_eq!(m.files_changed, vec![PathBuf::from("b.rs")]);
        // Nothing written after the midpoint: no progress, and no discount note
        // (no check ran).
        let mut g = first_block_gate();
        call(&mut g, "1", "write", json!({"path": "a.rs"}), "ok", false);
        g.capture_midpoint(files(&[("a.rs", Some("v1"))]));
        let (m, _) = g.measure(7, &files(&[("a.rs", Some("v1"))]), None);
        assert!(m.files_changed.is_empty());
        assert!(m.files_discounted.is_empty());
        assert!(!m.progressing());
    }

    #[test]
    fn a_later_block_is_measured_against_the_last_checkpoint_as_before() {
        let mut g = first_block_gate();
        let cmd = json!({"command": ["cargo", "test"]});
        call(&mut g, "1", "write", json!({"path": "a.rs"}), "ok", false);
        let (m, base) = g.measure(7, &files(&[("a.rs", Some("v1"))]), None);
        assert!(m.progressing(), "no check ran: the files count");
        g.decide(7, m, base);
        call(
            &mut g,
            "3",
            "write",
            json!({"path": "a.rs", "n": 2}),
            "ok",
            false,
        );
        call(&mut g, "4", "shell", cmd, FAILING, true);
        let (m, _) = g.measure(17, &files(&[("a.rs", Some("v2"))]), None);
        assert_eq!(m.files_changed, vec![PathBuf::from("a.rs")]);
        assert!(m.files_discounted.is_empty());
    }
}
