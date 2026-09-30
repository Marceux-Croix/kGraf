//! A live defect: a run reported `Completed`, and a
//! contract reported verified, without the run ever confirming the merge its
//! own record claimed had actually landed on `kgraf/<run>` (a redirected
//! worker's change went missing from the merged result even though the run
//! reported `Completed`). `finalize`/`integrate_winner`/`announce_verified`
//! already order a merge before a contract counts as verified; this test
//! targets the one place nothing checked the *run's own* claim against real
//! git before reporting it: the run's end.
//!
//! With real git ([`GitIntegration`]): a merge lands on the repository's
//! branch, exactly as `integrate_winner` records it, and then the branch is
//! moved back off that commit -- standing in for a force-reset, a second
//! process, or a bug upstream of this check, the class of fault
//! `Orchestrator::verify_completion_landed`
//! ([`kgraf_orchestrator::Workspaces::commit_landed`]) exists to catch. A
//! `Workspaces` provider that never implements `commit_landed` (the trait's
//! default, `Ok(true)`) cannot be told apart from one whose merge is real, so
//! this run's [`Workspaces`] does the real ancestry check.

mod contract_support;
mod support;

use async_trait::async_trait;
use contract_support::{commit_all, git};
use kgraf_core::{AgentId, RunId};
use kgraf_orchestrator::{
    Integration, OpenContract, Orchestrator, PrepareFailure, RunStatus, Workspaces,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// [`GitIntegration`], except every merge it accepts is immediately walked
/// back off the branch by hand -- the fault this test reproduces -- and
/// [`Workspaces::commit_landed`] answers from git itself, never from the
/// merge that already happened.
struct LossyIntegration {
    inner: Arc<GitIntegration>,
    repo: PathBuf,
}

impl LossyIntegration {
    fn new(inner: Arc<GitIntegration>, repo: PathBuf) -> Arc<Self> {
        Arc::new(Self { inner, repo })
    }
}

#[async_trait]
impl Workspaces for LossyIntegration {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &AgentId,
    ) -> Result<PathBuf, PrepareFailure> {
        self.inner.prepare(contract, worker).await
    }

    async fn release(&self, workspace: &Path, keep: bool) {
        self.inner.release(workspace, keep).await
    }

    async fn integrate(
        &self,
        contract: &OpenContract,
        workspace: &Path,
    ) -> Result<Integration, String> {
        let outcome = self.inner.integrate(contract, workspace).await?;
        if let Integration::Merged { commit } = &outcome {
            // The merge is real and accepted (exactly what `integrate_winner`
            // sees); something else now moves the branch off it, unbeknownst
            // to the orchestrator's own record.
            let parent = git(&self.repo, &["rev-parse", &format!("{commit}^1")]);
            git(
                &self.repo,
                &["update-ref", "refs/heads/main", &parent, commit],
            );
        }
        Ok(outcome)
    }

    async fn integration_tip(&self) -> Result<Option<String>, String> {
        self.inner.integration_tip().await
    }

    async fn commit_landed(&self, commit: &str) -> Result<bool, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["merge-base", "--is-ancestor", commit, "HEAD"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .map_err(|e| e.to_string())?;
        Ok(out.success())
    }
}

/// A run whose one merge lands, is accepted, and is then moved off the
/// branch: `Orchestrator::verify_completion_landed` must never let this
/// report `Completed`, and must name the contract and the commit that did
/// not hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_never_reports_completed_over_a_merge_the_branch_no_longer_holds() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    std::fs::write(repo.join("README.md"), "seed\n").expect("write");
    commit_all(&repo);

    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(10),
    );
    let inner = GitIntegration::new(dir.path().join("worktrees"), repo.clone());
    let integration = LossyIntegration::new(inner, repo.clone());
    let mut services = p.services(WINDOW);
    services.workspaces = integration;
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "build a".into())
        .await
        .expect("run");

    assert_eq!(
        report.verified,
        vec!["a".to_owned()],
        "the contract still verified locally; the branch losing the merge is a \
         fact only the run's end can catch, not `finalize` (it already ran) \
         {report:?}"
    );
    let RunStatus::Blocked { reason } = report.status else {
        panic!(
            "the merge the branch no longer holds must never be reported \
             Completed: {:?}",
            report.status
        );
    };
    assert!(
        reason.contains('a')
            && reason
                .to_lowercase()
                .contains("not on the integration branch"),
        "the reason must name what did not land: {reason}"
    );
}
