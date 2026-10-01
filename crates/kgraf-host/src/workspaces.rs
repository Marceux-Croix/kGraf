//! Git worktrees for a run: one per worker, the integration branch, startup
//! cleanup, and `kgraf clean`.

use crate::config::WorktreeRetention;
use crate::error::{Error, Result, ResultExt};
use kgraf_graph::GraphStore;
use kgraf_orchestrator::{
    ChildMerge, Integration, OpenContract, PrepareFailure, RunStore, Workspaces,
};
use kgraf_sandbox::{
    Attempt, FinishOutcome, MergeBack, MergeOutcome, MergedWorktree, ReopenSpec, Workspace,
    WorkspaceManager, WorkspaceSpec,
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
///
/// Kept worktrees live in two places under the state directory
/// ([`WORKTREE_BASES`]): an orchestrated run's under `worktrees`, and
/// `kgraf worker`'s under `workspaces` (decision 0088). `name` is looked up
/// in both; `worktrees/<name>` or `workspaces/<name>` names one. A name in
/// neither is refused listing every kept worktree in both; a name in both is
/// refused until the caller says which.
pub fn clean(root: &Path, name: &str, force: bool) -> Result<PathBuf> {
    let state = state_dir(root)?;
    let (only, leaf) = match name.split_once('/') {
        Some((base, leaf)) if WORKTREE_BASES.contains(&base) => (Some(base), leaf),
        _ => (None, name),
    };
    let git = kgraf_sandbox::git::Git::discover().step("cannot find git")?;
    let entries = git
        .worktree_list(root)
        .step("cannot list the repository's worktrees")?;
    let mut found = Vec::new();
    let mut kept = Vec::new();
    for base in WORKTREE_BASES {
        let dir = state.join(base);
        let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let mut names: Vec<String> = entries
            .iter()
            .filter(|e| e.path.parent() == Some(canonical.as_path()))
            .filter_map(|e| e.path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        if only.is_none_or(|o| o == *base) && names.iter().any(|n| n == leaf) {
            found.push((*base, dir));
        }
        kept.extend(names.into_iter().map(|n| format!("{base}/{n}")));
    }
    match found.as_slice() {
        [(_, dir)] => clean_in(root, dir, leaf, force),
        [] => Err(Error::Invalid(format!(
            "no worktree named {name} under .kgraf/worktrees (runs) or .kgraf/workspaces \
             (kgraf worker); kept worktrees: {}",
            if kept.is_empty() {
                "none".to_owned()
            } else {
                kept.join(", ")
            }
        ))),
        _ => Err(Error::Invalid(format!(
            "a worktree named {leaf} exists under both .kgraf/worktrees and .kgraf/workspaces; \
             name one: `kgraf clean worktrees/{leaf}` or `kgraf clean workspaces/{leaf}`"
        ))),
    }
}

/// The directories under the state directory that hold worktrees kGraf
/// makes and may keep: an orchestrated run's (`worktrees`) and
/// `kgraf worker`'s (`workspaces`).
pub const WORKTREE_BASES: &[&str] = &["worktrees", "workspaces"];

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
/// removed (conversation, probe, gate and interface-stub worktrees, worktrees a
/// record dropped), except a worktree some run record still names, which a
/// resume reopens. Worktrees of live processes (another run in progress) are
/// never touched.
///
/// With [`WorktreeRetention::Keep`] (decision 0088) a worker's attempt
/// worktree (a worker's or a writer subagent's) is kept too, whether or not a
/// record names it. The worktrees kGraf makes for itself
/// ([`is_harness_worktree`]) are still removed: they hold no worker's
/// attempt.
pub(crate) fn clean_stale_worktrees(
    manager: &WorkspaceManager,
    store: &RunStore,
    retention: WorktreeRetention,
) -> Result<Vec<kgraf_sandbox::OrphanRemoval>> {
    let referenced = store
        .referenced_worktrees()
        .step("cannot read the run records")?;
    manager
        .startup_keeping(&|name| keeps_at_startup(retention, name, &referenced))
        .step("cannot clean the worktree base")
}

/// Whether startup cleanup leaves the worktree `name` alone (see
/// [`clean_stale_worktrees`]).
fn keeps_at_startup(
    retention: WorktreeRetention,
    name: &str,
    referenced: &BTreeSet<String>,
) -> bool {
    referenced.contains(name) || (!retention.deletes() && !is_harness_worktree(name))
}

/// A conversation worktree ([`Workspaces::open_comms`]).
pub(crate) const COMMS_PREFIX: &str = "comms-";
/// A worktree a gate runs its commands in (`gates::CommandGateRunner`).
pub(crate) const GATE_PREFIX: &str = "gate-";
/// A worktree that probes the sandbox (`assembler`).
pub(crate) const PROBE_PREFIX: &str = "probe-";
/// The run's integration view ([`Workspaces::integration_view`]).
pub(crate) const VIEW_PREFIX: &str = "view-";

/// The name of a worktree kGraf makes for itself at `now_nanos()`.
pub(crate) fn harness_worktree_name(prefix: &str) -> String {
    format!("{prefix}{}", now_nanos())
}

/// Whether `name` is a worktree kGraf makes for itself rather than a worker's
/// attempt: a conversation, gate, probe or integration-view worktree
/// (`<prefix><nanos>`), or an
/// interface-stub landing (`stub-<id>-v<version>`, `GitStubLander`). A worker's
/// worktree never matches: its name ends in `-a<n>` (`<run>-<contract>-r<n>...
/// -a<n>`), a writer subagent's in `.s<n>`.
pub(crate) fn is_harness_worktree(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let stamped = [COMMS_PREFIX, GATE_PREFIX, PROBE_PREFIX, VIEW_PREFIX]
        .iter()
        .any(|p| name.strip_prefix(p).is_some_and(digits));
    let stub = name
        .strip_prefix("stub-")
        .and_then(|rest| rest.rsplit_once("-v"))
        .is_some_and(|(id, version)| !id.is_empty() && digits(version));
    stamped || stub
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
    /// gives up. While a run record names the worktree as a resolver's source,
    /// `sweep_refs` keeps its attempt ref, so a resolver resumed by a later
    /// process can still read the commit; that process settles it by name.
    conflicted: Mutex<BTreeMap<PathBuf, Attempt>>,
    /// Whether a released worktree that nothing needs is removed with its
    /// build output (decision 0088).
    retention: WorktreeRetention,
    /// The user's permission control: in the Bypass mode a snapshot carries
    /// `.gitattributes`, `.gitmodules` and `.githooks` changes as plain data
    /// (decision 0094); in every other mode it refuses them.
    permissions: Arc<kgraf_tools::PermissionControl>,
    /// Worktrees and attempts that could not be removed, for the orchestrator
    /// to show in the run's report ([`Workspaces::take_problems`]). Shared
    /// with the removals that run on a blocking thread or on drop.
    problems: Problems,
    /// The run's integration view, made at the first
    /// [`Workspaces::integration_view`] call. An async lock: the call holds
    /// it while git moves the tree, so two callers never move it at once.
    view: tokio::sync::Mutex<Option<IntegrationView>>,
}

/// A detached worktree at the integration tip that the graph reads merged
/// files from for the rest of the run ([`Workspaces::integration_view`]).
///
/// One tree per run, moved in place to each new tip (`git checkout
/// --detach`), so a file's recorded source root stays valid across merges and
/// only the changed files are rewritten; a fresh worktree per merge would
/// check out the whole repository each time and orphan the roots the graph
/// recorded for earlier merges. It holds no build and no worker's work.
/// Dropped with the run's [`GitWorkspaces`]: removed under
/// [`WorktreeRetention::Delete`], left under `Keep` (startup cleanup removes
/// it as a kGraf-made worktree either way).
pub(crate) struct IntegrationView {
    manager: Arc<WorkspaceManager>,
    ws: Option<Workspace>,
    /// The commit the tree is checked out at.
    head: String,
    remove: bool,
    problems: Problems,
}

impl IntegrationView {
    fn root(&self) -> Option<PathBuf> {
        self.ws.as_ref().map(|w| w.root().to_path_buf())
    }

    /// Move the tree to `tip`. Attributes are read from `tip`, a commit on the
    /// integration branch, never from the tree's own files.
    fn advance(&mut self, tip: &str) -> std::result::Result<(), String> {
        let ws = self
            .ws
            .as_ref()
            .ok_or_else(|| "the integration view was removed".to_owned())?;
        let admin = child::admin_of(ws)?;
        child::git(
            self.manager.repo(),
            Some(child::Wt {
                admin: &admin,
                work: ws.root(),
                attr: tip,
            }),
            None,
            &["checkout", "--quiet", "--force", "--detach", tip],
            None,
        )?;
        self.head = tip.to_owned();
        Ok(())
    }
}

impl Drop for IntegrationView {
    fn drop(&mut self) {
        let Some(ws) = self.ws.take() else { return };
        if !self.remove || !ws.root().exists() {
            return;
        }
        let root = ws.root().display().to_string();
        if let Err(e) = self.manager.remove(ws) {
            self.problems.push(format!(
                "integration view {root} could not be removed: {e}; `kgraf clean` removes it"
            ));
        }
    }
}

/// Where each failed removal or discard is recorded, once, for the run's
/// report (decision 0088: fail loud).
#[derive(Clone, Default)]
pub(crate) struct Problems(Arc<Mutex<Vec<String>>>);

impl Problems {
    pub(crate) fn push(&self, text: String) {
        eprintln!("{text}");
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(text);
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

impl GitWorkspaces {
    pub(crate) fn new(manager: Arc<WorkspaceManager>, merge_back: Arc<MergeBack>) -> Self {
        Self {
            retention: WorktreeRetention::default(),
            permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
            problems: Problems::default(),
            manager,
            merge_back,
            live: Mutex::new(BTreeMap::new()),
            committed: Mutex::new(BTreeMap::new()),
            empty: Mutex::new(BTreeSet::new()),
            touched: Mutex::new(BTreeSet::new()),
            conflicted: Mutex::new(BTreeMap::new()),
            view: tokio::sync::Mutex::new(None),
        }
    }

    /// The worktree policy this run applies when it releases a worktree
    /// (decision 0088).
    pub(crate) fn with_retention(mut self, retention: WorktreeRetention) -> Self {
        self.retention = retention;
        self
    }

    /// The user's permission control, read at each snapshot (decision 0094).
    pub(crate) fn with_permissions(mut self, control: Arc<kgraf_tools::PermissionControl>) -> Self {
        self.permissions = control;
        self
    }

    fn bypass_git_files(&self) -> bool {
        self.permissions.mode() == kgraf_tools::permissions::PermissionMode::Bypass
    }

    /// The policy this provider applies (tests).
    #[cfg(test)]
    pub(crate) fn retention(&self) -> WorktreeRetention {
        self.retention
    }

    fn problem(&self, text: String) {
        self.problems.push(text);
    }

    /// Settle an attempt that will never be merged: a losing or failed one,
    /// one merge-back refused, a conflict its resolver settled. Its attempt
    /// refs are deleted either way. Under [`WorktreeRetention::Delete`] its
    /// worktree goes with it, build output included (merge-back's discard);
    /// under [`WorktreeRetention::Keep`] the worktree stays for the user, and
    /// without its refs nothing can reopen it as live work (a reopen needs
    /// HEAD at its base or at a commit an attempt ref names). A failure is
    /// reported once, naming the worktree.
    async fn retire_unmerged(&self, attempt: Attempt) {
        let merge_back = self.merge_back.clone();
        let deletes = self.retention.deletes();
        let failed = tokio::task::spawn_blocking(move || {
            let mut attempt = attempt;
            let root = attempt.workspace().root().display().to_string();
            if deletes {
                merge_back.discard(&mut attempt).err().map(|e| {
                    format!(
                        "attempt {root} could not be discarded: {e}; its worktree stays, and \
                         `kgraf clean` removes it"
                    )
                })
            } else {
                merge_back
                    .sweep_attempt_refs(&[attempt.name().to_owned()])
                    .err()
                    .map(|e| {
                        format!(
                            "the attempt refs of the kept worktree {root} were not deleted: {e}"
                        )
                    })
            }
        })
        .await;
        match failed {
            Ok(Some(text)) => self.problem(text),
            Ok(None) => {}
            Err(e) => self.problem(format!("settling an attempt did not finish: {e}")),
        }
    }

    /// Remove a worktree this process holds, with its build output, under
    /// [`WorktreeRetention::Delete`]; under `Keep`, or when it is already
    /// gone, nothing. A failure is reported once.
    async fn remove_unless_kept(&self, ws: Workspace) {
        if !self.retention.deletes() || !ws.root().exists() {
            return;
        }
        let manager = self.manager.clone();
        let root = ws.root().to_path_buf();
        match tokio::task::spawn_blocking(move || manager.remove(ws)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => self.problem(format!(
                "worktree {} could not be removed: {e}; `kgraf clean` removes it",
                root.display()
            )),
            Err(e) => self.problem(format!(
                "removing worktree {} did not finish: {e}",
                root.display()
            )),
        }
    }

    /// The worktree `name` under this run's base, as this process holds it,
    /// or reopened with its work from the handle `contract` recorded for it
    /// (`contract.worktrees[name]`). `None` when it does not exist on disk.
    async fn reopen_recorded(
        &self,
        contract: &OpenContract,
        name: &str,
        inherited: bool,
    ) -> std::result::Result<Option<PathBuf>, PrepareFailure> {
        // What happens next, said with the refusal: a worker's own worktree
        // is replaced by a fresh one, an inherited one never is.
        const INHERITED: &str = "it holds the handoff parent's work, which no fresh worktree \
                                 has, so the contract is retried after a backoff and held if \
                                 it stays refused";
        let (unrecorded, refused) = if inherited {
            (INHERITED, INHERITED)
        } else {
            (
                "the attempt starts over with a fresh worker",
                "the contract is retried after a backoff with a fresh worker in a fresh \
                 worktree",
            )
        };
        if let Some(ws) = self.live().values().find(|w| w.name() == name) {
            return Ok(Some(ws.root().to_path_buf()));
        }
        let path = self.manager.base().join(name);
        if !path.exists() {
            return Ok(None);
        }
        let recorded = contract
            .worktrees
            .get(name)
            .map(|h| serde_json::from_str::<WorktreeHandle>(h));
        let handle = match recorded {
            Some(Ok(h)) => h,
            Some(Err(e)) => {
                return Err(format!("the recorded worktree is unreadable: {e}").into());
            }
            None => {
                return Err(format!(
                    "worktree {} was left with no recorded gitlink and cannot be \
                     reopened; {unrecorded}",
                    path.display()
                )
                .into());
            }
        };
        let manager = self.manager.clone();
        let owned = name.to_owned();
        let branch = format!("refs/heads/{}", self.merge_back.integration_branch());
        let ws = tokio::task::spawn_blocking(move || {
            manager.reopen(&ReopenSpec {
                name: &owned,
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
                "{busy}; the same worker is retried after a backoff, so its work is kept"
            )),
            other => PrepareFailure::Unavailable(format!("{other}; {refused}")),
        })?;
        let root = ws.root().to_path_buf();
        self.live().insert(root.clone(), ws);
        Ok(Some(root))
    }

    /// At the end of a run: delete the attempt refs of every worktree this
    /// process worked in that no open contract still uses.
    ///
    /// Subagent refs are shared by every run in the repository, so a child
    /// is kept while any run still names it: `referenced` returns every
    /// worktree name any run record names ([`RunStore::referenced_worktrees`]).
    /// It is read after the refs are listed: a child's spawn record is saved
    /// before its refs exist, so every listed child of another live run is
    /// already in it, and a child spawned after the listing is not deleted.
    ///
    /// [`RunStore::referenced_worktrees`]: kgraf_orchestrator::RunStore::referenced_worktrees
    pub(crate) fn sweep_refs(
        &self,
        record: Option<&kgraf_orchestrator::RunStateRecord>,
        referenced: impl FnOnce() -> Result<BTreeSet<String>>,
    ) -> Result<Vec<String>> {
        // A worktree an open contract still names keeps its attempt refs: a
        // worker's own, the one a handoff child inherited (its retry reopens
        // it at the ref's commit), and a conflicted attempt a resolver cites.
        let name_of = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
        let still: BTreeSet<String> = record
            .map(|r| {
                r.open_contracts
                    .iter()
                    .flat_map(|c| {
                        c.workers
                            .iter()
                            .map(|w| w.to_string())
                            .chain(c.worktrees.keys().cloned())
                            .chain(c.workspace.as_deref().and_then(name_of))
                            .chain(c.resolver_source.as_deref().and_then(name_of))
                            .collect::<Vec<_>>()
                    })
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
        let mut keep: BTreeSet<String> = record
            .map(|r| {
                r.open_contracts
                    .iter()
                    .flat_map(|c| c.worktrees.keys().cloned())
                    .collect()
            })
            .unwrap_or_default();
        let listed = child::list_refs(self.manager.repo())
            .map_err(|e| Error::Invalid(format!("subagent refs were not swept: {e}")))?;
        keep.extend(referenced()?);
        swept.extend(
            child::sweep(self.manager.repo(), listed, &keep)
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
        contract: &OpenContract,
        worker: &kgraf_core::AgentId,
    ) -> std::result::Result<PathBuf, PrepareFailure> {
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(worker.to_string());
        if let Some(root) = self
            .reopen_recorded(contract, worker.as_str(), false)
            .await?
        {
            return Ok(root);
        }
        let manager = self.manager.clone();
        let merge_back = self.merge_back.clone();
        let name = worker.to_string();
        let pinned = contract.base.clone();
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

    /// The worktree a handoff child inherited (decision 0067): the one this
    /// process holds, or reopened with the parent's work from the handle the
    /// child recorded under the worktree's name. Refused, never replaced,
    /// when it is outside this run's worktree base, gone, or not intact.
    async fn adopt(
        &self,
        contract: &OpenContract,
        workspace: &Path,
    ) -> std::result::Result<PathBuf, PrepareFailure> {
        let name = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|_| workspace.parent() == Some(self.manager.base()))
            .ok_or_else(|| {
                PrepareFailure::Unavailable(format!(
                    "the inherited worktree {} is not a worktree of this run",
                    workspace.display()
                ))
            })?
            .to_owned();
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.clone());
        self.reopen_recorded(contract, &name, true)
            .await?
            .ok_or_else(|| {
                PrepareFailure::Unavailable(format!(
                    "the inherited worktree {} is gone; it held the handoff parent's work, \
                     so no fresh worktree replaces it",
                    workspace.display()
                ))
            })
    }

    async fn handle(&self, workspace: &Path) -> Option<String> {
        let ws = self.get(workspace)?;
        serde_json::to_string(&WorktreeHandle {
            gitlink: ws.gitlink().to_owned(),
            base: ws.head().to_owned(),
        })
        .ok()
    }

    /// The winner was merged by `integrate`, which removed its worktree with
    /// the merge under [`WorktreeRetention::Delete`] and left it, with its
    /// builds, under [`WorktreeRetention::Keep`]. A worktree released with
    /// `keep` stays on disk as it is: its worker will be continued in it
    /// (reopened by `prepare`), an open contract still names it (a handoff
    /// child adopts it, a resolver cites it), or it is the merged winner.
    ///
    /// Otherwise the attempt in it is settled for good, under the run's policy
    /// (decision 0088). A committed attempt (a loser, a failed check, a merge
    /// merge-back refused) has its attempt refs deleted, and its worktree is
    /// removed under `Delete` and kept under `Keep`. A worktree never committed
    /// (a worker that failed, an empty attempt) is removed under `Delete` and
    /// kept under `Keep`. A removal or discard that fails is reported once
    /// ([`Workspaces::take_problems`]), never dropped.
    async fn release(&self, workspace: &Path, keep: bool) {
        let live = self.live().remove(workspace);
        self.empty
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(workspace);
        if keep {
            return;
        }
        let committed = self.committed().remove(workspace);
        if let Some(attempt) = committed {
            // The discard removes the worktree too: a failure is one report,
            // and the worktree it left is not removed a second time.
            self.retire_unmerged(attempt).await;
            return;
        }
        if let Some(ws) = live {
            self.remove_unless_kept(ws).await;
        }
    }

    fn take_problems(&self) -> Vec<String> {
        self.problems.take()
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
            // Nothing to merge. The verifier left the worktree for this
            // call: it goes now under `Delete`, and stays under `Keep`.
            let live = self.live().remove(workspace);
            if let Some(ws) = live {
                self.remove_unless_kept(ws).await;
            }
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
        // Decision 0088: under `Keep` the merged worktree stays with its
        // builds; under `Delete` merge-back removes it with the merge. The
        // graph never reads it after the merge: it reads the integration
        // view (`integration_view`), which holds the tip either way.
        let after = if self.retention.deletes() {
            MergedWorktree::Remove
        } else {
            MergedWorktree::Keep
        };
        // The attempt comes back out on a conflict (0073): kept, not
        // discarded, so a resolver's fresh worktree can read its commit; the
        // caller below stores it under `workspace` for `discard_conflict` to
        // settle once the resolver (or the plain fallback) is done with it.
        // A merge merge-back refused comes back out too, for `release` to
        // settle once no open contract names the worktree.
        let result =
            tokio::task::spawn_blocking(move || match merge_back.merge_then(&mut attempt, after) {
                Ok(MergeOutcome::Merged { merge_commit, .. }) => (
                    Ok(Integration::Merged {
                        commit: merge_commit,
                    }),
                    Back::Settled,
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
                    (Ok(outcome), Back::Conflicted(attempt))
                }
                // The work is on the branch; only the worktree's removal
                // failed. A merge, never a refusal that would redo it.
                Err(kgraf_sandbox::MergeError::CleanupFailed {
                    merge_commit,
                    source,
                }) => {
                    let root = attempt.workspace().root().display().to_string();
                    (
                        Ok(Integration::Merged {
                            commit: merge_commit,
                        }),
                        Back::Problem(format!(
                            "the attempt in {root} merged, but the cleanup after the merge \
                             failed: {source}; `kgraf clean` removes the worktree"
                        )),
                    )
                }
                Err(e) => (Err(format!("merge-back: {e}")), Back::Refused(attempt)),
            })
            .await
            .map_err(|e| e.to_string())?;
        let (outcome, back) = result;
        match back {
            Back::Settled => {}
            Back::Conflicted(attempt) => {
                self.conflicted
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(workspace.to_path_buf(), attempt);
            }
            Back::Refused(attempt) => {
                self.committed().insert(workspace.to_path_buf(), attempt);
            }
            Back::Problem(text) => self.problem(text),
        }
        outcome
    }

    /// The conflicted attempt kept at `workspace` (0073) is settled: a
    /// resolver merged its fix, or the contract gave up. Either way it is
    /// superseded, never merged itself: its attempt refs go, and its worktree
    /// is removed under [`WorktreeRetention::Delete`] and kept under `Keep`
    /// ([`GitWorkspaces::retire_unmerged`]). The orchestrator calls this once
    /// no open contract names the worktree.
    ///
    /// A conflict kept by an earlier process, or by an earlier run of this one
    /// (a run stopped before its resolver settled, then resumed), is settled
    /// by the worktree's name the same way: under `Delete` the worktree is
    /// reopened from the handle `resolver` recorded for it and removed (a
    /// worktree whose lock names a live owner, this process included, is not
    /// removed by name alone), or removed by name when its owner is gone. A
    /// `workspace` outside this run's base, or with nothing left, is a no-op.
    /// A failure is reported once.
    async fn discard_conflict(&self, resolver: &OpenContract, workspace: &Path) {
        let kept = self
            .conflicted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(workspace);
        if let Some(attempt) = kept {
            self.retire_unmerged(attempt).await;
            return;
        }
        let Some(name) = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|_| workspace.parent() == Some(self.manager.base()))
            .map(str::to_owned)
        else {
            return;
        };
        let deletes = self.retention.deletes();
        // Reopened before the refs go: a reopen needs the attempt ref that
        // names the worktree's HEAD.
        let mut reopen_failed = None;
        if deletes && let Err(e) = self.reopen_recorded(resolver, &name, false).await {
            reopen_failed = Some(e.to_string());
        }
        let held = self.take_live(&name);
        let manager = self.manager.clone();
        let merge_back = self.merge_back.clone();
        let failed = tokio::task::spawn_blocking(move || {
            let mut failed = Vec::new();
            if let Err(e) = merge_back.sweep_attempt_refs(std::slice::from_ref(&name)) {
                failed.push(format!(
                    "the attempt refs of the settled conflict {name} were not deleted: {e}"
                ));
            }
            if !deletes {
                return failed;
            }
            let removed = match held {
                Some(ws) => manager.remove(ws).map_err(|e| e.to_string()),
                None if manager.base().join(&name).exists() => {
                    manager.clean(&name).map(|_| ()).map_err(|e| e.to_string())
                }
                None => Ok(()),
            };
            if let Err(e) = removed {
                let reopen = reopen_failed
                    .map(|r| format!(" (it could not be reopened either: {r})"))
                    .unwrap_or_default();
                failed.push(format!(
                    "worktree {name} of a settled conflict could not be removed: {e}{reopen}; \
                     `kgraf clean` removes it"
                ));
            }
            failed
        })
        .await;
        match failed {
            Ok(texts) => texts.into_iter().for_each(|t| self.problem(t)),
            Err(e) => self.problem(format!("settling a conflict did not finish: {e}")),
        }
    }

    /// A fresh worktree of the integration tip for the bus to write in.
    async fn open_comms(&self) -> std::result::Result<Option<PathBuf>, String> {
        let manager = self.manager.clone();
        let merge_back = self.merge_back.clone();
        let ws = tokio::task::spawn_blocking(move || {
            let tip = merge_back.integration_tip().map_err(|e| e.to_string())?;
            let name = harness_worktree_name(COMMS_PREFIX);
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

    /// A conversation worktree holds no build and no work once its files are
    /// gone: removed under either policy. A failure is reported once.
    async fn discard_comms(&self, root: &Path) {
        let taken = self.live().remove(root);
        if let Some(ws) = taken {
            let manager = self.manager.clone();
            let shown = root.display().to_string();
            match tokio::task::spawn_blocking(move || manager.remove(ws)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => self.problem(format!(
                    "conversation worktree {shown} could not be removed: {e}; `kgraf clean` \
                     removes it"
                )),
                Err(e) => self.problem(format!(
                    "removing conversation worktree {shown} did not finish: {e}"
                )),
            }
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
            problems: self.problems.clone(),
        };
        let problems = self.problems.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let mut attempt = merge_back.attempt(ws).map_err(|e| e.to_string())?;
            // A conversation attempt that is not merged is discarded now (no
            // resolver for a conversation file): a failed discard is reported.
            let discard = |attempt: &mut Attempt| {
                if let Err(e) = merge_back.discard(attempt) {
                    problems.push(format!(
                        "conversation attempt {} could not be discarded: {e}",
                        attempt.workspace().root().display()
                    ));
                }
            };
            match merge_back.finish(&mut attempt, &message, &files) {
                Ok(FinishOutcome::Merged { merge_commit, .. }) => Ok(Integration::Merged {
                    commit: merge_commit,
                }),
                Ok(FinishOutcome::Empty) => Ok(Integration::Empty),
                Ok(FinishOutcome::Conflicted(c)) => {
                    // No resolver for a conversation file (0073 is for a
                    // worker's attempt): discarded immediately, as before.
                    discard(&mut attempt);
                    Ok(Integration::Conflict {
                        tip: c.integration_tip.clone(),
                        paths: c.paths.iter().map(|p| p.path.clone()).collect(),
                        detail: "conversation file conflict".into(),
                        commit: None,
                    })
                }
                Ok(FinishOutcome::Refused(r)) => {
                    discard(&mut attempt);
                    Ok(Integration::Refused {
                        detail: format!("{:?}", r.reasons),
                    })
                }
                Err(e) => {
                    discard(&mut attempt);
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

    /// The run's [`IntegrationView`], made at the tip on the first call and
    /// moved to the current tip on each later one. A view git could not move
    /// is dropped (removed under `Delete`) and made again at the tip; a view
    /// that cannot be made is an error, never an older tree passed off as
    /// the tip.
    async fn integration_view(&self) -> std::result::Result<Option<PathBuf>, String> {
        let mut slot = self.view.lock().await;
        let current = slot.take();
        let manager = self.manager.clone();
        let merge_back = self.merge_back.clone();
        let problems = self.problems.clone();
        let remove = self.retention.deletes();
        let (view, outcome) = tokio::task::spawn_blocking(move || {
            let tip = match merge_back.integration_tip() {
                Ok(tip) => tip,
                Err(e) => return (current, Err(e.to_string())),
            };
            let moved = match current {
                Some(v) if v.head == tip => return (Some(v), Ok(())),
                Some(mut v) => match v.advance(&tip) {
                    Ok(()) => Some(v),
                    Err(e) => {
                        problems.push(format!(
                            "the integration view could not be moved to {tip}, so it is \
                             made again: {e}"
                        ));
                        None
                    }
                },
                None => None,
            };
            if let Some(v) = moved {
                return (Some(v), Ok(()));
            }
            let name = harness_worktree_name(VIEW_PREFIX);
            match manager.create(&WorkspaceSpec {
                name: &name,
                base: &tip,
                branch: None,
            }) {
                Ok(ws) => (
                    Some(IntegrationView {
                        manager: manager.clone(),
                        ws: Some(ws),
                        head: tip,
                        remove,
                        problems,
                    }),
                    Ok(()),
                ),
                Err(e) => (None, Err(e.to_string())),
            }
        })
        .await
        .map_err(|e| e.to_string())?;
        *slot = view;
        outcome?;
        Ok(slot.as_ref().and_then(IntegrationView::root))
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
        let git_files = self.bypass_git_files();
        tokio::task::spawn_blocking(move || {
            child::snapshot(
                &repo,
                &ws,
                "kgraf: speculative snapshot (never on a branch)",
                git_files,
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
        let git_files = self.bypass_git_files();
        let ws = tokio::task::spawn_blocking(move || {
            child::discard(&manager, stale, &name).map_err(PrepareFailure::Unavailable)?;
            child::prepare_fresh(&manager, parent_ws.as_ref(), &name, git_files)
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

    /// A child whose worktree cannot be reopened, but whose result was
    /// recorded before the process stopped: merged again from that result
    /// (idempotent), so its report says what is in the parent's worktree.
    async fn recover_child_merge(
        &self,
        parent: &Path,
        child: &kgraf_core::AgentId,
        writes: &[String],
    ) -> std::result::Result<Option<ChildMerge>, String> {
        let Some(parent_ws) = self.get(parent) else {
            return Ok(None);
        };
        let manager = self.manager.clone();
        let name = child.to_string();
        let writes = writes.to_vec();
        let git_files = self.bypass_git_files();
        tokio::task::spawn_blocking(move || {
            if !child::has_result(manager.repo(), &name) {
                return Ok(None);
            }
            // No live worktree here, so nothing for the policy to keep.
            child::merge(&manager, &parent_ws, None, &name, &writes, false, git_files).map(Some)
        })
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
        // Decision 0088: under `Keep` a merged child's worktree stays with its
        // builds, like a merged attempt's.
        let keep = !self.retention.deletes();
        let git_files = self.bypass_git_files();
        let merged = tokio::task::spawn_blocking(move || {
            child::merge(
                &manager,
                &parent_ws,
                child_ws.as_ref(),
                &name,
                &writes,
                keep,
                git_files,
            )
        })
        .await
        .map_err(|e| e.to_string())??;
        if matches!(merged, ChildMerge::Merged { .. } | ChildMerge::Empty { .. }) {
            // Merged: the child's worktree is spent (`child::merge` removed
            // it, or kept it for the user). Anything else keeps both
            // worktrees as they are.
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
    use std::process::Stdio;

    /// The ref names of one child.
    const REFS: [&str; 3] = ["checkpoint", "handle", "result"];

    /// Where a child's refs live.
    pub(crate) const REF_ROOT: &str = "refs/kgraf/subagents/";

    pub(crate) fn ref_of(child: &str, what: &str) -> String {
        format!("{REF_ROOT}{child}/{what}")
    }

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

    /// One git command through the host runner (`kgraf_core::host_git`). `tree` is set for a command
    /// that reads a worktree (with `--attr-source` pinned to its trusted
    /// tree); without it, the command runs in the shared repository. `index`
    /// replaces the worktree's own index.
    pub(crate) fn git(
        repo: &Path,
        tree: Option<Wt<'_>>,
        index: Option<&Path>,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        // Shared repository lock for this one process (decision 0094): the
        // view's checkout and the child refs never move while a worker's
        // Bypass git window is open on the repository. Taken before the
        // command is built, so the host runner's config listing and the
        // command it prepares run in the same hold and a Bypass write to the
        // config cannot fall between them.
        let _repo = kgraf_sandbox::repo_lock::shared_for(repo, &format!("kGraf git {args:?}"))
            .map_err(|e| e.to_string())?;
        let mut pre: Vec<OsString> = Vec::new();
        if let Some(t) = tree {
            let mut git_dir = OsString::from("--git-dir=");
            git_dir.push(t.admin);
            let mut work_tree = OsString::from("--work-tree=");
            work_tree.push(t.work);
            pre.extend([
                git_dir,
                work_tree,
                OsString::from(format!("--attr-source={}", t.attr)),
            ]);
        }
        // A worktree command runs in its work tree, so pathspecs and
        // `add -A` resolve against it rather than the shared repository.
        let inv = kgraf_core::host_git::Invocation {
            exe: Path::new("git"),
            cwd: Some(tree.map_or(repo, |t| t.work)),
            pre: &pre,
        };
        // The host runner removes the inherited `GIT_*` environment (the
        // redirecting variables among it) and turns hooks, every configured
        // driver, the file system monitor and transports off.
        let mut cmd =
            kgraf_core::host_git::command(&inv, args).map_err(|e| format!("git {args:?}: {e}"))?;
        cmd.env("GIT_AUTHOR_NAME", "kgraf")
            .env("GIT_AUTHOR_EMAIL", "kgraf@localhost.invalid")
            .env("GIT_COMMITTER_NAME", "kgraf")
            .env("GIT_COMMITTER_EMAIL", "kgraf@localhost.invalid");
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.stdin(if stdin.is_some() {
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
    pub(crate) fn admin_of(ws: &Workspace) -> Result<PathBuf, String> {
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
    /// refuses the same attempt; with `bypass_git_files` (the Bypass mode,
    /// decision 0094) `.gitattributes`, `.gitmodules` and `.githooks` pass as
    /// plain data, as merge-back lets them. Used for a subagent's checkpoint
    /// and result (0067) and for speculative merge (0074).
    pub(crate) fn snapshot(
        repo: &Path,
        ws: &Workspace,
        message: &str,
        bypass_git_files: bool,
    ) -> Result<String, String> {
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
                .filter(|p| {
                    !(bypass_git_files && kgraf_core::paths::is_bypass_git_file(Path::new(p)))
                })
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
        bypass_git_files: bool,
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
            bypass_git_files,
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
    /// applied again. Once merged, the child's worktree is removed with its
    /// build output, unless `keep_worktree` (decision 0088's `Keep`).
    pub(crate) fn merge(
        manager: &WorkspaceManager,
        parent: &Workspace,
        child_ws: Option<&Workspace>,
        child: &str,
        writes: &[String],
        keep_worktree: bool,
        bypass_git_files: bool,
    ) -> Result<ChildMerge, String> {
        let repo = manager.repo();
        let checkpoint = resolve(repo, &ref_of(child, "checkpoint"))
            .ok_or_else(|| format!("subagent `{child}` has no recorded checkpoint"))?;
        let result_ref = ref_of(child, "result");
        let result = match (resolve(repo, &result_ref), child_ws) {
            (Some(recorded), _) => recorded,
            (None, Some(ws)) => {
                let result = snapshot(
                    repo,
                    ws,
                    &format!("kgraf: result of subagent {child}"),
                    bypass_git_files,
                )?;
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
        // The work is in the parent's worktree now: a worktree that cannot
        // be removed is a warning on the result, never an error that would
        // tell the parent nothing was applied. Startup cleanup removes it.
        let warning = match child_ws {
            Some(ws) if !keep_worktree => manager.remove(ws.clone()).err().map(|e| {
                format!("subagent `{child}` merged, but its worktree could not be removed: {e}")
            }),
            _ => None,
        };
        // The handle goes, kept worktree or not: nothing may reopen a spent
        // child as live work (a kept one stays only for the user to inspect).
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
            ChildMerge::Empty {
                not_applied,
                warning,
            }
        } else {
            ChildMerge::Merged {
                commit: result,
                not_applied,
                warning,
            }
        })
    }

    /// Whether `child` left a result that was merged, or can be merged again:
    /// its `result` ref, recorded before any file is applied. For a child
    /// whose worktree is gone (a kill after the merge removed it and before
    /// the child was marked settled), [`merge`] with no live worktree
    /// re-applies that result, finding every path already applied.
    pub(crate) fn has_result(repo: &Path, child: &str) -> bool {
        resolve(repo, &ref_of(child, "result")).is_some()
    }

    /// Every child ref, as `<refname> <oid>` lines.
    pub(crate) fn list_refs(repo: &Path) -> Result<String, String> {
        git_str(
            repo,
            None,
            None,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                REF_ROOT,
            ],
        )
    }

    /// Delete the `listed` refs ([`list_refs`]) of every child not in `keep`,
    /// each only if it still names the listed object. Returns the refs
    /// deleted.
    pub(crate) fn sweep(
        repo: &Path,
        listed: String,
        keep: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<String>, String> {
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

/// Removes a worktree when dropped, unless merge-back already has. A
/// removal that fails is reported once.
struct RemoveOnDrop {
    manager: Arc<WorkspaceManager>,
    ws: Option<Workspace>,
    problems: Problems,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(ws) = self.ws.take()
            && ws.root().exists()
        {
            let root = ws.root().display().to_string();
            if let Err(e) = self.manager.remove(ws) {
                self.problems.push(format!(
                    "conversation worktree {root} could not be removed: {e}; `kgraf clean` \
                     removes it"
                ));
            }
        }
    }
}

/// What `integrate` hands back from merge-back's blocking call besides the
/// outcome.
enum Back {
    /// Nothing is left to settle.
    Settled,
    /// A conflict, kept for its resolver (0073).
    Conflicted(Attempt),
    /// Merge-back refused the merge: the attempt is settled by `release`.
    Refused(Attempt),
    /// Merged, with a failure to report.
    Problem(String),
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
            warning: None,
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
            false,
            false,
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
            f.ws.sweep_refs(Some(&open), || Ok(Default::default()))
                .expect("sweep")
        };
        assert!(swept_open.iter().all(|r| !r.contains("subagents")));
        assert!(!refs_under(&f.root, child::REF_ROOT).is_empty());
        record.open_contracts.clear();
        let swept =
            f.ws.sweep_refs(Some(&record), || Ok(Default::default()))
                .expect("sweep");
        assert!(
            swept.iter().any(|r| r.contains("r1-s-r0-a0.s1")),
            "{swept:?}"
        );
        assert!(
            refs_under(&f.root, child::REF_ROOT).is_empty(),
            "no subagent refs remain"
        );
    }

    /// Two runs in one repository: the end of one run sweeps its own
    /// children's refs and keeps those of a child another run's record
    /// still names.
    #[tokio::test]
    async fn a_sweep_keeps_the_refs_of_another_runs_child() {
        let f = fixture();
        let p1 = parent(&f, &f.ws, "r1-s-r0-a0");
        let p2 = parent(&f, &f.ws, "r2-s-r0-a0");
        f.ws.prepare_child(&p1, &id("r1-s-r0-a0.s1"))
            .await
            .expect("child of r1");
        f.ws.prepare_child(&p2, &id("r2-s-r0-a0.s1"))
            .await
            .expect("child of r2");
        let record = kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "x", 0);
        let swept =
            f.ws.sweep_refs(Some(&record), || {
                Ok(["r2-s-r0-a0.s1".to_owned()].into_iter().collect())
            })
            .expect("sweep");
        assert!(
            swept.iter().any(|r| r.contains("r1-s-r0-a0.s1")),
            "r1's own child is swept: {swept:?}"
        );
        assert!(
            swept.iter().all(|r| !r.contains("r2-s-r0-a0.s1")),
            "{swept:?}"
        );
        let left = refs_under(&f.root, child::REF_ROOT);
        assert!(
            !left.is_empty() && left.iter().all(|r| r.contains("r2-s-r0-a0.s1")),
            "only the other run's child keeps its refs: {left:?}"
        );
    }

    /// A kill after the merge removed the child's worktree and before the
    /// child was marked settled: the worktree cannot be reopened, but the
    /// recorded result is merged again (every path already applied), so the
    /// report says Merged. A child that recorded no result has nothing to
    /// recover.
    #[tokio::test]
    async fn a_child_merged_before_a_kill_is_recovered_from_its_result() {
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-m-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-m-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("m.txt"), "m\n").expect("write");
        let merged =
            f.ws.merge_child(&p, &c, &w(&["m.txt"]))
                .await
                .expect("merge");
        assert!(matches!(merged, ChildMerge::Merged { .. }), "{merged:?}");
        assert!(!c.exists(), "the merge removed the child's worktree");
        assert!(
            f.ws.reopen_child(&p, &id("r1-m-r0-a0.s1")).await.is_err(),
            "what a resumed process finds: nothing to reopen"
        );
        let recovered =
            f.ws.recover_child_merge(&p, &id("r1-m-r0-a0.s1"), &w(&["m.txt"]))
                .await
                .expect("recover");
        assert!(
            matches!(recovered, Some(ChildMerge::Merged { .. })),
            "{recovered:?}"
        );
        assert_eq!(read(&p.join("m.txt")), "m\n", "the landed file is kept");

        f.ws.prepare_child(&p, &id("r1-m-r0-a0.s2"))
            .await
            .expect("second child");
        let none =
            f.ws.recover_child_merge(&p, &id("r1-m-r0-a0.s2"), &w(&["n.txt"]))
                .await
                .expect("recover");
        assert_eq!(none, None, "no recorded result, nothing recovered");
    }

    /// The child's files are applied, then its worktree cannot be removed:
    /// the merge is reported as merged with a warning, never as an error
    /// that would tell the parent nothing was applied.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_worktree_that_cannot_be_removed_after_its_merge_is_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        // The removal is made to fail with a read-only folder, and root
        // ignores file modes, so as root the worktree is removed and there is
        // nothing to warn about.
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!(
                "skipped: running as root, which ignores the read-only folder this test \
                 relies on to make the removal fail"
            );
            return;
        }
        let f = fixture();
        let p = parent(&f, &f.ws, "r1-q-r0-a0");
        let c =
            f.ws.prepare_child(&p, &id("r1-q-r0-a0.s1"))
                .await
                .expect("child");
        std::fs::write(c.join("q.txt"), "q\n").expect("write");
        let stuck = c.join("stuck");
        std::fs::create_dir_all(&stuck).expect("mkdir");
        std::fs::write(stuck.join("f"), "f\n").expect("write");
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let merged = f.ws.merge_child(&p, &c, &w(&["q.txt"])).await;
        // Restored first, so the temporary directory can be removed.
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let merged = merged.expect("the applied files are reported, not an error");
        match merged {
            ChildMerge::Merged {
                warning: Some(w), ..
            } => assert!(w.contains("could not be removed"), "{w}"),
            other => panic!("expected a merge with a warning, got {other:?}"),
        }
        assert_eq!(read(&p.join("q.txt")), "q\n", "the child's work landed");
    }

    /// Decision 0088: a merged writer subagent's worktree, with what it built,
    /// goes under `Delete` and stays under `Keep`, where it can never be
    /// reopened as live work.
    #[tokio::test]
    async fn a_merged_childs_worktree_and_build_follow_the_policy() {
        for (retention, stays) in [
            (WorktreeRetention::Delete, false),
            (WorktreeRetention::Keep, true),
        ] {
            let f = fixture();
            let ws = Arc::new(
                GitWorkspaces::new(f.ws.manager.clone(), f.ws.merge_back.clone())
                    .with_retention(retention),
            );
            let p = parent(&f, &ws, "r1-k-r0-a0");
            let c = ws
                .prepare_child(&p, &id("r1-k-r0-a0.s1"))
                .await
                .expect("child");
            std::fs::write(c.join("new.txt"), "child\n").expect("write");
            let build = c.join("target").join("blob");
            std::fs::create_dir_all(build.parent().expect("parent")).expect("mkdir");
            std::fs::write(&build, "build").expect("write");
            let merged = ws
                .merge_child(&p, &c, &w(&["new.txt"]))
                .await
                .expect("merge");
            assert!(
                matches!(merged, ChildMerge::Merged { warning: None, .. }),
                "{merged:?}"
            );
            assert_eq!(read(&p.join("new.txt")), "child\n", "{retention:?}");
            assert_eq!(c.exists(), stays, "{retention:?}");
            assert_eq!(build.exists(), stays, "{retention:?}");
            if stays {
                assert!(
                    ws.reopen_child(&p, &id("r1-k-r0-a0.s1")).await.is_err(),
                    "a spent child is never reopened as live work"
                );
            }
        }
    }
}
