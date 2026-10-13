//! The verifier, and selection among parallel attempts (decision 0009).
//!
//! A worker's claim of completion is not progress. Progress is a verifier's verdict
//! (0057: progress claims are checked against the verifier), and the verifier is a
//! mechanism, tests and not a model acting as judge (0009).
//!
//! Attempt lifecycle ported from enricoros/big-AGI
//! `src/modules/beam/scatter/beam.scatter.ts` (`BRay.status`, `rayScatterStart`,
//! `messageWasInterruptedAtStart`, the `playNice` throttle), commit
//! 00715049819786dc66bb6961a5795dd538e79f29, MIT (license read at the source
//! 2026-09-26; see `third_party/big-agi/UPSTREAM`):
//!
//! - an attempt is `empty`, `scattering`, `success`, `stopped` or `error`;
//! - each attempt owns its cancellation, so stopping one never touches another
//!   (here: each is its own `WorkerHandle`);
//! - an attempt aborted before any content is `empty`, **no candidate**, which
//!   differs from a candidate that failed;
//! - parallelism is a setting: `max_parallel_attempts` caps how many run at once.
//!
//! Beam's fusion half (a model merging answers) is not ported: the owner excluded
//! it, and 0009 selects by verifier.

use crate::run_state::{OpenContract, VerifiedCommit};
use kgraf_core::{AgentId, Money};
use kgraf_runtime::{WorkerOutcome, WorkerResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A verifier's decision on one attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// The contract's acceptance holds. `commit` is the worktree commit that
    /// holds the change, when the verifier made or found one.
    Pass {
        by: String,
        commit: Option<VerifiedCommit>,
        /// Facts about the project the check surfaced that the owner should act
        /// on and that are not the attempt's failure, for example a lockfile
        /// the check generated because none is committed. Reported with the
        /// run, never merged.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notices: Vec<String>,
    },
    Fail {
        reason: String,
    },
}

impl Verdict {
    pub fn passed(&self) -> bool {
        matches!(self, Verdict::Pass { .. })
    }
}

/// Decides whether an attempt met its contract.
#[async_trait::async_trait]
pub trait Verifier: Send + Sync {
    /// Verify one completed attempt in its worktree. `Err` means the verifier
    /// itself could not run, which is not a verdict on the work.
    async fn verify(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        result: &WorkerResult,
    ) -> Result<Verdict, String>;

    /// [`Verifier::verify`], told which other contracts of the plan are still
    /// open (`unmerged`, with the files each writes): their work may be
    /// missing from the attempt's tree. A verifier whose check needs that
    /// work may defer the check to the root gate for such an attempt, saying
    /// so in the verdict (decision 0018, amended 2026-10-02 and 2026-10-10).
    /// The default ignores the list.
    async fn verify_among(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        result: &WorkerResult,
        unmerged: &[OpenContract],
    ) -> Result<Verdict, String> {
        let _ = unmerged;
        self.verify(contract, workspace, result).await
    }

    /// Re-check a recorded progress claim on resume: whether the commit the
    /// record says was verified still verifies.
    async fn recheck(&self, commit: &VerifiedCommit) -> Result<bool, String>;

    /// What a fresh attempt's brief says about the check it will be judged by,
    /// when that differs from the run's acceptance command as written (for
    /// example, narrowed to the tests of the contract's files). `workspace` is
    /// the attempt's worktree as dispatched. `None` adds nothing.
    fn acceptance_brief(&self, _contract: &OpenContract, _workspace: &Path) -> Option<String> {
        None
    }

    /// What judges an attempt of `contract` whose worktree is `workspace`, as
    /// the tree is now, said to a worker that may not write another
    /// contract's file: the command the verifier runs, and what happens to a
    /// failure that comes from files of the `unmerged` contracts the tree
    /// lacks. One or more sentences, or `None` when the verifier cannot say;
    /// an answer then names no judge, never one the verifier does not apply.
    fn judge(
        &self,
        _contract: &OpenContract,
        _workspace: &Path,
        _unmerged: &[OpenContract],
    ) -> Option<String> {
        None
    }

    /// The test files (or packages) a contract that declares `writes` would
    /// be checked by in the repository at `root` (when the run has one), when
    /// the check narrows to them; `None` when the whole command runs or the
    /// verifier cannot tell. Read at plan intake: contracts that share one
    /// write set checked by one test file cannot be verified apart.
    fn plan_check_scope(&self, _writes: &[&str], _root: Option<&Path>) -> Option<Vec<String>> {
        None
    }
}

/// Beam's ray status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// Stopped before any model call produced anything: no candidate.
    Empty,
    /// Running.
    Scattering,
    /// Completed with an answer: a candidate for the verifier.
    Success,
    /// Cancelled after it had started producing.
    Stopped,
    /// Failed, stalled, ran out of steps, or handed back.
    Error,
}

/// The status a finished worker's result puts its attempt in.
pub fn status_of(result: &WorkerResult) -> AttemptStatus {
    match &result.outcome {
        WorkerOutcome::Completed => AttemptStatus::Success,
        // Upstream `messageWasInterruptedAtStart`: nothing came back.
        WorkerOutcome::Cancelled if result.steps == 0 || result.calls.is_empty() => {
            AttemptStatus::Empty
        }
        WorkerOutcome::Cancelled => AttemptStatus::Stopped,
        WorkerOutcome::HandedBack { .. }
        | WorkerOutcome::BudgetExhausted
        | WorkerOutcome::Stalled { .. }
        | WorkerOutcome::Failed { .. } => AttemptStatus::Error,
    }
}

/// Whether a worker that ended without finishing left a tree the verifier
/// should still judge: it was stopped by the stall check or ran out of rounds,
/// and it changed files.
///
/// A worker's claim of completion is not progress, and neither is its lack of
/// one: the verifier's verdict on the tree is. A worker can deliver a tree that
/// passes, then fumble a cosmetic edit until the stall check ends it; without
/// this, that tree was discarded unseen. Both endings stop at a round
/// boundary (the stall check refuses the whole round before any call runs; the
/// budget ends between rounds or refuses the final round's calls), so the
/// tree is final. `Failed` is left out: it can end inside a round whose calls
/// ran only in part, and it is mostly a provider or environment fault whose
/// retry is already handled apart. `HandedBack` usually continues in a fresh
/// worker; when it will not, [`verifies_handed_back`] covers it. `Cancelled`
/// was asked for. A tree with no change has nothing to judge.
pub fn verifies_unfinished(result: &WorkerResult) -> bool {
    matches!(
        result.outcome,
        WorkerOutcome::Stalled { .. } | WorkerOutcome::BudgetExhausted
    ) && !result.changed_paths.is_empty()
}

/// Whether a worker that handed its work back left a tree the verifier
/// should judge, given that no continuation will run for it (the caller
/// checks that: its re-split would be refused, or another attempt is the one
/// continued). A hand-back ends at a round boundary, so the tree is final,
/// and a worker can hand back a remainder after it already delivered the
/// whole contract; without this, that tree was held unjudged. A tree with no
/// change has nothing to judge.
pub fn verifies_handed_back(result: &WorkerResult) -> bool {
    matches!(result.outcome, WorkerOutcome::HandedBack { .. }) && !result.changed_paths.is_empty()
}

/// One attempt at a contract.
#[derive(Clone, Debug)]
pub struct Attempt {
    pub worker: AgentId,
    pub route: String,
    pub workspace: PathBuf,
    pub status: AttemptStatus,
    pub result: Option<WorkerResult>,
    pub verdict: Option<Verdict>,
    pub spend: Money,
    /// False when the ledger could not price the attempt: `spend` is then a
    /// bound, not a measured figure.
    pub spend_known: bool,
    /// The verifier could not run for this attempt: its verdict is the
    /// environment's, not a judgment of the work.
    pub infrastructure: bool,
}

impl Attempt {
    pub fn scattering(worker: AgentId, route: String, workspace: PathBuf) -> Self {
        Self {
            worker,
            route,
            workspace,
            status: AttemptStatus::Scattering,
            result: None,
            verdict: None,
            spend: Money::ZERO,
            spend_known: true,
            infrastructure: false,
        }
    }
}

/// The winner among finished attempts: a passing verdict, then an attempt
/// already merged into the integration branch (choosing another would merge a
/// second time), then a known spend over an unknown one (an unpriced attempt's
/// zero is not a measured cost), then the lowest spend, then the earliest
/// attempt. `None` when no attempt passed.
pub fn select(attempts: &[Attempt], premerged: &BTreeSet<AgentId>) -> Option<usize> {
    attempts
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            a.status == AttemptStatus::Success && a.verdict.as_ref().is_some_and(Verdict::passed)
        })
        .min_by_key(|(i, a)| (!premerged.contains(&a.worker), !a.spend_known, a.spend, *i))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::CallId;

    fn result(outcome: WorkerOutcome, steps: u32, calls: usize) -> WorkerResult {
        WorkerResult {
            calls: (0..calls).map(|i| CallId::new(format!("c{i}"))).collect(),
            steps,
            ..WorkerResult::bare("t", "w", outcome)
        }
    }

    #[test]
    fn a_cancel_before_any_content_is_no_candidate_not_a_failure() {
        assert_eq!(
            status_of(&result(WorkerOutcome::Cancelled, 0, 0)),
            AttemptStatus::Empty
        );
        assert_eq!(
            status_of(&result(WorkerOutcome::Cancelled, 2, 2)),
            AttemptStatus::Stopped
        );
        assert_eq!(
            status_of(&result(WorkerOutcome::Completed, 2, 2)),
            AttemptStatus::Success
        );
        assert_eq!(
            status_of(&result(WorkerOutcome::Failed { error: "x".into() }, 1, 1)),
            AttemptStatus::Error
        );
    }

    #[test]
    fn only_a_stalled_or_spent_worker_that_changed_files_is_verified_unfinished() {
        let stalled = || WorkerOutcome::Stalled {
            call_digest: "d".into(),
            repeats: 3,
            call: String::new(),
        };
        let changed = |outcome| WorkerResult {
            changed_paths: vec![PathBuf::from("src/lib.rs")],
            ..result(outcome, 5, 5)
        };
        assert!(verifies_unfinished(&changed(stalled())));
        assert!(verifies_unfinished(&changed(
            WorkerOutcome::BudgetExhausted
        )));
        // Nothing changed: nothing to judge.
        assert!(!verifies_unfinished(&result(stalled(), 5, 5)));
        assert!(!verifies_unfinished(&result(
            WorkerOutcome::BudgetExhausted,
            5,
            5
        )));
        // A failure can end inside a round; a cancel was asked for.
        assert!(!verifies_unfinished(&changed(WorkerOutcome::Failed {
            error: "x".into()
        })));
        assert!(!verifies_unfinished(&changed(WorkerOutcome::Cancelled)));
        // Completed goes to the verifier by its own path.
        assert!(!verifies_unfinished(&changed(WorkerOutcome::Completed)));
    }

    #[test]
    fn selection_takes_the_cheapest_passing_candidate() {
        let mut a = Attempt::scattering("w1".into(), "r1".into(), PathBuf::from("/a"));
        let mut b = Attempt::scattering("w2".into(), "r2".into(), PathBuf::from("/b"));
        let mut c = Attempt::scattering("w3".into(), "r1".into(), PathBuf::from("/c"));
        for (x, spend, pass) in [
            (&mut a, 300, true),
            (&mut b, 100, false),
            (&mut c, 200, true),
        ] {
            x.status = AttemptStatus::Success;
            x.spend = Money(spend);
            x.verdict = Some(if pass {
                Verdict::Pass {
                    by: "tests".into(),
                    commit: None,
                    notices: Vec::new(),
                }
            } else {
                Verdict::Fail {
                    reason: "red".into(),
                }
            });
        }
        assert_eq!(
            select(&[a.clone(), b.clone(), c.clone()], &BTreeSet::new()),
            Some(2)
        );
        c.status = AttemptStatus::Stopped;
        assert_eq!(select(&[a, b, c], &BTreeSet::new()), Some(0));
        assert_eq!(select(&[], &BTreeSet::new()), None);
    }

    fn passing(worker: &str, spend: i64, known: bool) -> Attempt {
        let mut a = Attempt::scattering(worker.into(), "r".into(), PathBuf::from("/w"));
        a.status = AttemptStatus::Success;
        a.spend = Money(spend);
        a.spend_known = known;
        a.verdict = Some(Verdict::Pass {
            by: "tests".into(),
            commit: None,
            notices: Vec::new(),
        });
        a
    }

    #[test]
    fn an_unknown_spend_never_beats_a_known_one() {
        let attempts = [passing("w1", 0, false), passing("w2", 500, true)];
        assert_eq!(select(&attempts, &BTreeSet::new()), Some(1));
    }

    #[test]
    fn a_premerged_attempt_wins_over_a_cheaper_one() {
        let attempts = [passing("w1", 900, true), passing("w2", 100, true)];
        let premerged: BTreeSet<AgentId> = ["w1".into()].into();
        assert_eq!(select(&attempts, &premerged), Some(0));
    }
}
