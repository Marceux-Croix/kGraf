//! A handoff child continues in its parent's worktree (decision 0067). That
//! worktree holds the parent's uncommitted work, and it is shared: the child,
//! the child's retry and any later child of the same handoff all run in it. A
//! provider that deletes a worktree once it is released (decision 0088's
//! default) must never be asked to delete it while an open contract still
//! names it.

mod support;

use kgraf_core::{AgentId, RunId};
use kgraf_orchestrator::{
    DirWorkspaces, Integration, OpenContract, Orchestrator, PrepareFailure, RunStatus, Verdict,
    VerifiedCommit, Verifier, Workspaces,
};
use kgraf_runtime::WorkerResult;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

/// The parent's own work, written into every fresh worktree: a file the
/// handoff child never writes, so only an intact worktree still has it.
const PARENT_WORK: &str = "parent-work.txt";

/// Plain directories, removed on release as `WorktreeRetention::Delete` does
/// (a released worktree that is not kept, and a merged one, go with their
/// builds). Records every removal.
struct DeletingWorkspaces {
    dirs: DirWorkspaces,
    removed: Mutex<Vec<PathBuf>>,
    merges: AtomicUsize,
}

impl DeletingWorkspaces {
    fn remove(&self, workspace: &Path) {
        if workspace.exists() {
            std::fs::remove_dir_all(workspace).expect("remove");
        }
        self.removed
            .lock()
            .expect("lock")
            .push(workspace.to_path_buf());
    }
}

#[async_trait::async_trait]
impl Workspaces for DeletingWorkspaces {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &AgentId,
    ) -> Result<PathBuf, PrepareFailure> {
        let path = self.dirs.prepare(contract, worker).await?;
        std::fs::write(path.join(PARENT_WORK), "done before the handoff").expect("write");
        Ok(path)
    }

    async fn release(&self, workspace: &Path, keep: bool) {
        if !keep {
            self.remove(workspace);
        }
    }

    async fn integrate(
        &self,
        _contract: &OpenContract,
        workspace: &Path,
    ) -> Result<Integration, String> {
        let n = self.merges.fetch_add(1, Ordering::SeqCst) + 1;
        self.remove(workspace);
        Ok(Integration::Merged {
            commit: format!("{n:040x}"),
        })
    }
}

/// Fails the handoff child's first verification (its work is refused), then
/// passes only a worktree that still holds the parent's work.
struct ParentWorkVerifier {
    child_verifications: AtomicUsize,
    seen: Mutex<Vec<(String, PathBuf, bool)>>,
}

#[async_trait::async_trait]
impl Verifier for ParentWorkVerifier {
    async fn verify(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        _result: &WorkerResult,
    ) -> Result<Verdict, String> {
        let intact = workspace.join(PARENT_WORK).is_file();
        self.seen.lock().expect("lock").push((
            contract.contract.clone(),
            workspace.to_path_buf(),
            intact,
        ));
        if contract.contract.contains(".h")
            && self.child_verifications.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Ok(Verdict::Fail {
                reason: "the first attempt at the rest is refused".into(),
            });
        }
        if !intact {
            return Ok(Verdict::Fail {
                reason: format!("{PARENT_WORK} is gone from {}", workspace.display()),
            });
        }
        Ok(Verdict::Pass {
            by: "fake-tests".into(),
            commit: Some(VerifiedCommit {
                sha: "a".repeat(40),
                verified_by: "fake-tests".into(),
                verified_ms: 0,
            }),
            notices: Vec::new(),
        })
    }

    async fn recheck(&self, _commit: &VerifiedCommit) -> Result<bool, String> {
        Ok(true)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_handoff_child_never_deletes_the_worktree_its_retry_continues_in() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = serde_json::json!({"contracts":[
        {"id":"big","instruction":"BIG part WRITE src/big.rs","acceptance":"src/big.rs exists",
         "writes":[{"path":"src/big.rs"}],"tools":["write_file"]},
        {"id":"after","instruction":"after WRITE src/after.rs","acceptance":"ok",
         "writes":[{"path":"src/after.rs"}],"depends_on":["big"],"tools":["write_file"]}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let workspaces = Arc::new(DeletingWorkspaces {
        dirs: DirWorkspaces {
            base: dir.path().join("worktrees"),
        },
        removed: Mutex::new(Vec::new()),
        merges: AtomicUsize::new(0),
    });
    let verifier = Arc::new(ParentWorkVerifier {
        child_verifications: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let mut cfg = config();
    cfg.max_split_depth = 0;
    let mut services = p.services(10_000);
    services.workspaces = workspaces.clone();
    services.verifier = verifier.clone();
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "a big part, then another".into())
        .await
        .expect("run");

    let seen = verifier.seen.lock().expect("lock").clone();
    let child: Vec<&(String, PathBuf, bool)> =
        seen.iter().filter(|(c, _, _)| c == "big.h1").collect();
    assert!(child.len() >= 2, "the child was retried: {seen:?}");
    let inherited = &child[0].1;
    assert!(
        child
            .iter()
            .all(|(_, ws, intact)| ws == inherited && *intact),
        "every attempt at the child ran in the parent's intact worktree: {seen:?}"
    );
    let removed = workspaces.removed.lock().expect("lock").clone();
    let first_removal = removed.iter().position(|r| r == inherited);
    assert_eq!(
        first_removal,
        removed.iter().rposition(|r| r == inherited),
        "the inherited worktree is removed at most once: {removed:?}"
    );
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        report.verified,
        vec!["big.h1".to_owned(), "after".to_owned()]
    );
}
