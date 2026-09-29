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

    /// Re-check a recorded progress claim on resume: whether the commit the
    /// record says was verified still verifies.
    async fn recheck(&self, commit: &VerifiedCommit) -> Result<bool, String>;
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
        }
    }
}

/// The winner among finished attempts: a passing verdict, then the lowest spend,
/// then the earliest attempt. `None` when no attempt passed.
pub fn select(attempts: &[Attempt]) -> Option<usize> {
    attempts
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            a.status == AttemptStatus::Success && a.verdict.as_ref().is_some_and(Verdict::passed)
        })
        .min_by_key(|(i, a)| (a.spend, *i))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::CallId;

    fn result(outcome: WorkerOutcome, steps: u32, calls: usize) -> WorkerResult {
        WorkerResult {
            task: "t".into(),
            worker: "w".into(),
            outcome,
            answer: String::new(),
            changed_paths: vec![],
            commit: None,
            calls: (0..calls).map(|i| CallId::new(format!("c{i}"))).collect(),
            steps,
            prefix_digest: String::new(),
            denied: 0,
            trace_failures: Vec::new(),
            rationale: None,
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
        assert_eq!(select(&[a.clone(), b.clone(), c.clone()]), Some(2));
        c.status = AttemptStatus::Stopped;
        assert_eq!(select(&[a, b, c]), Some(0));
        assert_eq!(select(&[]), None);
    }
}
