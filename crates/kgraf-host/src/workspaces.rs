//! Git worktrees for a run: one per worker, the integration branch, startup
//! cleanup, and `kgraf clean`. Moved from `kgraf-cli`'s
//! `orchestrate_cmd.rs` without a change in behavior.

use crate::error::{Error, Result, ResultExt};
use kgraf_graph::GraphStore;
use kgraf_orchestrator::{
    ChildMerge, Integration, OpenContract, PrepareFailure, RunStore, Workspaces,
};
use kgraf_sandbox::{
    Attempt, FinishOutcome, MergeBack, MergeOutcome, ReopenSpec, Workspace, WorkspaceManager,
    WorkspaceSpec,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where the project's own state lives. One directory, gitignored: every call
/// also runs [`ensure_local_exclude`], so a plain `git add -A` in the user's
/// repository never picks up the ledger, allow-rules, graph, schedule, run
/// records, transcripts or worktrees this holds.
///
/// A project that still has its state under the former name (`.haustoria`,
/// decision 0062) has it moved first ([`crate::migrate::migrate_state_dir`]);
/// the one notice that prints says what moved and what was repaired.
pub fn state_dir(root: &Path) -> Result<PathBuf> {
    if let Some(report) = crate::migrate::migrate_state_dir(root)? {
        eprintln!("{}", report.notice());
    }
    let dir = root.join(kgraf_core::legacy::STATE_DIR);
    std::fs::create_dir_all(&dir).with_step(|| format!("cannot create {}", dir.display()))?;
    ensure_local_exclude(root)?;
    Ok(dir)
}

/// The name of kGraf's state directory, relative to the repository root,
/// as it is written to the exclude file (with the trailing slash so it only
/// ever matches the directory, never a same-named file).
const STATE_DIR_EXCLUDE_LINE: &str = "/.kgraf/";

/// Every line [`ensure_local_exclude`] makes sure of: the state directory,
/// the same under its former name (a project not yet migrated, or one where
/// someone recreates it), and the migration lock beside them (decision 0062).
const STATE_EXCLUDE_LINES: [&str; 3] =
    [STATE_DIR_EXCLUDE_LINE, "/.haustoria/", "/.kgraf.migrating"];

/// Add [`STATE_DIR_EXCLUDE_LINE`] to this git repository's *local* exclude
/// file, never to the project's tracked `.gitignore`: `$GIT_DIR/info/exclude`,
/// resolved by asking git itself (`git rev-parse --git-path info/exclude`)
/// rather than assuming `<root>/.git/info/exclude`, because that assumption
/// breaks for a linked worktree (whose `info/exclude` lives in the *common*
/// git directory, shared with the checkout it was made from) and for a
/// `GIT_DIR` override (`Git::run` already strips `GIT_DIR` and friends from
/// the child's environment before invoking git, so the resolution below sees
/// the real repository either way).
///
/// Called from the single place ([`state_dir`]) every entry point already
/// goes through to create or open `.kgraf` (the CLI commands, the host,
/// the shell, and the scheduler all call it, directly or through
/// `Host::open`), so there is exactly one call site to keep correct.
///
/// Idempotent: the file is read first, and the line is appended only when it
/// is not already present as its own line, so running this any number of
/// times leaves exactly one `/.kgraf/` line. The append preserves
/// whatever the exclude file already held (a user may have local ignores of
/// their own in it) and adds a leading newline only when the existing content
/// does not already end in one.
///
/// Deliberately does nothing, without error, when `root` is not a git working
/// tree the git CLI can resolve (no `git` on `PATH`, or `root` is outside any
/// repository): `state_dir` is also reached from contexts such as `kgraf
/// worker run` against a scratch directory that is never a git repository, and
/// there both `git add -A` and this defect are moot.
///
/// A repository that already tracks files under `.kgraf` is not fixed
/// here: the exclude line only stops a *future* `git add` from re-staging
/// them, and this helper never touches the index. A loud warning names the
/// risk and the untracking command instead.
///
/// Fails loud only when the exclude file itself cannot be written
/// (for example a read-only `$GIT_DIR`): every other step above is a
/// best-effort skip, but silently losing a write here would let the very
/// defect this exists to close recur without anyone knowing.
pub(crate) fn ensure_local_exclude(root: &Path) -> Result<()> {
    let git = match kgraf_sandbox::git::Git::discover() {
        Ok(git) => git,
        // No `git` on PATH: nothing this helper does can apply, and every
        // other git-dependent step in kGraf will fail its own way first.
        Err(_) => return Ok(()),
    };
    let Ok(raw) = git.run_str(root, ["rev-parse", "--git-path", "info/exclude"]) else {
        // `root` is not inside a git working tree: no exclude file to add to.
        return Ok(());
    };
    let exclude_path = PathBuf::from(&raw);
    let exclude_path = if exclude_path.is_absolute() {
        exclude_path
    } else {
        root.join(exclude_path)
    };
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)
            .with_step(|| format!("cannot create {}", parent.display()))?;
    }
    // Only a missing file reads as empty: an unreadable one must fail, or the
    // write below would replace the user's own local ignore rules.
    let existing = match std::fs::read_to_string(&exclude_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_step(|| format!("cannot read {}", exclude_path.display()));
        }
    };
    let missing: Vec<&str> = STATE_EXCLUDE_LINES
        .iter()
        .copied()
        .filter(|wanted| !existing.lines().any(|line| line.trim() == *wanted))
        .collect();
    if !missing.is_empty() {
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        for line in missing {
            text.push_str(line);
            text.push('\n');
        }
        std::fs::write(&exclude_path, text)
            .with_step(|| format!("cannot write {}", exclude_path.display()))?;
    }
    // A repository that already tracks `.kgraf` would still push the
    // ledger and allow-rules on the next commit that touches them; the
    // exclude line above cannot undo that, so name the risk loudly instead of
    // changing the index ourselves.
    if let Some(warning) = tracked_state_warning(&git, root) {
        eprintln!("{warning}");
    }
    Ok(())
}

/// `Some(message)` naming the risk when `root` already tracks files under
/// `.kgraf` in git (checked with `git ls-files`, which reads the index,
/// never changes it). Split out from [`ensure_local_exclude`] so the warning
/// text is one thing to test without capturing process stderr.
pub(crate) fn tracked_state_warning(git: &kgraf_sandbox::git::Git, root: &Path) -> Option<String> {
    let tracked = git.run_str(root, ["ls-files", "--", ".kgraf"]).ok()?;
    if tracked.trim().is_empty() {
        // Tracked under the former name (decision 0062): moving the directory
        // left those files deleted in the working tree, and a commit of
        // everything would delete them from history's next commit while the
        // ledger it held stays in the earlier ones.
        let legacy = kgraf_core::legacy::LEGACY_STATE_DIR;
        let old = git.run_str(root, ["ls-files", "--", legacy]).ok()?;
        if old.trim().is_empty() {
            return None;
        }
        return Some(format!(
            "warning: {} tracks files under {legacy} in git (the state directory under \
             kGraf's former name, now moved to .kgraf). Their earlier commits still hold \
             the ledger, allow rules or transcripts. Untrack the old path: `git rm -r \
             --cached {legacy}` (then commit the removal).",
            root.display()
        ));
    }
    Some(format!(
        "warning: {} already tracks files under .kgraf in git (its ledger, allow rules, \
         run records or transcripts); the local exclude line just ensured only stops a \
         *future* `git add`, so what is already tracked would still be committed and could be \
         pushed. Untrack it without deleting it: `git rm -r --cached .kgraf` (then commit \
         the removal).",
        root.display()
    ))
}

pub(crate) fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// `kgraf clean <worktree>`: remove a worktree on the owner's word. A
/// worktree a run record still names is the work a resume would continue:
/// refused without `force`, saying which run. A live owner or a process still
/// running in it is refused by the sandbox (`WorkspaceError::Busy`), with its
/// reason.
pub fn clean(root: &Path, name: &str, force: bool) -> Result<PathBuf> {
    let state = state_dir(root)?;
    clean_in(root, &state.join("worktrees"), name, force)
}

pub(crate) fn clean_in(root: &Path, base: &Path, name: &str, force: bool) -> Result<PathBuf> {
    let state = state_dir(root)?;
    let graph = Arc::new(Mutex::new(
        GraphStore::open(&state.join("graph.db"), root).step("cannot open the graph")?,
    ));
    let store = RunStore::new(graph);
    if !force {
        let runs: Vec<String> = store
            .all()
            .step("cannot read the run records")?
            .into_iter()
            .filter(|r| {
                r.open_contracts.iter().any(|c| {
                    c.workers.iter().any(|w| w.as_str() == name)
                        || c.worktrees.contains_key(name)
                        || c.workspace
                            .as_deref()
                            .and_then(|p| p.file_name())
                            .is_some_and(|n| n == name)
                })
            })
            .map(|r| r.run.to_string())
            .collect();
        if !runs.is_empty() {
            return Err(Error::Invalid(format!(
                "worktree {name} is still named by run(s) {} for a resume, which would continue \
                 its worker in it; resume the run instead, or give --force to remove it (the \
                 resume then starts that worker over)",
                runs.join(", ")
            )));
        }
    }
    let manager = WorkspaceManager::open(root, base).step("cannot open the worktree manager")?;
    manager
        .clean(name)
        .with_step(|| format!("worktree {name} was not removed"))
}

/// Startup cleanup (0056): what dead processes left under the worktree base is
/// removed (conversation and probe worktrees, worktrees a record dropped),
/// except a worktree some run record still names, which a resume reopens.
/// Worktrees of live processes (another run in progress) are never touched.
pub(crate) fn clean_stale_worktrees(
    manager: &WorkspaceManager,
    store: &RunStore,
) -> Result<Vec<kgraf_sandbox::OrphanRemoval>> {
    let referenced = store
        .referenced_worktrees()
        .step("cannot read the run records")?;
    manager
        .startup_keeping(&|name| referenced.contains(name))
        .step("cannot clean the worktree base")
}

/// The run's integration branch, `kgraf/<run>`, created at the checkout's
/// HEAD the first time and reused on resume.
///
/// **Greenfield (decision 0013).** A genuinely empty directory has a
/// git repository with no commit yet (`git init` alone), so `HEAD` is unborn
/// and there is nothing for `git branch ... HEAD` to point at: a plan approved
/// against an empty corpus could be read but never dispatched. One empty
/// commit, under the repository's own configured identity (the same one
/// merge-back already requires; the product's rule is that commits carry the
/// user's own identity), gives git a first object to branch from. It adds no code and
/// changes nothing else about the checkout, since there is nothing else there
/// yet; the plan's own work items are what write code, in their own worktrees,
/// after this.
pub(crate) fn ensure_integration_branch(root: &Path, run: &str) -> Result<String> {
    let safe: String = run
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let branch = format!("kgraf/{safe}");
    let git = kgraf_sandbox::git::Git::discover().step("git is needed for merge-back")?;
    let exists = |name: &str| {
        let full = format!("refs/heads/{name}");
        git.run(root, ["rev-parse", "--verify", "--quiet", full.as_str()])
            .is_ok()
    };
    if git
        .run(root, ["rev-parse", "--verify", "--quiet", "HEAD"])
        .is_err()
    {
        let identity = crate::worker::workspace::owner_identity(root).with_settings(|| {
            "the repository has no commit yet (a greenfield directory): its first commit \
             needs the owner's identity"
                .to_owned()
        })?;
        git.run(
            root,
            [
                "-c",
                &format!("user.name={}", identity.name),
                "-c",
                &format!("user.email={}", identity.email),
                // Never signs, never prompts: the user's own global
                // `commit.gpgsign` must not turn this internal bootstrap
                // commit into a hang waiting on a passphrase (fail
                // loud rather than silently hang).
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "kGraf: first commit (a greenfield directory had none)",
            ],
        )
        .step("cannot make the greenfield directory's first commit")?;
    }
    // A run started before the rename (decision 0062) has its integration
    // branch under the former prefix. Resuming it continues that branch; a
    // fresh one at HEAD would silently drop the work already merged there.
    // The user's branch is never renamed: it may have been pushed.
    let legacy = format!("haustoria/{safe}");
    if !exists(&branch) && exists(&legacy) {
        return Ok(legacy);
    }
    if !exists(&branch) {
        git.run(root, ["branch", branch.as_str(), "HEAD"])
            .with_step(|| format!("cannot create the integration branch {branch}"))?;
    }
    Ok(branch)
}

/// One git worktree per worker, created by `kgraf-sandbox` at the run's
/// current integration tip (or at a remediation's recorded base). The live
/// [`Workspace`] objects are kept: the verifier needs each one to commit it
/// through merge-back and to run the acceptance command in its sandbox, and the
/// committed attempt is kept until the orchestrator merges the selected one or
/// releases the rest.
///
/// A worktree left on disk by an earlier process (or kept for a worker that
/// will be continued) is reopened by the gitlink and base recorded in the run
/// record ([`Workspaces::handle`], `WorkspaceManager::reopen`), with the
/// worker's changes, when it is intact. One that is not intact is refused, never
/// replaced: the attempt fails loudly and the contract is retried with a fresh
/// worker.
///
/// Conversation files are written in a fresh worktree of the integration tip
/// ([`Workspaces::open_comms`]) and merged back like an attempt, never in the
/// user's checkout.
pub(crate) struct GitWorkspaces {
    pub(crate) manager: Arc<WorkspaceManager>,
    pub(crate) merge_back: Arc<MergeBack>,
    live: Mutex<BTreeMap<PathBuf, Workspace>>,
    /// Attempts the verifier committed, by worktree root.
    committed: Mutex<BTreeMap<PathBuf, Attempt>>,
    /// Worktrees whose attempt changed nothing (the verifier removed them
    /// after their check passed).
    pub(crate) empty: Mutex<BTreeSet<PathBuf>>,
    /// Every worker worktree name this process prepared or reopened, for the
    /// attempt-ref sweep at the end of the run.
    touched: Mutex<BTreeSet<String>>,
    /// Attempts a conflict left kept (0073), by their original worktree root:
    /// not discarded at conflict time so a resolver's brief can cite the
    /// commit and the attempt's own fresh worktree can read it. Settled by
    /// `discard_conflict` once a resolver merges the fix or the contract
    /// gives up; an unsettled entry is still swept by `sweep_refs` like any
    /// other attempt ref at the end of the run.
    conflicted: Mutex<BTreeMap<PathBuf, Attempt>>,
}

impl GitWorkspaces {
    pub(crate) fn new(manager: Arc<WorkspaceManager>, merge_back: Arc<MergeBack>) -> Self {
        Self {
            manager,
            merge_back,
            live: Mutex::new(BTreeMap::new()),
            committed: Mutex::new(BTreeMap::new()),
            empty: Mutex::new(BTreeSet::new()),
            touched: Mutex::new(BTreeSet::new()),
            conflicted: Mutex::new(BTreeMap::new()),
        }
    }

    /// At the end of a run: delete the attempt refs of every worktree this
    /// process worked in that no open contract still uses.
    pub(crate) fn sweep_refs(
        &self,
        record: Option<&kgraf_orchestrator::RunStateRecord>,
    ) -> Result<Vec<String>> {
        let still: BTreeSet<String> = record
            .map(|r| {
                r.open_contracts
                    .iter()
                    .flat_map(|c| c.workers.iter().map(|w| w.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let names: Vec<String> = self
            .touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|n| !still.contains(*n))
            .cloned()
            .collect();
        let mut swept = self.merge_back.sweep_attempt_refs(&names).lift()?;
        // Subagents' refs (decision 0067), once no open contract of this run
        // names the child any more. The spawn record of every live or
        // unmerged child sits in its contract's `worktrees` map.
        let keep: BTreeSet<String> = record
            .map(|r| {
                r.open_contracts
                    .iter()
                    .flat_map(|c| c.worktrees.keys().cloned())
                    .collect()
            })
            .unwrap_or_default();
        swept.extend(
            child::sweep(self.manager.repo(), &keep)
                .map_err(|e| Error::Invalid(format!("subagent refs were not swept: {e}")))?,
        );
        Ok(swept)
    }

    /// Take the live worktree named `name` out of this process's map.
    fn take_live(&self, name: &str) -> Option<Workspace> {
        let mut live = self.live();
        let root = live
            .iter()
            .find(|(_, w)| w.name() == name)
            .map(|(r, _)| r.clone())?;
        live.remove(&root)
    }

    fn live(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Workspace>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn committed(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Attempt>> {
        self.committed.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn get(&self, root: &Path) -> Option<Workspace> {
        self.live().get(root).cloned()
    }
}

#[async_trait::async_trait]
impl Workspaces for GitWorkspaces {
    async fn prepare(
        &self,
        _contract: &OpenContract,
        worker: &kgraf_core::AgentId,
    ) -> std::result::Result<PathBuf, PrepareFailure> {
        if let Some(ws) = self.live().values().find(|w| w.name() == worker.as_str()) {
            return Ok(ws.root().to_path_buf());
        }
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(worker.to_string());
        let path = self.manager.base().join(worker.as_str());
        if path.exists() {
            let recorded = _contract
                .worktrees
                .get(worker.as_str())
                .map(|h| serde_json::from_str::<WorktreeHandle>(h));
            let handle = match recorded {
                Some(Ok(h)) => h,
                Some(Err(e)) => {
                    return Err(format!("the recorded worktree is unreadable: {e}").into());
                }
                None => {
                    return Err(format!(
                        "worktree {} was left with no recorded gitlink and cannot be \
                         reopened; the attempt starts over with a fresh worker",
                        path.display()
                    )
                    .into());
                }
            };
            let manager = self.manager.clone();
            let name = worker.to_string();
            let branch = format!("refs/heads/{}", self.merge_back.integration_branch());
            let ws = tokio::task::spawn_blocking(move || {
                manager.reopen(&ReopenSpec {
                    name: &name,
                    gitlink: &handle.gitlink,
                    base: &handle.base,
                    integration: Some(&branch),
                })
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| match e {
                kgraf_sandbox::WorkspaceError::AlreadyMerged { commit, .. } => {
                    PrepareFailure::AlreadyMerged { commit }
                }
                busy @ kgraf_sandbox::WorkspaceError::Busy { .. } => PrepareFailure::Busy(format!(
                    "{busy}; the same worker is retried after a backoff, so its work \
                         is kept"
                )),
                other => PrepareFailure::Unavailable(format!(
                    "{other}; the contract is retried after a backoff with a fresh worker \
                     in a fresh worktree"
                )),
            })?;
            let root = ws.root().to_path_buf();
            self.live().insert(root.clone(), ws);
            return Ok(root);
        }
        let manager = self.manager.clone();
        let merge_back = self.merge_back.clone();
        let name = worker.to_string();
        let pinned = _contract.base.clone();
        let ws = tokio::task::spawn_blocking(move || {
            // The current integration tip, read now: it holds every attempt
            // merged so far. A remediation starts from its gate's failed commit.
            let base = match pinned {
                Some(b) => b,
                None => merge_back.integration_tip().map_err(|e| e.to_string())?,
            };
            manager
                .create(&WorkspaceSpec {
                    name: &name,
                    base: &base,
                    branch: None,
                })
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(PrepareFailure::from)?;
        let root = ws.root().to_path_buf();
        self.live().insert(root.clone(), ws);
        Ok(root)
    }

    async fn handle(&self, workspace: &Path) -> Option<String> {
        let ws = self.get(workspace)?;
        serde_json::to_string(&WorktreeHandle {
            gitlink: ws.gitlink().to_owned(),
            base: ws.head().to_owned(),
        })
        .ok()
    }

    /// The winner was merged (and retired) by `integrate`; a kept worktree
    /// stays on disk for its worker to be continued in (reopened by
    /// `prepare`). A committed loser is discarded with its worktree and attempt
    /// ref. A worktree whose acceptance command failed was committed (the
    /// snapshot comes before the check) but never registered here, so it is
    /// left for inspection; its attempt ref goes in the end-of-run sweep.
    async fn release(&self, workspace: &Path, keep: bool) {
        self.live().remove(workspace);
        self.empty
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(workspace);
        if keep {
            return;
        }
        let loser = self.committed().remove(workspace);
        if let Some(mut attempt) = loser {
            let merge_back = self.merge_back.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = merge_back.discard(&mut attempt) {
                    eprintln!("merge-back could not discard {}: {e}", attempt.name());
                }
            })
            .await;
        }
    }

    async fn integrate(
        &self,
        _contract: &OpenContract,
        workspace: &Path,
    ) -> std::result::Result<Integration, String> {
        if self
            .empty
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(workspace)
        {
            return Ok(Integration::Empty);
        }
        let taken = self.committed().remove(workspace);
        let Some(mut attempt) = taken else {
            return Err(format!(
                "{} has no committed attempt to merge",
                workspace.display()
            ));
        };
        let merge_back = self.merge_back.clone();
        // `Ok` carries the attempt back out on a conflict (0073): kept, not
        // discarded, so a resolver's fresh worktree can read its commit; the
        // caller below stores it under `workspace` for `discard_conflict` to
        // settle once the resolver (or the plain fallback) is done with it.
        let result = tokio::task::spawn_blocking(move || match merge_back.merge(&mut attempt) {
            Ok(MergeOutcome::Merged { merge_commit, .. }) => (
                Ok(Integration::Merged {
                    commit: merge_commit,
                }),
                None,
            ),
            Ok(MergeOutcome::Conflicted(c)) => {
                // Never auto-resolved by kGraf itself (0018's original rule):
                // a resolver worker, or nobody, decides what the conflicted
                // paths become. The attempt is kept, not discarded, until
                // that is settled (0073).
                let commit = Some(c.attempt_commit.clone());
                let outcome = Integration::Conflict {
                    tip: c.integration_tip.clone(),
                    paths: c
                        .paths
                        .iter()
                        .map(|p| p.path.clone())
                        .chain(c.collisions.iter().flatten().cloned())
                        .collect(),
                    detail: c
                        .messages
                        .iter()
                        .map(|m| m.kind.clone())
                        .collect::<Vec<_>>()
                        .join("; "),
                    commit,
                };
                (Ok(outcome), Some(attempt))
            }
            Err(e) => {
                let _ = merge_back.discard(&mut attempt);
                (Err(format!("merge-back: {e}")), None)
            }
        })
        .await
        .map_err(|e| e.to_string())?;
        let (outcome, kept) = result;
        if let Some(attempt) = kept {
            self.conflicted
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(workspace.to_path_buf(), attempt);
        }
        outcome
    }

    /// The conflicted attempt kept at `workspace` (0073) is settled: a
    /// resolver merged its fix, or the contract gave up and falls back to a
    /// plain re-dispatch. Either way it is superseded, never merged itself:
    /// discard its ref and worktree once, whichever happened. A `workspace`
    /// with nothing kept (already settled, or never conflicted) is a no-op.
    async fn discard_conflict(&self, workspace: &Path) {
        let kept = self
            .conflicted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(workspace);
        let Some(mut attempt) = kept else {
            return;
        };
        let merge_back = self.merge_back.clone();
        let _ = tokio::task::spawn_blocking(move || merge_back.discard(&mut attempt)).await;
    }

    /// A fresh worktree of the integration tip for the bus to write in.
    async fn open_comms(&self) -> std::result::Result<Option<PathBuf>, String> {
        let manager = self.manager.clone();
        let merge_back = self.merge_back.clone();
        let ws = tokio::task::spawn_blocking(move || {
            let tip = merge_back.integration_tip().map_err(|e| e.to_string())?;
            let name = format!("comms-{}", now_nanos());
            manager
                .create(&WorkspaceSpec {
                    name: &name,
                    base: &tip,
                    branch: None,
                })
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        let root = ws.root().to_path_buf();
        self.live().insert(root.clone(), ws);
        Ok(Some(root))
    }

    async fn discard_comms(&self, root: &Path) {
        let taken = self.live().remove(root);
        if let Some(ws) = taken {
            let manager = self.manager.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = manager.remove(ws) {
                    eprintln!("could not remove a conversation worktree: {e}");
                }
            })
            .await;
        }
    }

    /// The bus's files, written in the worktree `open_comms` gave it, merged
    /// back like an attempt.
    async fn commit_comms(
        &self,
        root: &Path,
        files: &[String],
        message: &str,
    ) -> std::result::Result<Integration, String> {
        let Some(ws) = self.live().remove(root) else {
            return Err(format!(
                "{} is not a conversation worktree of this run",
                root.display()
            ));
        };
        let merge_back = self.merge_back.clone();
        let message = message.to_owned();
        // The bus declared exactly these files: anything else in the
        // conversation worktree is refused, never merged (decision 0083).
        let files = files.to_vec();
        // Whatever the outcome (merged, empty, conflict, refusal, an error, even
        // `attempt` failing), the conversation worktree does not outlive this call.
        let guard = RemoveOnDrop {
            manager: self.manager.clone(),
            ws: Some(ws.clone()),
        };
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let mut attempt = merge_back.attempt(ws).map_err(|e| e.to_string())?;
            match merge_back.finish(&mut attempt, &message, &files) {
                Ok(FinishOutcome::Merged { merge_commit, .. }) => Ok(Integration::Merged {
                    commit: merge_commit,
                }),
                Ok(FinishOutcome::Empty) => Ok(Integration::Empty),
                Ok(FinishOutcome::Conflicted(c)) => {
                    // No resolver for a conversation file (0073 is for a
                    // worker's attempt): discarded immediately, as before.
                    let _ = merge_back.discard(&mut attempt);
                    Ok(Integration::Conflict {
                        tip: c.integration_tip.clone(),
                        paths: c.paths.iter().map(|p| p.path.clone()).collect(),
                        detail: "conversation file conflict".into(),
                        commit: None,
                    })
                }
                Ok(FinishOutcome::Refused(r)) => {
                    let _ = merge_back.discard(&mut attempt);
                    Ok(Integration::Refused {
                        detail: format!("{:?}", r.reasons),
                    })
                }
                Err(e) => {
                    let _ = merge_back.discard(&mut attempt);
                    Err(e.to_string())
                }
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn integration_tip(&self) -> std::result::Result<Option<String>, String> {
        let merge_back = self.merge_back.clone();
        tokio::task::spawn_blocking(move || merge_back.integration_tip().map(Some))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    async fn commit_landed(&self, commit: &str) -> std::result::Result<bool, String> {
        let merge_back = self.merge_back.clone();
        let commit = commit.to_owned();
        tokio::task::spawn_blocking(move || merge_back.commit_is_on_branch(&commit))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    /// An in-flight worktree as it stands (decision 0074): the subagent
    /// checkpoint's private-index snapshot ([`child::snapshot`], reused), so
    /// the worker's files, index and HEAD are untouched and no ref is made.
    /// A worktree this process does not hold is refused, never guessed at.
    async fn snapshot(&self, workspace: &Path) -> std::result::Result<Option<String>, String> {
        let ws = self
            .get(workspace)
            .or_else(|| {
                self.committed()
                    .get(workspace)
                    .map(|a| a.workspace().clone())
            })
            .ok_or_else(|| format!("{} is not a worktree of this run", workspace.display()))?;
        let repo = self.manager.repo().to_path_buf();
        tokio::task::spawn_blocking(move || {
            child::snapshot(
                &repo,
                &ws,
                "kgraf: speculative snapshot (never on a branch)",
            )
        })
        .await
        .map_err(|e| e.to_string())?
        .map(Some)
    }

    /// `git gc --auto` on the object store the speculative round wrote to
    /// (`MergeBack::gc_auto`): nothing happens below git's own thresholds.
    async fn collect_garbage(&self) -> std::result::Result<(), String> {
        let merge_back = self.merge_back.clone();
        tokio::task::spawn_blocking(move || merge_back.gc_auto())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("git gc --auto: {e}"))
    }

    /// Merge-back's own merge computation, onto the tip, moving nothing
    /// (`MergeBack::trial_merge`, decision 0074).
    async fn trial_merge(
        &self,
        commits: &[String],
    ) -> std::result::Result<Option<kgraf_orchestrator::Trial>, String> {
        let merge_back = self.merge_back.clone();
        let commits = commits.to_vec();
        let trial = tokio::task::spawn_blocking(move || merge_back.trial_merge(&commits))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("speculative merge: {e}"))?;
        Ok(Some(match trial {
            kgraf_sandbox::TrialMerge::Clean { commit, tree, .. } => {
                kgraf_orchestrator::Trial::Clean { commit, tree }
            }
            kgraf_sandbox::TrialMerge::Conflicted { paths, kinds, .. } => {
                kgraf_orchestrator::Trial::Conflict {
                    paths,
                    detail: kinds.join("; "),
                }
            }
        }))
    }

    /// A writer subagent's fresh worktree (decision 0067, point 5): see
    /// [`child::prepare_fresh`]. Whatever an earlier child of the same id
    /// left in this process is discarded first.
    async fn prepare_child(
        &self,
        parent: &Path,
        child: &kgraf_core::AgentId,
    ) -> std::result::Result<PathBuf, PrepareFailure> {
        let stale = self.take_live(child.as_str());
        let parent_ws = self.get(parent);
        let manager = self.manager.clone();
        let name = child.to_string();
        let ws = tokio::task::spawn_blocking(move || {
            child::discard(&manager, stale, &name).map_err(PrepareFailure::Unavailable)?;
            child::prepare_fresh(&manager, parent_ws.as_ref(), &name)
        })
        .await
        .map_err(|e| PrepareFailure::Unavailable(e.to_string()))??;
        let root = ws.root().to_path_buf();
        self.live().insert(root.clone(), ws);
        Ok(root)
    }

    /// A child an earlier process started, reopened with its work by its
    /// recorded handle: see [`child::reopen`].
    async fn reopen_child(
        &self,
        _parent: &Path,
        child: &kgraf_core::AgentId,
    ) -> std::result::Result<PathBuf, PrepareFailure> {
        if let Some(ws) = self.live().values().find(|w| w.name() == child.as_str()) {
            return Ok(ws.root().to_path_buf());
        }
        let manager = self.manager.clone();
        let name = child.to_string();
        let ws = tokio::task::spawn_blocking(move || child::reopen(&manager, &name))
            .await
            .map_err(|e| PrepareFailure::Unavailable(e.to_string()))??;
        let root = ws.root().to_path_buf();
        self.live().insert(root.clone(), ws);
        Ok(root)
    }

    /// Remove a child's worktree and refs: a spawn that never started, or
    /// one whose record could not be written.
    async fn discard_child(&self, child: &kgraf_core::AgentId) -> std::result::Result<(), String> {
        let stale = self.take_live(child.as_str());
        let manager = self.manager.clone();
        let name = child.to_string();
        tokio::task::spawn_blocking(move || child::discard(&manager, stale, &name))
            .await
            .map_err(|e| e.to_string())?
    }

    /// Merge a finished writer subagent's declared writes back into its
    /// parent's worktree (decision 0067, point 5): see [`child::merge`].
    async fn merge_child(
        &self,
        parent: &Path,
        child: &Path,
        writes: &[String],
    ) -> std::result::Result<ChildMerge, String> {
        let Some(parent_ws) = self.get(parent) else {
            return Err(format!(
                "{} is not a live worktree of this run",
                parent.display()
            ));
        };
        let child_ws = self.get(child);
        let name = match &child_ws {
            Some(ws) => ws.name().to_owned(),
            None => child
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| format!("{} names no worktree", child.display()))?,
        };
        let manager = self.manager.clone();
        let writes = writes.to_vec();
        let merged = tokio::task::spawn_blocking(move || {
            child::merge(&manager, &parent_ws, child_ws.as_ref(), &name, &writes)
        })
        .await
        .map_err(|e| e.to_string())??;
        if matches!(merged, ChildMerge::Merged { .. } | ChildMerge::Empty { .. }) {
            // Merged: the child's worktree is spent (`child::merge` removed
            // it). Anything else keeps both worktrees as they are.
            self.live().remove(child);
        }
        Ok(merged)
    }
}

/// Writer subagents' worktrees (decision 0067, point 5).
///
/// **Nothing moves the parent's HEAD.** A worker's worktree is committed by
/// merge-back only once, against the base it was created at
/// (`MergeBack::commit` refuses a moved HEAD, and `WorkspaceManager::reopen`
/// refuses one too). So the checkpoint a child branches from is a commit
/// object built from a private index (`GIT_INDEX_FILE`) over the parent's
/// working tree, parented on its HEAD and kept alive by a ref; the parent's
/// HEAD, index and files are untouched. The child's result is built the
/// same way from the child's tree, parented on the checkpoint.
///
/// **Merging back is a path-wise three-way apply of the declared writes.**
/// The child may write only inside its declared paths, and the parent's
/// writes to those paths are fenced while the child lives, so the two sides
/// are expected to be disjoint. For every declared path the child changed
/// (checkpoint to result), the parent's current file, hashed the way git
/// would store it (`hash-object --path`, so clean filters such as
/// `eol=crlf`, `core.autocrlf` and LFS apply) with its mode, must still be
/// the checkpoint's entry (or already the child's, after a crash mid-apply);
/// then the child's version is written through the smudge filters
/// (`cat-file --filters`). Any other parent version is a conflict: typed,
/// returned, never resolved, and nothing is written, so both worktrees stay
/// as they are (a resolver worker handles it; this only reports). A
/// hunk-level merge would hide exactly the fence breach this check exists to
/// surface. A changed path the child did not declare is never applied and
/// is reported; a declared path the snapshot left out (a `.gitignore` match)
/// refuses the merge and is reported, rather than reading as "empty".
///
/// **Durable and idempotent.** Three refs per child under
/// `refs/kgraf/subagents/<child>/`: `checkpoint`, `handle` (a blob with the
/// gitlink and base, so a resumed process reopens the worktree by what was
/// recorded, never by the worker-writable `.git` file) and `result`. A fresh
/// `prepare` first discards whatever an earlier child of the same id left;
/// `reopen` only ever reopens. A second `merge` after a crash re-applies to
/// the same end state, and refuses a `result` that is not a child of the
/// current `checkpoint` (a stale ref of an earlier child of the same id).
/// The refs live in the shared git directory, which sandboxed workers cannot
/// write, and are swept once no open contract names the child.
///
/// Written new: searched this repository (`kgraf-sandbox` merge-back
/// commits and merges whole attempts into a branch, and moves HEAD) and
/// git's own plumbing (`git stash create` has no untracked files; `git
/// merge-tree --merge-base` needs git 2.40 and resolves hunks, which is the
/// silent resolution this must not do). Nothing covers a HEAD-preserving
/// snapshot plus a path-disjoint apply, so it is plumbing calls here.
pub(crate) mod child {
    use super::{PrepareFailure, WorktreeHandle, now_nanos};
    use kgraf_orchestrator::ChildMerge;
    use kgraf_sandbox::{ReopenSpec, Workspace, WorkspaceManager, WorkspaceSpec};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// The ref names of one child.
    const REFS: [&str; 3] = ["checkpoint", "handle", "result"];

    /// Where a child's refs live.
    pub(crate) const REF_ROOT: &str = "refs/kgraf/subagents/";

    pub(crate) fn ref_of(child: &str, what: &str) -> String {
        format!("{REF_ROOT}{child}/{what}")
    }

    /// Variables that would redirect git away from the directories named on
    /// the command line (the same list `kgraf_sandbox::git` strips).
    const REDIRECT: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_PREFIX",
    ];

    /// A worktree a git command reads: its trusted admin dir, its root, and
    /// the tree git reads attributes from. Never the worker's own
    /// `.gitattributes`: a worker-written one could select a filter driver
    /// the user configured (git-lfs, git-crypt, any `filter.<name>`), which
    /// git would run on the host, unsandboxed, while staging or hashing. The
    /// same defense as merge-back's (`kgraf_sandbox::merge`, module doc).
    #[derive(Clone, Copy)]
    pub(crate) struct Wt<'a> {
        pub(crate) admin: &'a Path,
        pub(crate) work: &'a Path,
        /// A commit or tree kGraf made or trusts: the worktree's recorded
        /// base (`Workspace::head`, set at creation, not worker-writable).
        pub(crate) attr: &'a str,
    }

    /// One git command, hooks and fsmonitor off. `tree` is set for a command
    /// that reads a worktree (with `--attr-source` pinned to its trusted
    /// tree); without it, the command runs in the shared repository. `index`
    /// replaces the worktree's own index.
    fn git(
        repo: &Path,
        tree: Option<Wt<'_>>,
        index: Option<&Path>,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        let mut cmd = Command::new("git");
        for var in REDIRECT {
            cmd.env_remove(var);
        }
        // A worktree command runs in its work tree, so pathspecs and
        // `add -A` resolve against it rather than the shared repository.
        cmd.current_dir(tree.map_or(repo, |t| t.work))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "kgraf")
            .env("GIT_AUTHOR_EMAIL", "kgraf@localhost.invalid")
            .env("GIT_COMMITTER_NAME", "kgraf")
            .env("GIT_COMMITTER_EMAIL", "kgraf@localhost.invalid")
            .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor="]);
        if let Some(t) = tree {
            let mut git_dir = OsString::from("--git-dir=");
            git_dir.push(t.admin);
            let mut work_tree = OsString::from("--work-tree=");
            work_tree.push(t.work);
            cmd.arg(git_dir)
                .arg(work_tree)
                .arg(format!("--attr-source={}", t.attr));
        }
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("git {args:?}: {e}"))?;
        if let Some(bytes) = stdin {
            use std::io::Write as _;
            if let Some(mut pipe) = child.stdin.take() {
                pipe.write_all(bytes)
                    .map_err(|e| format!("git {args:?}: {e}"))?;
            }
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("git {args:?}: {e}"))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn git_str(
        repo: &Path,
        tree: Option<Wt<'_>>,
        index: Option<&Path>,
        args: &[&str],
    ) -> Result<String, String> {
        let out = git(repo, tree, index, args, None)?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    /// The worktree's admin directory, taken from the gitlink recorded when it
    /// was created (never from the worker-writable `.git` file), after
    /// checking that file still says the same.
    fn admin_of(ws: &Workspace) -> Result<PathBuf, String> {
        ws.verify_gitlink().map_err(|e| e.to_string())?;
        let target = ws
            .gitlink()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .ok_or_else(|| format!("{}: the recorded gitlink names no admin dir", ws.name()))?;
        let target = Path::new(target);
        Ok(if target.is_absolute() {
            target.to_path_buf()
        } else {
            ws.root().join(target)
        })
    }

    /// A commit of `ws`'s working tree as it stands, parented on its HEAD,
    /// built from a private index: HEAD, the worktree's index and every file
    /// are left as they are. Ignored files are left out, as `add -A` does.
    ///
    /// Attributes come from the worktree's recorded base, never from what
    /// the worker wrote ([`Wt`]), and a snapshot whose changes touch a path
    /// that steers git (`.gitattributes`, `.gitmodules`, `.git*`
    /// directories) or kGraf's state is refused, loudly, as merge-back
    /// refuses the same attempt. Used for a subagent's checkpoint
    /// and result (0067) and for speculative merge (0074).
    pub(crate) fn snapshot(repo: &Path, ws: &Workspace, message: &str) -> Result<String, String> {
        let admin = admin_of(ws)?;
        let tree = Some(Wt {
            admin: &admin,
            work: ws.root(),
            attr: ws.head(),
        });
        // In the admin dir: outside every sandbox's writable roots, so a
        // worker cannot swap the index while it is read.
        let index = admin.join(format!("kgraf-snapshot-{}.index", now_nanos()));
        let own = admin.join("index");
        let result = (|| {
            if own.exists() {
                // A copy keeps git's stat cache, so unchanged files are not
                // hashed again.
                std::fs::copy(&own, &index).map_err(|e| format!("{}: {e}", own.display()))?;
            } else {
                git(repo, tree, Some(&index), &["read-tree", "HEAD"], None)?;
            }
            git(repo, tree, Some(&index), &["add", "-A"], None)?;
            let head = git_str(
                repo,
                tree,
                None,
                &["rev-parse", "--verify", "HEAD^{commit}"],
            )?;
            let changed = git(
                repo,
                tree,
                Some(&index),
                &[
                    "diff-index",
                    "--cached",
                    "--name-only",
                    "-z",
                    "--no-renames",
                    &head,
                ],
                None,
            )?;
            let protected: Vec<String> = changed
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .filter(|p| p.split('/').any(kgraf_core::paths::is_protected_name))
                .collect();
            if !protected.is_empty() {
                return Err(format!(
                    "refused: {} changes {}, which steer git or hold kGraf's state; \
                     nothing was snapshotted",
                    ws.name(),
                    protected.join(", ")
                ));
            }
            let written = git_str(repo, tree, Some(&index), &["write-tree"])?;
            git_str(
                repo,
                tree,
                Some(&index),
                &[
                    "commit-tree",
                    "--no-gpg-sign",
                    &written,
                    "-p",
                    &head,
                    "-m",
                    message,
                ],
            )
        })();
        let _ = std::fs::remove_file(&index);
        result
    }

    /// The commit a ref names, if the ref exists.
    fn resolve(repo: &Path, name: &str) -> Option<String> {
        let spec = format!("{name}^{{commit}}");
        git_str(
            repo,
            None,
            None,
            &["rev-parse", "--verify", "--quiet", &spec],
        )
        .ok()
    }

    fn delete_refs(repo: &Path, child: &str) {
        for what in REFS {
            let _ = git(
                repo,
                None,
                None,
                &["update-ref", "-d", &ref_of(child, what)],
                None,
            );
        }
    }

    /// Remove what a child of this id left: its worktree (`live`, when this
    /// process holds it; else whatever is registered on disk) and its refs.
    pub(crate) fn discard(
        manager: &WorkspaceManager,
        live: Option<Workspace>,
        child: &str,
    ) -> Result<(), String> {
        match live {
            Some(ws) => manager.remove(ws).map_err(|e| e.to_string())?,
            None => {
                if manager.base().join(child).exists() {
                    manager.clean(child).map_err(|e| {
                        format!(
                            "the earlier worktree of subagent `{child}` could not be removed: {e}"
                        )
                    })?;
                }
            }
        }
        delete_refs(manager.repo(), child);
        Ok(())
    }

    /// A fresh worktree for `child`, branched from a checkpoint of `parent`'s
    /// working tree. Stale refs of an earlier child of this id are replaced
    /// (the caller discarded its worktree).
    pub(crate) fn prepare_fresh(
        manager: &WorkspaceManager,
        parent: Option<&Workspace>,
        child: &str,
    ) -> Result<Workspace, PrepareFailure> {
        let repo = manager.repo();
        let Some(parent) = parent else {
            return Err(PrepareFailure::Unavailable(format!(
                "the parent of subagent `{child}` has no live worktree in this process"
            )));
        };
        delete_refs(repo, child);
        let checkpoint = snapshot(
            repo,
            parent,
            &format!(
                "kgraf: checkpoint of {} for subagent {child}",
                parent.name()
            ),
        )
        .map_err(PrepareFailure::Unavailable)?;
        git(
            repo,
            None,
            None,
            &["update-ref", &ref_of(child, "checkpoint"), &checkpoint],
            None,
        )
        .map_err(PrepareFailure::Unavailable)?;
        let ws = manager
            .create(&WorkspaceSpec {
                name: child,
                base: &checkpoint,
                branch: None,
            })
            .map_err(|e| match e {
                busy @ kgraf_sandbox::WorkspaceError::Busy { .. } => {
                    PrepareFailure::Busy(busy.to_string())
                }
                other => PrepareFailure::Unavailable(other.to_string()),
            })?;
        let handle = serde_json::to_vec(&WorktreeHandle {
            gitlink: ws.gitlink().to_owned(),
            base: ws.head().to_owned(),
        })
        .map_err(|e| PrepareFailure::Unavailable(e.to_string()))?;
        let blob = git(
            repo,
            None,
            None,
            &["hash-object", "-w", "--stdin"],
            Some(&handle),
        )
        .map_err(PrepareFailure::Unavailable)?;
        let blob = String::from_utf8_lossy(&blob).trim().to_owned();
        git(
            repo,
            None,
            None,
            &["update-ref", &ref_of(child, "handle"), &blob],
            None,
        )
        .map_err(PrepareFailure::Unavailable)?;
        Ok(ws)
    }

    /// The worktree an earlier process created for `child`, reopened by its
    /// recorded handle, with the child's work in it. Never branched again.
    pub(crate) fn reopen(
        manager: &WorkspaceManager,
        child: &str,
    ) -> Result<Workspace, PrepareFailure> {
        let repo = manager.repo();
        let handle_ref = ref_of(child, "handle");
        let text =
            git(repo, None, None, &["cat-file", "blob", &handle_ref], None).map_err(|_| {
                PrepareFailure::Unavailable(format!("subagent `{child}` has no recorded worktree"))
            })?;
        let handle: WorktreeHandle = serde_json::from_slice(&text)
            .map_err(|e| PrepareFailure::Unavailable(format!("{handle_ref} is unreadable: {e}")))?;
        manager
            .reopen(&ReopenSpec {
                name: child,
                gitlink: &handle.gitlink,
                base: &handle.base,
                integration: None,
            })
            .map_err(|e| match e {
                busy @ kgraf_sandbox::WorkspaceError::Busy { .. } => {
                    PrepareFailure::Busy(busy.to_string())
                }
                other => PrepareFailure::Unavailable(format!(
                    "the subagent's worktree could not be reopened: {other}"
                )),
            })
    }

    /// One tree entry: `(mode, blob id)`.
    type Entry = (String, String);

    const ABSENT: &str = "000000";

    /// One path the child changed, before and after (`None`: absent).
    struct Change {
        before: Option<Entry>,
        after: Option<Entry>,
        path: String,
    }

    /// `diff-tree -r -z --raw --no-renames` output.
    fn changes(repo: &Path, from: &str, to: &str) -> Result<Vec<Change>, String> {
        let raw = git(
            repo,
            None,
            None,
            &["diff-tree", "-r", "-z", "--raw", "--no-renames", from, to],
            None,
        )?;
        let entry =
            |mode: &str, id: &str| (mode != ABSENT).then(|| (mode.to_owned(), id.to_owned()));
        let mut out = Vec::new();
        let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
        while let Some(meta) = fields.next() {
            let meta = String::from_utf8_lossy(meta);
            let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            let path = fields
                .next()
                .ok_or_else(|| format!("diff-tree: no path after `{meta}`"))?;
            let [old_mode, new_mode, old_blob, new_blob, _status] = parts.as_slice() else {
                return Err(format!("diff-tree: unexpected record `{meta}`"));
            };
            out.push(Change {
                before: entry(old_mode, old_blob),
                after: entry(new_mode, new_blob),
                path: String::from_utf8_lossy(path).into_owned(),
            });
        }
        Ok(out)
    }

    /// The entry `rel` has in commit `rev`, if any.
    fn tree_entry(repo: &Path, rev: &str, rel: &str) -> Result<Option<Entry>, String> {
        let out = git(repo, None, None, &["ls-tree", "-z", rev, "--", rel], None)?;
        let text = String::from_utf8_lossy(&out);
        let Some(line) = text.split('\0').find(|l| !l.is_empty()) else {
            return Ok(None);
        };
        let (meta, _) = line.split_once('\t').unwrap_or((line, ""));
        let parts: Vec<&str> = meta.split(' ').collect();
        match parts.as_slice() {
            [mode, "blob", id] => Ok(Some(((*mode).to_owned(), (*id).to_owned()))),
            [mode, kind, id] => Ok(Some(((*mode).to_owned(), format!("{kind}:{id}")))),
            _ => Err(format!("ls-tree: unexpected record `{line}`")),
        }
    }

    /// `rel` in `ws`'s working tree as git would store it: its mode and the
    /// blob id after the clean filters `ws`'s attributes select. A symbolic
    /// link is hashed as its target, never followed.
    fn disk_entry(repo: &Path, ws: &Workspace, rel: &str) -> Result<Option<Entry>, String> {
        // A folder on the way that is a link (a worker swapped `src` for
        // `src -> $HOME`) is never looked through (decision 0083): the path
        // gets an entry no commit can hold, so it matches neither side and
        // the merge is a conflict before anything is written.
        let mut at = ws.root().to_path_buf();
        let parts: Vec<&str> = rel.split('/').collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            at.push(part);
            match std::fs::symlink_metadata(&at) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Ok(Some((
                        "<link on the path>".to_owned(),
                        at.display().to_string(),
                    )));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let path = ws.root().join(rel);
        let meta = match std::fs::symlink_metadata(&path) {
            // A path under a file (`x/y` where `x` is a file) is absent too.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
            Ok(m) => m,
        };
        if meta.file_type().is_symlink() {
            let target =
                std::fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let id = git_str_in(
                repo,
                None,
                &["hash-object", "--no-filters", "--stdin"],
                target.to_string_lossy().as_bytes(),
            )?;
            return Ok(Some(("120000".to_owned(), id)));
        }
        if meta.is_dir() {
            return Ok(Some(("040000".to_owned(), "<directory>".to_owned())));
        }
        let admin = admin_of(ws)?;
        let tree = Some(Wt {
            admin: &admin,
            work: ws.root(),
            attr: ws.head(),
        });
        let arg = format!("--path={rel}");
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let id = git_str_in(repo, tree, &["hash-object", &arg, "--stdin"], &bytes)?;
        Ok(Some((mode_of(&meta).to_owned(), id)))
    }

    fn git_str_in(
        repo: &Path,
        tree: Option<Wt<'_>>,
        args: &[&str],
        stdin: &[u8],
    ) -> Result<String, String> {
        let out = git(repo, tree, None, args, Some(stdin))?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    #[cfg(unix)]
    fn mode_of(meta: &std::fs::Metadata) -> &'static str {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o111 != 0 {
            "100755"
        } else {
            "100644"
        }
    }

    #[cfg(not(unix))]
    fn mode_of(_meta: &std::fs::Metadata) -> &'static str {
        "100644"
    }

    /// Merge the child's declared writes into the parent's worktree.
    /// `child_ws` is the child's live worktree; `None` after a crash that
    /// followed an earlier merge's snapshot, when the recorded result is
    /// applied again.
    pub(crate) fn merge(
        manager: &WorkspaceManager,
        parent: &Workspace,
        child_ws: Option<&Workspace>,
        child: &str,
        writes: &[String],
    ) -> Result<ChildMerge, String> {
        let repo = manager.repo();
        let checkpoint = resolve(repo, &ref_of(child, "checkpoint"))
            .ok_or_else(|| format!("subagent `{child}` has no recorded checkpoint"))?;
        let result_ref = ref_of(child, "result");
        let result = match (resolve(repo, &result_ref), child_ws) {
            (Some(recorded), _) => recorded,
            (None, Some(ws)) => {
                let result = snapshot(repo, ws, &format!("kgraf: result of subagent {child}"))?;
                git(
                    repo,
                    None,
                    None,
                    &["update-ref", &result_ref, &result],
                    None,
                )?;
                result
            }
            (None, None) => {
                return Err(format!(
                    "subagent `{child}` has no live worktree and no recorded result"
                ));
            }
        };
        // A result left by an earlier child of this id is not this child's
        // work: applying it would revert the parent to that child's world.
        let first_parent = git_str(
            repo,
            None,
            None,
            &["rev-parse", "--verify", "--quiet", &format!("{result}^1")],
        )
        .unwrap_or_default();
        if first_parent != checkpoint {
            return Err(format!(
                "the recorded result {result} of subagent `{child}` does not descend from its \
                 checkpoint {checkpoint}; refused rather than applied (a stale ref?)"
            ));
        }
        let declared = |path: &str| {
            writes
                .iter()
                .any(|w| kgraf_core::paths::same_path(Path::new(w), Path::new(path)))
        };
        // Every declared path the child wrote must be in the result: a
        // `.gitignore` match or a filter would drop it silently otherwise.
        if let Some(ws) = child_ws {
            let mut dropped = Vec::new();
            for w in writes {
                let on_disk = disk_entry(repo, ws, w)?;
                let in_result = tree_entry(repo, &result, w)?;
                if on_disk != in_result {
                    dropped.push(w.clone());
                }
            }
            if !dropped.is_empty() {
                return Ok(ChildMerge::Dropped {
                    paths: dropped,
                    detail: "the child's work on these declared paths is not in its result \
                             (a .gitignore match or a filter left them out); nothing was merged \
                             and both worktrees are as they were"
                        .into(),
                });
            }
        }
        let changed = changes(repo, &checkpoint, &result)?;
        let (mine, not_applied): (Vec<Change>, Vec<Change>) =
            changed.into_iter().partition(|c| declared(&c.path));
        let not_applied: Vec<String> = not_applied.into_iter().map(|c| c.path).collect();
        let mut conflicted = Vec::new();
        // Two of the child's paths the file system resolves as one (`Foo.rs`
        // and `foo.rs` on APFS): applying both would let one overwrite the
        // other, so both are a conflict (decision 0083).
        let mut folded: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for c in mine.iter().filter(|c| c.after.is_some()) {
            folded
                .entry(kgraf_core::paths::fold_path(Path::new(&c.path)))
                .or_default()
                .push(&c.path);
        }
        let colliding: BTreeSet<&str> = folded
            .into_values()
            .filter(|p| p.len() > 1)
            .flatten()
            .collect();
        let mut plan = Vec::new();
        for c in &mine {
            if colliding.contains(c.path.as_str()) {
                conflicted.push(c.path.clone());
                continue;
            }
            if let Some((mode, _)) = &c.after
                && (mode == "160000" || mode == "120000")
            {
                // A nested repository or a symbolic link written by a worker
                // is never applied into another worker's tree silently.
                conflicted.push(c.path.clone());
                continue;
            }
            let now = disk_entry(repo, parent, &c.path)?;
            if now == c.after {
                continue; // already applied (a crash mid-apply)
            }
            if now != c.before {
                conflicted.push(c.path.clone());
                continue;
            }
            plan.push(c);
        }
        if !conflicted.is_empty() {
            return Ok(ChildMerge::Conflict {
                tip: result,
                paths: conflicted,
                detail: format!(
                    "the parent's worktree changed the subagent's paths after the checkpoint \
                     {checkpoint}; nothing was merged and both worktrees are as they were"
                ),
            });
        }
        let admin = admin_of(parent)?;
        let tree = Some(Wt {
            admin: &admin,
            work: parent.root(),
            attr: parent.head(),
        });
        let mut applied = Vec::new();
        for c in plan {
            let rel = Path::new(&c.path);
            let step = match &c.after {
                Some((mode, id)) => {
                    let arg = format!("--path={}", c.path);
                    // Smudged for the parent's worktree, as a checkout would.
                    // Written and chmodded through descriptors, never by path
                    // (decision 0083): the parent's worker runs meanwhile and
                    // can swap a folder for a link between any two calls.
                    git(repo, tree, None, &["cat-file", "--filters", &arg, id], None).and_then(
                        |bytes| {
                            kgraf_sandbox::write_confined_mode(
                                parent.root(),
                                rel,
                                &bytes,
                                Some(mode == "100755"),
                            )
                            .map_err(|e| format!("{}: {e}", c.path))
                        },
                    )
                }
                // The file was there as planned a moment ago: gone now, or a
                // folder on its path no longer a real folder, is not a removal
                // and is said so rather than counted as applied.
                None => match kgraf_sandbox::remove_confined(parent.root(), rel) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(format!(
                        "{}: not removed; it, or a folder on its path, changed since the \
                         merge was planned",
                        c.path
                    )),
                    Err(e) => Err(format!("{}: {e}", c.path)),
                },
            };
            if let Err(detail) = step {
                return Ok(ChildMerge::Failed {
                    detail: format!(
                        "applying {} failed: {detail}; the paths in `applied` are already in \
                         the parent's worktree, the rest are not",
                        c.path
                    ),
                    applied,
                });
            }
            applied.push(c.path.clone());
        }
        if let Some(ws) = child_ws {
            manager
                .remove(ws.clone())
                .map_err(|e| format!("subagent `{child}` merged; its worktree: {e}"))?;
        }
        // The handle goes with the worktree: nothing may reopen what is gone.
        // `checkpoint` and `result` stay, so the merge can be re-applied (and
        // audited) from the refs alone, until the contract closes.
        let _ = git(
            repo,
            None,
            None,
            &["update-ref", "-d", &ref_of(child, "handle")],
            None,
        );
        Ok(if mine.is_empty() {
            ChildMerge::Empty { not_applied }
        } else {
            ChildMerge::Merged {
                commit: result,
                not_applied,
            }
        })
    }

    /// Delete the refs of every child not in `keep`. Returns the refs deleted.
    pub(crate) fn sweep(
        repo: &Path,
        keep: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<String>, String> {
        let listed = git_str(
            repo,
            None,
            None,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                REF_ROOT,
            ],
        )?;
        let mut deleted = Vec::new();
        for line in listed.lines() {
            let Some((refname, oid)) = line.split_once(' ') else {
                continue;
            };
            let Some(child) = refname
                .strip_prefix(REF_ROOT)
                .and_then(|t| t.rsplit_once('/'))
                .map(|(c, _)| c)
            else {
                continue;
            };
            if keep.contains(child) {
                continue;
            }
            git(repo, None, None, &["update-ref", "-d", refname, oid], None)?;
            deleted.push(refname.to_owned());
        }
        Ok(deleted)
    }
}

/// Removes a worktree when dropped, unless merge-back already has.
struct RemoveOnDrop {
    manager: Arc<WorkspaceManager>,
    ws: Option<Workspace>,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(ws) = self.ws.take()
            && ws.root().exists()
            && let Err(e) = self.manager.remove(ws)
        {
            eprintln!("could not remove a conversation worktree: {e}");
        }
    }
}

/// What the run record keeps per worker so a later process can reopen its
/// worktree: the gitlink and base recorded when it was created.
#[derive(serde::Serialize, serde::Deserialize)]
struct WorktreeHandle {
    gitlink: String,
    base: String,
}

#[cfg(test)]
#[path = "subagent_e2e.rs"]
pub(crate) mod subagent_e2e;

#[cfg(test)]
#[path = "speculation_e2e.rs"]
mod speculation_e2e;

#[cfg(test)]
mod child_tests {
    //! Decision 0067, point 5: a writer subagent's worktree is branched from a
    //! checkpoint of its parent's working tree, and its declared writes are
    //! merged back path by path. Real git in temp repositories: an adapter is
    //! tested against the thing it speaks to, not a restatement of it.
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        base: PathBuf,
        ws: Arc<GitWorkspaces>,
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn workspaces(root: &Path, base: &Path) -> Arc<GitWorkspaces> {
        let manager = Arc::new(WorkspaceManager::open(root, base).expect("manager"));
        let merge_back = Arc::new(
            MergeBack::open(
                manager.clone(),
                "kgraf/r-1",
                kgraf_sandbox::Identity {
                    name: "Test Owner".into(),
                    email: "owner@example.invalid".into(),
                },
            )
            .expect("merge-back"),
        );
        Arc::new(GitWorkspaces::new(manager, merge_back))
    }

    /// A repository with `files` committed on `main` and the run's branch.
    fn fixture_with(files: &[(&str, &[u8])]) -> Fixture {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("repo");
        let base = dir.path().join("ws");
        std::fs::create_dir_all(&root).expect("mkdir");
        run(&root, &["init", "-q", "-b", "main"]);
        for (path, bytes) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, bytes).expect("write");
        }
        run(&root, &["add", "."]);
        run(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        run(&root, &["branch", "kgraf/r-1", "HEAD"]);
        let ws = workspaces(&root, &base);
        Fixture {
            _dir: dir,
            root,
            base,
            ws,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(&[("README", b"hello\n"), ("docs/old.md", b"old\n")])
    }

    /// A parent worktree as `prepare` makes one, live in `ws`.
    fn parent(f: &Fixture, ws: &GitWorkspaces, name: &str) -> PathBuf {
        let tip = run(&f.root, &["rev-parse", "kgraf/r-1"]);
        let made = ws
            .manager
            .create(&WorkspaceSpec {
                name,
                base: &tip,
                branch: None,
            })
            .expect("create");
        let root = made.root().to_path_buf();
        ws.live().insert(root.clone(), made);
        root
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| format!("<{e}>"))
    }

    fn id(s: &str) -> kgraf_core::AgentId {
        kgraf_core::AgentId::new(s)
    }

    fn w(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    fn refs_under(root: &Path, prefix: &str) -> Vec<String> {
        run(root, &["for-each-ref", "--format=%(refname)", prefix])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn a_child_starts_from_the_parents_uncommitted_work_and_merges_back_without_moving_its_head()
     {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-a-r0-a0");
        std::fs::create_dir_all(p.join("src")).expect("mkdir");
        std::fs::write(p.join("src/a.rs"), "parent a\n").expect("write");
        std::fs::write(p.join("README"), "edited by the parent\n").expect("write");
        let head = run(&p, &["rev-parse", "HEAD"]);
        let status = run(&p, &["status", "--porcelain"]);

        let c =
            f.ws.prepare_child(&p, &id("r1-a-r0-a0.s1"))
                .await
                .expect("child");
        assert_ne!(c, p, "the child has its own worktree");
        assert_eq!(read(&c.join("src/a.rs")), "parent a\n");
        assert_eq!(read(&c.join("README")), "edited by the parent\n");
        assert_eq!(run(&p, &["rev-parse", "HEAD"]), head, "parent HEAD moved");
        assert_eq!(
            run(&p, &["status", "--porcelain"]),
            status,
            "the parent's index or files changed"
        );
        assert_eq!(
            f.ws.reopen_child(&p, &id("r1-a-r0-a0.s1"))
                .await
                .expect("again"),
            c,
            "a reopen returns the same worktree"
        );

        std::fs::create_dir_all(c.join("src")).expect("mkdir");
        std::fs::write(c.join("src/c.rs"), "child c\n").expect("write");
        std::fs::remove_file(c.join("docs/old.md")).expect("rm");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["src/c.rs", "docs/old.md"]))
                .await
                .expect("merge");
        let ChildMerge::Merged {
            commit,
            not_applied,
        } = merged
        else {
            panic!("expected a merge, got {merged:?}");
        };
        assert!(not_applied.is_empty(), "{not_applied:?}");
        assert_eq!(read(&p.join("src/c.rs")), "child c\n", "the child's work");
        assert!(!p.join("docs/old.md").exists(), "the child's deletion");
        assert_eq!(read(&p.join("src/a.rs")), "parent a\n");
        assert_eq!(run(&p, &["rev-parse", "HEAD"]), head, "parent HEAD moved");
        assert!(!c.exists(), "a merged child's worktree is removed");
        assert_eq!(
            run(
                &f.root,
                &["rev-parse", &child::ref_of("r1-a-r0-a0.s1", "result")]
            ),
            commit
        );
        // After a crash between the apply and the caller's record: applied
        // again from the refs alone, to the same end state.
        let again =
            f.ws.merge_child(&p, &c, &w(&["src/c.rs", "docs/old.md"]))
                .await
                .expect("merge again");
        assert!(matches!(again, ChildMerge::Merged { .. }), "{again:?}");
        assert_eq!(read(&p.join("src/c.rs")), "child c\n");
        // The parent's own commit still works: merge-back refuses a worktree
        // whose HEAD moved.
        let ws = f.ws.get(&p).expect("live");
        let mut attempt = f.ws.merge_back.attempt(ws).expect("attempt");
        let committed =
            f.ws.merge_back
                .commit(&mut attempt, "parent")
                .expect("commit");
        assert!(
            matches!(committed, kgraf_sandbox::CommitOutcome::Committed { .. }),
            "{committed:?}"
        );
    }

    /// The parent's later work on a path the child also changed is a typed
    /// conflict: nothing is written, and both worktrees stay as they were.
    #[tokio::test]
    async fn a_conflict_is_typed_and_leaves_both_worktrees_intact() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-b-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-b-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::create_dir_all(c.join("src")).expect("mkdir");
        std::fs::write(c.join("src/c.rs"), "child\n").expect("write");
        std::fs::write(c.join("src/d.rs"), "child d\n").expect("write");
        std::fs::create_dir_all(p.join("src")).expect("mkdir");
        std::fs::write(p.join("src/c.rs"), "parent later\n").expect("write");

        let merged =
            f.ws.merge_child(&p, &c, &w(&["src/c.rs", "src/d.rs"]))
                .await
                .expect("merge");
        let ChildMerge::Conflict { paths, detail, .. } = merged else {
            panic!("expected a conflict, got {merged:?}");
        };
        assert_eq!(paths, vec!["src/c.rs".to_owned()]);
        assert!(detail.contains("nothing was merged"), "{detail}");
        assert_eq!(read(&p.join("src/c.rs")), "parent later\n");
        assert!(
            !p.join("src/d.rs").exists(),
            "a conflict must not apply the child's other paths either"
        );
        assert_eq!(read(&c.join("src/c.rs")), "child\n", "child intact");
        assert_eq!(read(&c.join("src/d.rs")), "child d\n", "child intact");
        assert!(f.ws.get(&c).is_some(), "the child's worktree stays live");
    }

    /// A killed process's child worktree is reopened by its recorded handle,
    /// with the child's work in it, not branched again from a new checkpoint.
    #[tokio::test]
    async fn a_killed_processes_child_worktree_is_reopened_with_its_work() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-c-r0-a0");
        let parent_handle = f.ws.handle(&p).await.expect("handle");
        let c =
            f.ws.prepare_child(&p, &id("r1-c-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::create_dir_all(c.join("src")).expect("mkdir");
        std::fs::write(c.join("src/c.rs"), "before the kill\n").expect("write");
        std::fs::write(p.join("README"), "after the checkpoint\n").expect("write");

        let second = workspaces(&f.root, &f.base);
        let handle: WorktreeHandle = serde_json::from_str(&parent_handle).expect("json");
        let reopened = second
            .manager
            .reopen(&ReopenSpec {
                name: "r1-c-r0-a0",
                gitlink: &handle.gitlink,
                base: &handle.base,
                integration: None,
            })
            .expect("reopen parent");
        second
            .live()
            .insert(reopened.root().to_path_buf(), reopened);
        let again = second
            .reopen_child(&p, &id("r1-c-r0-a0.s1"))
            .await
            .expect("reopen");
        assert_eq!(again, c);
        assert_eq!(read(&c.join("src/c.rs")), "before the kill\n");
        assert_eq!(read(&c.join("README")), "hello\n", "the old checkpoint");
        let merged = second
            .merge_child(&p, &c, &w(&["src/c.rs"]))
            .await
            .expect("merge");
        assert!(matches!(merged, ChildMerge::Merged { .. }), "{merged:?}");
        assert_eq!(read(&p.join("src/c.rs")), "before the kill\n");
        assert_eq!(read(&p.join("README")), "after the checkpoint\n");
    }

    #[tokio::test]
    async fn a_child_of_a_worktree_this_process_does_not_hold_is_refused() {
        let f = fixture();
        let missing = f.base.join("nobody");
        let err =
            f.ws.prepare_child(&missing, &id("nobody.s1"))
                .await
                .expect_err("no parent");
        assert!(
            matches!(err, PrepareFailure::Unavailable(ref e) if e.contains("no live worktree")),
            "{err:?}"
        );
        let err =
            f.ws.reopen_child(&missing, &id("nobody.s1"))
                .await
                .expect_err("nothing recorded");
        assert!(
            matches!(err, PrepareFailure::Unavailable(ref e) if e.contains("no recorded worktree")),
            "{err:?}"
        );
    }

    /// An earlier child of the same id left its `result` ref (and
    /// its worktree). A new child of that id must never have that result
    /// applied: it would revert the parent to the earlier child's world.
    #[tokio::test]
    async fn a_stale_result_of_an_earlier_child_of_the_same_id_is_never_applied() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-h-r0-a0");
        let name = "r1-h-r0-a0.s1";
        let old = f.ws.prepare_child(&p, &id(name)).await.expect("old child");
        std::fs::write(old.join("README"), "the old child's world\n").expect("write");
        // A merge whose snapshot was taken and whose apply never happened (a
        // kill), leaving the result ref behind.
        let old_ws = f.ws.get(&old).expect("live");
        let _ = child::merge(
            &f.ws.manager,
            &f.ws.get(&p).expect("p"),
            Some(&old_ws),
            name,
            &w(&[]),
        );
        assert!(!refs_under(&f.root, &child::ref_of(name, "result")).is_empty());
        // The parent moves on.
        std::fs::write(p.join("new.txt"), "new\n").expect("write");
        std::fs::write(p.join("README"), "parent now\n").expect("write");

        // A new child, same id: a fresh worktree from a fresh checkpoint.
        let c = f.ws.prepare_child(&p, &id(name)).await.expect("new child");
        assert_eq!(read(&c.join("new.txt")), "new\n", "branched from now");
        assert!(
            refs_under(&f.root, &child::ref_of(name, "result")).is_empty(),
            "the stale result is gone"
        );
        std::fs::write(c.join("src.txt"), "child\n").expect("write");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["src.txt"]))
                .await
                .expect("merge");
        assert!(matches!(merged, ChildMerge::Merged { .. }), "{merged:?}");
        assert_eq!(read(&p.join("README")), "parent now\n", "not reverted");
        assert_eq!(read(&p.join("new.txt")), "new\n", "not deleted");
        assert_eq!(read(&p.join("src.txt")), "child\n");

        // And a result ref that does not descend from the checkpoint is
        // refused outright, whatever wrote it.
        let c2 =
            f.ws.prepare_child(&p, &id("r1-h-r0-a0.s2"))
                .await
                .expect("c2");
        let stray = run(&f.root, &["rev-parse", "main"]);
        run(
            &f.root,
            &[
                "update-ref",
                &child::ref_of("r1-h-r0-a0.s2", "result"),
                &stray,
            ],
        );
        let err =
            f.ws.merge_child(&p, &c2, &w(&["README"]))
                .await
                .expect_err("a stray result");
        assert!(
            err.contains("does not descend from its checkpoint"),
            "{err}"
        );
        assert_eq!(read(&p.join("README")), "parent now\n");
    }

    /// Decision 0083: after the checkpoint the parent's worker swaps a folder
    /// for a link out of its tree (`src -> <temp>/outside`, a directory link)
    /// and plants a leaf link (`docs/old.md -> <temp>/outside/rc`). The
    /// child's write under `src/` and its deletion of `docs/old.md` never
    /// reach outside: the merge is a typed conflict naming both paths, nothing
    /// is applied, and the files outside are as they were.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_the_parent_plants_after_the_checkpoint_is_a_conflict_never_followed() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-l-r0-a0");
        std::fs::create_dir_all(p.join("src")).expect("mkdir");
        std::fs::write(p.join("src/keep.rs"), "k\n").expect("write");
        let c =
            f.ws.prepare_child(&p, &id("r1-l-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("src/c.rs"), "child c\n").expect("write");
        std::fs::remove_file(c.join("docs/old.md")).expect("rm");

        let outside = f._dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("keep.rs"), "k\n").expect("write");
        std::fs::write(outside.join("rc"), "old\n").expect("write");
        std::fs::remove_dir_all(p.join("src")).expect("rm");
        std::os::unix::fs::symlink(&outside, p.join("src")).expect("link");
        std::fs::remove_file(p.join("docs/old.md")).expect("rm");
        std::os::unix::fs::symlink(outside.join("rc"), p.join("docs/old.md")).expect("link");

        let merged =
            f.ws.merge_child(&p, &c, &w(&["src/c.rs", "docs/old.md"]))
                .await
                .expect("merge");
        let ChildMerge::Conflict { mut paths, .. } = merged else {
            panic!("expected a conflict, got {merged:?}");
        };
        paths.sort();
        assert_eq!(paths, w(&["docs/old.md", "src/c.rs"]));
        assert!(!outside.join("c.rs").exists(), "nothing written outside");
        assert_eq!(
            read(&outside.join("rc")),
            "old\n",
            "nothing removed outside"
        );
        assert!(
            p.join("docs/old.md")
                .symlink_metadata()
                .expect("meta")
                .is_symlink(),
            "the parent's link is left for the parent to answer for"
        );
    }

    /// Decision 0083: the child's result holds `Foo.rs` and `foo.rs`, which
    /// APFS and NTFS resolve as one file. Both are a conflict; neither is
    /// written, so one never overwrites the other in the parent.
    #[tokio::test]
    async fn two_child_paths_the_file_system_folds_as_one_are_a_conflict() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-f-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-f-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("Foo.rs"), "upper\n").expect("write");
        std::fs::write(c.join("foo.rs"), "upper\n").expect("write");
        // Both names in the child's index whatever the file system folds.
        let blob = run(&c, &["hash-object", "-w", "Foo.rs"]);
        for name in ["Foo.rs", "foo.rs"] {
            run(
                &c,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{name}"),
                ],
            );
        }
        let merged =
            f.ws.merge_child(&p, &c, &w(&["Foo.rs", "foo.rs"]))
                .await
                .expect("merge");
        let ChildMerge::Conflict { mut paths, .. } = merged else {
            panic!("expected a conflict, got {merged:?}");
        };
        paths.sort();
        assert_eq!(paths, w(&["Foo.rs", "foo.rs"]));
        assert!(!p.join("Foo.rs").exists(), "nothing applied");
    }

    /// A kill between `prepare_child` and the spawn record left a
    /// worktree and refs that no record names. A fresh spawn of that id is
    /// not refused for good: what was left is removed and it starts clean.
    #[tokio::test]
    async fn a_fresh_spawn_over_what_a_killed_prepare_left_starts_clean() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-m-r0-a0");
        let parent_handle = f.ws.handle(&p).await.expect("handle");
        let c =
            f.ws.prepare_child(&p, &id("r1-m-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("leftover.txt"), "from the killed process\n").expect("write");
        // The kill: a new process knows nothing of it.
        let second = workspaces(&f.root, &f.base);
        let handle: WorktreeHandle = serde_json::from_str(&parent_handle).expect("json");
        let reopened = second
            .manager
            .reopen(&ReopenSpec {
                name: "r1-m-r0-a0",
                gitlink: &handle.gitlink,
                base: &handle.base,
                integration: None,
            })
            .expect("reopen parent");
        second
            .live()
            .insert(reopened.root().to_path_buf(), reopened);
        // The first process's lock owner is this test process, still alive:
        // re-tag the stale worktree's lock with a dead PID, as a killed
        // owner's is.
        let locked = f.root.join(".git/worktrees/r1-m-r0-a0.s1/locked");
        let reason = std::fs::read_to_string(&locked).expect("lock reason");
        let pid = format!(":pid={}:", std::process::id());
        assert!(reason.contains(&pid), "{reason}");
        std::fs::write(&locked, reason.replace(&pid, ":pid=999999:")).expect("retag");
        let again = second
            .prepare_child(&p, &id("r1-m-r0-a0.s1"))
            .await
            .expect("a fresh spawn over the leftovers is not refused");
        assert!(!again.join("leftover.txt").exists(), "started clean");

        // Discard removes the worktree and every ref.
        second
            .discard_child(&id("r1-m-r0-a0.s1"))
            .await
            .expect("discard");
        assert!(!again.exists());
        assert!(refs_under(&f.root, &format!("{}r1-m-r0-a0.s1/", child::REF_ROOT)).is_empty());
    }

    /// With `eol=crlf` (and `core.autocrlf=true`, the Git for
    /// Windows default) the file on disk is not the stored blob. Neither
    /// side changing a file must not read as a conflict, and the child's
    /// version lands with the parent's line endings.
    #[tokio::test]
    async fn line_ending_filters_are_neither_a_false_conflict_nor_lost() {
        let f = fixture_with(&[
            (".gitattributes", b"*.txt text eol=crlf\n"),
            ("a.txt", b"one\ntwo\n"),
            ("b.txt", b"b\n"),
        ]);
        let p = parent(&f, &f.ws, "r1-e-r0-a0");
        assert_eq!(
            std::fs::read(p.join("a.txt")).expect("read"),
            b"one\r\ntwo\r\n",
            "checked out with CRLF"
        );
        let c =
            f.ws.prepare_child(&p, &id("r1-e-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("a.txt"), "one\r\ntwo\r\nthree\r\n").expect("write");
        std::fs::write(c.join("new.txt"), "fresh\r\n").expect("write");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["a.txt", "b.txt", "new.txt"]))
                .await
                .expect("merge");
        assert!(matches!(merged, ChildMerge::Merged { .. }), "{merged:?}");
        assert_eq!(
            std::fs::read(p.join("a.txt")).expect("read"),
            b"one\r\ntwo\r\nthree\r\n",
            "smudged for the parent's worktree"
        );
        assert_eq!(
            std::fs::read(p.join("new.txt")).expect("read"),
            b"fresh\r\n"
        );
        assert_eq!(
            run(&p, &["status", "--porcelain", "--", "b.txt"]),
            "",
            "b.txt untouched"
        );
    }

    /// A worker-written `.gitattributes` selecting a
    /// filter the user configured never runs it on the host while a
    /// subagent's checkpoint is taken, and the checkpoint is refused, loudly.
    /// The sandbox's `worker_attributes_never_run_a_filter_on_the_host`,
    /// ported to the checkpoint snapshot.
    #[tokio::test]
    async fn a_parents_attributes_never_run_a_filter_while_a_checkpoint_is_taken() {
        let f = fixture();
        let marker = f.base.parent().expect("dir").join("FILTER_RAN");
        run(
            &f.root,
            &[
                "config",
                "filter.evil.clean",
                &format!("echo ran >> '{}'; cat", marker.display()),
            ],
        );
        // Control: plain git in a worktree with the planted attributes runs it.
        let ctl = parent(&f, &f.ws, "r1-ctl-r0-a0");
        std::fs::write(ctl.join(".gitattributes"), "* filter=evil\n").expect("write");
        std::fs::write(ctl.join("x.txt"), "x\n").expect("write");
        run(&ctl, &["add", "x.txt"]);
        assert!(marker.exists(), "control: the filter is live");
        std::fs::remove_file(&marker).expect("rm");

        let p = parent(&f, &f.ws, "r1-f-r0-a0");
        std::fs::write(p.join(".gitattributes"), "* filter=evil\n").expect("write");
        std::fs::write(p.join("x.txt"), "x\n").expect("write");
        let refused = f.ws.prepare_child(&p, &id("r1-f-r0-a0.s1")).await;
        let Err(PrepareFailure::Unavailable(why)) = refused else {
            panic!("{refused:?}");
        };
        assert!(why.contains(".gitattributes"), "{why}");
        assert!(!marker.exists(), "the filter ran on the host");
    }

    /// A mode-only change is applied, not dropped.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mode_only_change_is_merged() {
        use std::os::unix::fs::PermissionsExt as _;
        let f = fixture_with(&[("run.sh", b"echo hi\n")]);
        let p = parent(&f, &f.ws, "r1-x-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-x-r0-a0.s1"))
                .await
                .expect("child");
        let mut perms = std::fs::metadata(c.join("run.sh"))
            .expect("meta")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(c.join("run.sh"), perms).expect("chmod");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["run.sh"]))
                .await
                .expect("merge");
        assert!(matches!(merged, ChildMerge::Merged { .. }), "{merged:?}");
        let mode = std::fs::metadata(p.join("run.sh"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable in the parent: {mode:o}");
    }

    /// A write that fails part-way reports what was already
    /// applied.
    #[tokio::test]
    async fn a_merge_failing_part_way_reports_the_paths_already_applied() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-f-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-f-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("a.txt"), "a\n").expect("write");
        std::fs::create_dir_all(c.join("x")).expect("mkdir");
        std::fs::write(c.join("x/y.txt"), "y\n").expect("write");
        // In the parent, `x` is now a file: `x/y.txt` cannot be written.
        std::fs::write(p.join("x"), "a file\n").expect("write");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["a.txt", "x/y.txt"]))
                .await
                .expect("merge");
        let ChildMerge::Failed { applied, detail } = merged else {
            panic!("expected a failure part-way, got {merged:?}");
        };
        assert_eq!(applied, vec!["a.txt".to_owned()], "{detail}");
        assert!(detail.contains("x/y.txt"), "{detail}");
        assert_eq!(read(&p.join("a.txt")), "a\n");
        assert!(f.ws.get(&c).is_some(), "the child's worktree is kept");
    }

    /// Only declared writes are applied; any other changed path
    /// is reported.
    #[tokio::test]
    async fn only_declared_writes_are_applied_and_the_rest_reported() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-l-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-l-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("mine.txt"), "declared\n").expect("write");
        std::fs::write(c.join("README"), "not declared\n").expect("write");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["mine.txt"]))
                .await
                .expect("merge");
        let ChildMerge::Merged { not_applied, .. } = merged else {
            panic!("expected a merge, got {merged:?}");
        };
        assert_eq!(not_applied, vec!["README".to_owned()]);
        assert_eq!(read(&p.join("mine.txt")), "declared\n");
        assert_eq!(read(&p.join("README")), "hello\n", "not applied");
    }

    /// A declared write the snapshot leaves out (a `.gitignore`
    /// match) is reported, never read as "empty".
    #[tokio::test]
    async fn a_declared_write_the_snapshot_left_out_is_reported() {
        let f = fixture_with(&[(".gitignore", b"*.log\n"), ("README", b"r\n")]);
        let p = parent(&f, &f.ws, "r1-g-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-g-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("out.log"), "the child's work\n").expect("write");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["out.log"]))
                .await
                .expect("merge");
        let ChildMerge::Dropped { paths, .. } = merged else {
            panic!("expected the dropped path reported, got {merged:?}");
        };
        assert_eq!(paths, vec!["out.log".to_owned()]);
        assert!(f.ws.get(&c).is_some(), "the child's worktree is kept");
    }

    /// A child's refs are swept once no open contract names it.
    #[tokio::test]
    async fn a_childs_refs_are_swept_once_its_contract_closes() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-s-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-s-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("s.txt"), "s\n").expect("write");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["s.txt"]))
                .await
                .expect("merge");
        assert!(matches!(merged, ChildMerge::Merged { .. }), "{merged:?}");
        assert!(!refs_under(&f.root, child::REF_ROOT).is_empty());
        let mut record =
            kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "x", 0);
        let swept_open = {
            // While a contract still names the child, its refs stay.
            let mut open = record.clone();
            let mut contract = kgraf_orchestrator::OpenContract::planned("s", "t".into(), "x", "y");
            contract
                .worktrees
                .insert("r1-s-r0-a0.s1".into(), "{}".into());
            open.open_contracts.push(contract);
            f.ws.sweep_refs(Some(&open)).expect("sweep")
        };
        assert!(swept_open.iter().all(|r| !r.contains("subagents")));
        assert!(!refs_under(&f.root, child::REF_ROOT).is_empty());
        record.open_contracts.clear();
        let swept = f.ws.sweep_refs(Some(&record)).expect("sweep");
        assert!(
            swept.iter().any(|r| r.contains("r1-s-r0-a0.s1")),
            "{swept:?}"
        );
        assert!(
            refs_under(&f.root, child::REF_ROOT).is_empty(),
            "no subagent refs remain"
        );
    }
}
