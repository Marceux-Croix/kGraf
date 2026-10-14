//! The one land function (decision 0100, section 5): `kgraf land`, the box's
//! `/land`, and the plan root node's land action all call [`land`] (or its
//! two halves, [`prepare`] and [`complete`], when a live loop runs the gate
//! in the background).
//!
//! It runs through the run's live loop when there is one (the loop is then
//! the only writer of the run record and of `kgraf/<run>`), or under the
//! run's lease when there is none; the caller provides the context
//! ([`LandContext`]) for either. Git is behind [`LandGit`], implemented over
//! the real repository in `kgraf-host`.
//!
//! The steps, as a state machine:
//!
//! ```text
//! (journal open?) --yes--> Resume ---------------------------------------+
//!       | no                                                              |
//!   prepare: target (start branch or --into) -> busy check -> candidate   |
//!       -> stub rule -> program check -> trial merge                      |
//!       -> [gate on the merge result, in the background when looped]      |
//!   complete: target unchanged? -> gate evidence (passed / untested /     |
//!       remediation) -> program re-check -> build merge commit (signed)   |
//!       -> write journal (before any ref moves)                           |
//!       -> move target (ff-only in its worktree, or CAS) <----------------+
//!       -> fold into kgraf/<run> (idempotent) -> close journal
//! ```
//!
//! Every land is exactly one merge commit with the target as first parent.
//! There is no squash. Nothing lands without the user's explicit yes: the
//! caller only calls this after one (a typed `kgraf land`, `/land`, or an
//! accepted suggestion). An untested land needs a second yes after a loud
//! warning ([`LandOutcome::NeedsUntestedYes`]), bound to the candidate and
//! the target tip it was given for; a land that would run hooks, filters or
//! signing programs that changed since the run started needs a yes naming
//! the changes ([`LandOutcome::NeedsProgramsYes`]).

use crate::gates::{GateVerdict, ROOT_GATE};
use crate::land_record::{
    Ancestry, LandBook, LandGate, LandingEntry, NO_PROGRAM_SNAPSHOT, ProgramSnapshot, land_message,
    newest_passed, stub_blockers,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// What an untested yes was given for. A yes given for another candidate,
/// target or target tip is no yes: the warning is shown again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UntestedBinding {
    pub candidate: String,
    pub target: String,
    pub target_tip: String,
}

/// What the user asked to land.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandRequest {
    /// The target branch; `None`: the branch the run started from.
    pub into: Option<String>,
    /// The commit of `kgraf/<run>` to land; `None`: the newest commit the
    /// root gate passed on.
    pub candidate: Option<String>,
    /// The user said yes to landing with no passed gate.
    pub untested_confirmed: bool,
    /// What that yes was given for (from [`LandOutcome::NeedsUntestedYes`]);
    /// a yes counts only with it, so the warning always comes first.
    pub untested_for: Option<UntestedBinding>,
    /// The program changes the user confirmed, exactly as
    /// [`LandOutcome::NeedsProgramsYes`] listed them (with their hashes),
    /// for the `candidate` and `into` it named.
    pub programs_confirmed: Vec<String>,
    /// Someone can answer. An unattended land never lands untested.
    pub attended: bool,
}

/// Where the target branch is checked out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetLocation {
    /// The repository's main working tree.
    MainCheckout(PathBuf),
    /// A linked worktree (`git worktree add`).
    LinkedWorktree(PathBuf),
    /// No worktree has it checked out.
    NotCheckedOut,
}

impl TargetLocation {
    pub fn worktree(&self) -> Option<&PathBuf> {
        match self {
            TargetLocation::MainCheckout(p) | TargetLocation::LinkedWorktree(p) => Some(p),
            TargetLocation::NotCheckedOut => None,
        }
    }
}

/// The target branch as found at land time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetInfo {
    pub name: String,
    pub tip: String,
    pub location: TargetLocation,
}

/// A merge built in the object database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Built {
    Merge { commit: String, tree: String },
    Conflict { paths: Vec<String> },
}

/// What folding a commit into `kgraf/<run>` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FoldResult {
    AlreadyContains,
    FastForwarded,
    Merged { commit: String },
    Conflicted { paths: Vec<String> },
}

/// Why a land did not happen. Nothing moved unless the variant says so.
#[derive(Debug, thiserror::Error)]
pub enum LandError {
    #[error("{0}")]
    Git(String),
    #[error(
        "the checkout is on a detached HEAD, so there is no branch to land into; \
         check out a branch or name one with --into"
    )]
    DetachedHead,
    #[error("branch {branch} has no commit yet, so there is nothing to merge into")]
    UnbornTarget { branch: String },
    #[error(
        "the run started from {start}, but {checked_out} is checked out now; name the \
         branch to land into with --into"
    )]
    TargetMismatch { start: String, checked_out: String },
    #[error(
        "this run did not record the branch it started from; name the branch to land into \
         with --into"
    )]
    NoTarget,
    #[error("{0}")]
    NoSuchBranch(String),
    #[error("cannot land into {target}: {what}; finish or abort it first")]
    TargetBusy { target: String, what: String },
    #[error("cannot land: {0}")]
    GitTooOld(String),
    #[error("commit signing failed; nothing was committed and no branch moved: {0}")]
    Signing(String),
    #[error("cannot land into {target} itself: it is the run's own branch")]
    TargetIsRunBranch { target: String },
    #[error("commit {commit} is not on {run_branch}")]
    NotOnRunBranch { commit: String, run_branch: String },
    #[error("cannot land: {}", .0.join("; "))]
    StubsHeld(Vec<String>),
    #[error(
        "nobody can answer, and kGraf never lands untested unattended: root gate {gate} did \
         not pass: {reason}"
    )]
    UnattendedUntested { gate: String, reason: String },
    #[error(
        "{target} moved from {expected} to {found} while landing; nothing was changed. \
         Land again to merge onto its new tip"
    )]
    TargetMoved {
        target: String,
        expected: String,
        found: String,
    },
    #[error(
        "git refused to update {target} in {worktree}; nothing was changed. Git said: {stderr}"
    )]
    TargetRefused {
        target: String,
        worktree: PathBuf,
        stderr: String,
    },
    #[error("a land of this run is already in progress (into {target}); wait for it to finish")]
    LandPending { target: String },
    /// Git was stopped while it updated the target's worktree. Files may be
    /// partly written; the journal keeps the land open.
    #[error(
        "git was stopped while updating {target} in {worktree} ({detail}); files there may be \
         partly written. {recovery}"
    )]
    CheckoutInterrupted {
        target: String,
        worktree: PathBuf,
        detail: String,
        recovery: String,
    },
    #[error("cannot record the landing: {0}")]
    Persist(String),
    /// The run cannot be landed now (no such run, or its lease is held).
    #[error("{0}")]
    Run(String),
}

/// How a land ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandOutcome {
    /// The target moved to the merge commit. `notices` are lines for the
    /// user: a resumed land, an error git reported after moving, a fold
    /// conflict.
    Landed {
        entry: LandingEntry,
        notices: Vec<String>,
    },
    /// The candidate is already on the target.
    NothingToLand { candidate: String, target: String },
    /// The candidate does not merge cleanly onto the target (or the target
    /// into the run branch); nothing landed. A resolver merges the target
    /// into `kgraf/<run>`, then land again.
    Conflict { target: String, paths: Vec<String> },
    /// No gate passed on what would land. The loud warning: nothing moved.
    /// A yes bound to `candidate`, `target` and `target_tip` lands it
    /// untested.
    NeedsUntestedYes {
        gate: String,
        reason: String,
        candidate: String,
        target: String,
        target_tip: String,
        /// Commits of the candidate not on the target.
        commits: usize,
    },
    /// Landing would run hooks, filters or signing programs that changed
    /// since the run started (a worker could have written them). Nothing
    /// moved; a yes naming exactly `changes` lands it.
    NeedsProgramsYes {
        changes: Vec<String>,
        candidate: String,
        target: String,
        commits: usize,
    },
    /// The gate failed on the merge result while the run's loop is live: the
    /// target was merged into `kgraf/<run>`, a remediation contract is open
    /// on it, and the land waits for it.
    Remediating { contract: String, target: String },
}

/// Git operations of a land, over one repository and one run branch.
pub trait LandGit: Send + Sync {
    /// The run's integration branch (`kgraf/<run>`).
    fn run_branch(&self) -> &str;
    fn run_tip(&self) -> Result<String, LandError>;
    /// The branch checked out in the main checkout; `None` on a detached
    /// HEAD.
    fn checked_out_branch(&self) -> Result<Option<String>, LandError>;
    /// The branch named exactly `name` (no case folding), its tip and where
    /// it is checked out.
    fn resolve_target(&self, name: &str) -> Result<TargetInfo, LandError>;
    /// `Some(what)` when a worktree has a rebase or bisect in progress on
    /// `target`.
    fn target_busy(&self, target: &str) -> Result<Option<String>, LandError>;
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, LandError>;
    fn tree_of(&self, commit: &str) -> Result<String, LandError>;
    /// Commits reachable from `tip` and not from `base`.
    fn count_commits(&self, base: &str, tip: &str) -> Result<usize, LandError>;
    /// The hooks and program-valued settings the user's own git would run,
    /// read in `worktree` (the target's checkout) or the repository.
    fn programs(&self, worktree: Option<&std::path::Path>) -> Result<ProgramSnapshot, LandError>;
    /// For a checked-out target: config files under its work tree that the
    /// user's own git reads (included files, `.lfsconfig`) which checking out
    /// `merge_commit` would change, each named with its blob. They take
    /// effect in the user's git after the land, so they are asked about.
    fn post_land_config_changes(
        &self,
        target: &TargetInfo,
        merge_commit: &str,
    ) -> Result<Vec<String>, LandError>;
    /// Whether `worktree` has no change to tracked files and no index lock.
    fn worktree_clean(&self, worktree: &std::path::Path) -> Result<bool, LandError>;
    /// How to recover `worktree` after a stopped checkout of `entry`.
    fn checkout_recovery(&self, worktree: &std::path::Path, entry: &LandingEntry) -> String;
    /// Whether the merge commit will be signed (the user's `commit.gpgSign`).
    fn signing_on(&self) -> Result<bool, LandError>;
    /// The merge of `candidate` onto `target_tip`, target first parent.
    /// `signed`: sign as the user's configuration says (the commit that
    /// lands); unsigned for a commit only a gate runs on.
    fn build(
        &self,
        target_tip: &str,
        candidate: &str,
        message: &str,
        signed: bool,
    ) -> Result<Built, LandError>;
    /// Move the target from `entry.target_old` to `entry.merge_commit`. A
    /// checked-out target moves with the user's own git, enforcing
    /// `programs` (run-start hooks copy and program settings; `None`: a run
    /// from before the snapshot, which runs no hooks), after checking under
    /// one repository lock that the post-land config changes are still
    /// `entry.include_changes`.
    fn move_target(
        &self,
        entry: &LandingEntry,
        location: &TargetLocation,
        programs: Option<&ProgramSnapshot>,
    ) -> Result<(), LandError>;
    /// Merge `commit` into the run branch; never backward.
    fn fold(&self, commit: &str) -> Result<FoldResult, LandError>;
}

/// The run-side state of a land: the record and, with a live loop, the
/// loop's own remediation path.
#[async_trait::async_trait]
pub trait LandContext: Send {
    fn book(&mut self) -> &mut LandBook;
    fn run_id(&self) -> String;
    /// The verified contracts, for the message.
    fn contracts(&self) -> Vec<String>;
    /// Interfaces whose stub the run is known to have landed.
    fn known_stubs(&self) -> BTreeSet<String>;
    fn now_ms(&self) -> i64;
    /// Save the record. Called before any ref moves and after each step.
    fn persist(&mut self) -> Result<(), String>;
    /// Whether the run's loop is live (a failed merge-result gate then opens
    /// a remediation contract).
    fn loop_live(&self) -> bool;
    /// Run the root gate's commands on `commit`. `None`: no gate runner.
    /// Called by [`land`]; a live loop runs the gate itself, in the
    /// background, and calls [`complete`] with its verdict.
    async fn run_merge_gate(&mut self, commit: &str) -> Option<Result<GateVerdict, String>>;
    /// With a live loop: open a remediation contract for a gate that failed,
    /// based on `commit` (the run branch with the target merged in); its id.
    fn open_remediation(&mut self, commit: &str, verdict: GateVerdict) -> Option<String>;
}

struct GitAncestry<'a>(&'a dyn LandGit);

impl Ancestry for GitAncestry<'_> {
    fn is_ancestor(&self, a: &str, d: &str) -> Result<bool, String> {
        self.0.is_ancestor(a, d).map_err(|e| e.to_string())
    }
}

/// A land waiting for its merge-result gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingLand {
    pub request: LandRequest,
    pub target: TargetInfo,
    pub candidate: String,
    pub commits: usize,
    /// The unsigned trial merge the gate runs on.
    pub trial_commit: String,
    pub merged_tree: String,
}

/// What [`prepare`] or [`complete`] left to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Done(LandOutcome),
    /// Run the root gate on `trial_commit`, then call [`complete`].
    Gate(PendingLand),
    /// The journal is written: move the target ([`move_phase`], the user's
    /// own git, which may run their hooks and take long), then call
    /// [`finish_phase`] with its result.
    Move {
        entry: LandingEntry,
        notices: Vec<String>,
    },
}

/// How the gate evidence was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// The candidate's own pass covers the merge (same tree).
    Passed,
    /// No gate passed on the candidate.
    NotPassed(String),
    /// The merge-result gate's verdict; `None`: no gate runner.
    Verdict(Option<Result<GateVerdict, String>>),
}

/// Land the run (see the module documentation): [`prepare`], the gate when
/// one is needed, [`complete`], then the move and the fold.
pub async fn land(
    git: &dyn LandGit,
    ctx: &mut dyn LandContext,
    req: &LandRequest,
) -> Result<LandOutcome, LandError> {
    let mut step = prepare(git, ctx, req)?;
    loop {
        step = match step {
            Step::Done(outcome) => return Ok(outcome),
            Step::Gate(pending) => {
                let verdict = ctx.run_merge_gate(&pending.trial_commit).await;
                complete(git, ctx, pending, Evidence::Verdict(verdict))?
            }
            Step::Move { entry, notices } => {
                let programs = ctx.book().programs.clone();
                let moved = move_phase(git, &entry, programs.as_ref());
                return finish_phase(git, ctx, entry, notices, moved);
            }
        };
    }
}

/// The first half of a land: everything up to the merge-result gate. A land
/// that needs no gate goes on to [`complete`].
pub fn prepare(
    git: &dyn LandGit,
    ctx: &mut dyn LandContext,
    req: &LandRequest,
) -> Result<Step, LandError> {
    if let Some(entry) = ctx.book().landing.clone() {
        if !entry.target_moved
            && let Some(what) = git.target_busy(&entry.target)?
        {
            return Err(LandError::TargetBusy {
                target: entry.target,
                what,
            });
        }
        return Ok(Step::Move {
            entry,
            notices: vec![
                "an interrupted land was completed; run land again for newer work".into(),
            ],
        });
    }
    let run_branch = git.run_branch().to_owned();
    let (name, from_start) = match (&req.into, ctx.book().start_branch.clone()) {
        (Some(name), _) => (name.clone(), false),
        (None, Some(start)) => (start.name, true),
        (None, None) => return Err(LandError::NoTarget),
    };
    // First: a rebase or bisect on the target also detaches its checkout.
    if let Some(what) = git.target_busy(&name)? {
        return Err(LandError::TargetBusy { target: name, what });
    }
    if from_start {
        let checked = git.checked_out_branch()?;
        if checked.as_deref() != Some(name.as_str()) {
            return Err(LandError::TargetMismatch {
                start: name,
                checked_out: checked
                    .map(|b| format!("branch {b}"))
                    .unwrap_or_else(|| "a detached HEAD".into()),
            });
        }
    }
    let target = git.resolve_target(&name)?;
    if target.name == run_branch {
        return Err(LandError::TargetIsRunBranch {
            target: target.name,
        });
    }
    let tip = git.run_tip()?;
    let on_branch = |sha: &str| git.is_ancestor(sha, &tip).map_err(|e| e.to_string());
    let (candidate, passed) = match &req.candidate {
        Some(c) => {
            if !git.is_ancestor(c, &tip)? {
                return Err(LandError::NotOnRunBranch {
                    commit: c.clone(),
                    run_branch,
                });
            }
            let passed = ctx.book().root_gate_passes.iter().any(|p| &p.sha == c);
            (c.clone(), passed)
        }
        None => match newest_passed(ctx.book(), &on_branch).map_err(LandError::Git)? {
            Some(c) => (c, true),
            None => (tip.clone(), false),
        },
    };
    if git.is_ancestor(&candidate, &target.tip)? {
        return Ok(Step::Done(LandOutcome::NothingToLand {
            candidate,
            target: target.name,
        }));
    }
    let known = ctx.known_stubs();
    let blockers =
        stub_blockers(ctx.book(), &known, &candidate, &GitAncestry(git)).map_err(LandError::Git)?;
    if !blockers.is_empty() {
        return Err(LandError::StubsHeld(blockers));
    }
    let commits = git.count_commits(&target.tip, &candidate)?;
    // A passed gate covers the merge only when the merge tree is the
    // candidate's own (the target had nothing the candidate lacks);
    // otherwise the root gate runs on the merge result first.
    let trial = git.build(
        &target.tip,
        &candidate,
        "kgraf: merge result under test (never on a branch)",
        false,
    )?;
    let (trial_commit, merged_tree) = match trial {
        Built::Conflict { paths } => {
            return Ok(Step::Done(LandOutcome::Conflict {
                target: target.name,
                paths,
            }));
        }
        Built::Merge { commit, tree } => (commit, tree),
    };
    let pending = PendingLand {
        request: req.clone(),
        target,
        candidate,
        commits,
        trial_commit,
        merged_tree,
    };
    if let Some(ask) = program_check(git, ctx, req, &pending)? {
        return Ok(Step::Done(ask));
    }
    if !passed {
        let reason = match &req.candidate {
            Some(c) => format!("it never passed on {c}"),
            None => format!("it has not passed on any commit of {run_branch}"),
        };
        return complete(git, ctx, pending, Evidence::NotPassed(reason));
    }
    if pending.merged_tree == git.tree_of(&pending.candidate)? {
        return complete(git, ctx, pending, Evidence::Passed);
    }
    Ok(Step::Gate(pending))
}

/// What this land needs a yes for, beyond what it enforces. Hooks and
/// program settings are enforced at their run-start values in the move
/// ([`LandGit::move_target`]); what cannot be enforced is asked about:
/// signing settings changed since the run started (signing runs in kGraf's
/// own git), config files in the work tree the merge changes (they act in
/// the user's own git later), and a run from before the snapshot. A yes
/// counts only for the same list, candidate and target.
fn program_check(
    git: &dyn LandGit,
    ctx: &mut dyn LandContext,
    req: &LandRequest,
    p: &PendingLand,
) -> Result<Option<LandOutcome>, LandError> {
    let changes = program_changes(git, ctx, p)?;
    let bound = req.programs_confirmed == changes
        && req.candidate.as_deref() == Some(p.candidate.as_str())
        && req.into.as_deref() == Some(p.target.name.as_str());
    if changes.is_empty() || bound {
        return Ok(None);
    }
    Ok(Some(LandOutcome::NeedsProgramsYes {
        changes,
        candidate: p.candidate.clone(),
        target: p.target.name.clone(),
        commits: p.commits,
    }))
}

fn program_changes(
    git: &dyn LandGit,
    ctx: &mut dyn LandContext,
    p: &PendingLand,
) -> Result<Vec<String>, LandError> {
    let worktree = p.target.location.worktree();
    let signing = git.signing_on()?;
    if worktree.is_none() && !signing {
        return Ok(Vec::new());
    }
    let mut changes = Vec::new();
    match ctx.book().programs.clone() {
        None => changes.push(NO_PROGRAM_SNAPSHOT.to_owned()),
        Some(then) if signing => {
            changes.extend(then.signing_changes(&git.programs(worktree.map(PathBuf::as_path))?))
        }
        Some(_) => {}
    }
    if worktree.is_some() {
        changes.extend(git.post_land_config_changes(&p.target, &p.trial_commit)?);
    }
    Ok(changes)
}

/// The untested path: the warning, or the gate a yes bound to this
/// candidate, target and target tip lands with.
fn untested(
    req: &LandRequest,
    p: &PendingLand,
    reason: String,
) -> Result<Result<LandGate, LandOutcome>, LandError> {
    if !req.attended {
        return Err(LandError::UnattendedUntested {
            gate: ROOT_GATE.into(),
            reason,
        });
    }
    // A yes is always given after the warning, for what the warning named.
    let bound = req.untested_for.as_ref().is_some_and(|b| {
        b.candidate == p.candidate && b.target == p.target.name && b.target_tip == p.target.tip
    });
    if req.untested_confirmed && bound {
        return Ok(Ok(LandGate::Untested {
            gate: ROOT_GATE.into(),
            reason,
        }));
    }
    Ok(Err(LandOutcome::NeedsUntestedYes {
        gate: ROOT_GATE.into(),
        reason,
        candidate: p.candidate.clone(),
        target: p.target.name.clone(),
        target_tip: p.target.tip.clone(),
        commits: p.commits,
    }))
}

/// The second half of a land: the gate evidence, the signed merge commit
/// and the journal. The target must still be where [`prepare`] found it,
/// with no rebase or bisect started on it; when it moved (during a gate),
/// nothing is changed. Returns [`Step::Move`] once the journal is written.
pub fn complete(
    git: &dyn LandGit,
    ctx: &mut dyn LandContext,
    p: PendingLand,
    evidence: Evidence,
) -> Result<Step, LandError> {
    if let Some(what) = git.target_busy(&p.target.name)? {
        return Err(LandError::TargetBusy {
            target: p.target.name.clone(),
            what,
        });
    }
    let now = git.resolve_target(&p.target.name)?;
    if now.tip != p.target.tip {
        return Err(LandError::TargetMoved {
            target: p.target.name.clone(),
            expected: p.target.tip.clone(),
            found: now.tip,
        });
    }
    let req = p.request.clone();
    let reason = match evidence {
        Evidence::Passed | Evidence::Verdict(Some(Ok(GateVerdict { passed: true, .. }))) => None,
        Evidence::Verdict(Some(Ok(v))) if ctx.loop_live() => {
            // The fix must see the code the gate failed on: the run branch
            // gets the target's commits first.
            if let FoldResult::Conflicted { paths } = git.fold(&p.target.tip)? {
                return Ok(Step::Done(LandOutcome::Conflict {
                    target: p.target.name.clone(),
                    paths,
                }));
            }
            let base = git.run_tip()?;
            let by = v.by.clone();
            if let Some(contract) = ctx.open_remediation(&base, v) {
                return Ok(Step::Done(LandOutcome::Remediating {
                    contract,
                    target: p.target.name.clone(),
                }));
            }
            Some(format!(
                "it failed on the merge result with {} running `{by}` and no more \
                 remediations are allowed",
                p.target.name
            ))
        }
        Evidence::NotPassed(r) => Some(r),
        Evidence::Verdict(None) => Some(format!(
            "{} gained commits since, and no gate runner can check the merge result",
            p.target.name
        )),
        Evidence::Verdict(Some(Ok(v))) => Some(format!(
            "it failed on the merge result with {} running `{}`",
            p.target.name, v.by
        )),
        Evidence::Verdict(Some(Err(e))) => Some(format!(
            "it could not run on the merge result with {}: {e}",
            p.target.name
        )),
    };
    let gate = match reason {
        None => LandGate::Passed {
            candidate: p.candidate.clone(),
            tree: p.merged_tree.clone(),
        },
        Some(reason) => match untested(&req, &p, reason)? {
            Ok(g) => g,
            Err(ask) => return Ok(Step::Done(ask)),
        },
    };
    let target = TargetInfo {
        location: now.location,
        ..p.target.clone()
    };
    let checked = PendingLand {
        target,
        ..p.clone()
    };
    if let Some(ask) = program_check(git, ctx, &req, &checked)? {
        return Ok(Step::Done(ask));
    }
    let run = ctx.run_id();
    let message = land_message(&run, &p.target.name, &ctx.contracts(), &gate);
    let merge_commit = match git.build(&p.target.tip, &p.candidate, &message, true)? {
        Built::Merge { commit, tree } => {
            // What lands is exactly what was gated and checked.
            if tree != p.merged_tree {
                return Err(LandError::Git(format!(
                    "the signed merge commit {commit} has tree {tree}, not the gated tree {}; \
                     nothing was moved",
                    p.merged_tree
                )));
            }
            commit
        }
        Built::Conflict { paths } => {
            return Ok(Step::Done(LandOutcome::Conflict {
                target: p.target.name.clone(),
                paths,
            }));
        }
    };
    let include_changes = if checked.target.location.worktree().is_some() {
        git.post_land_config_changes(&checked.target, &p.trial_commit)?
    } else {
        Vec::new()
    };
    let programs_digest = ctx.book().programs.as_ref().map(ProgramSnapshot::digest);
    let entry = LandingEntry {
        run_branch: git.run_branch().to_owned(),
        candidate: p.candidate.clone(),
        merge_commit,
        target: p.target.name.clone(),
        target_old: p.target.tip.clone(),
        gate,
        started_ms: ctx.now_ms(),
        target_moved: false,
        checkout_interrupted: false,
        include_changes,
        programs_digest,
        fold: None,
        finished_ms: None,
    };
    // The journal goes to disk before any ref moves.
    ctx.book().landing = Some(entry.clone());
    ctx.persist().map_err(LandError::Persist)?;
    Ok(Step::Move {
        entry,
        notices: Vec::new(),
    })
}

/// Whether `merge_commit` is on `tip`: the target moved to it, or the user
/// committed on top since.
fn landed_on(git: &dyn LandGit, merge_commit: &str, tip: &str) -> Result<bool, LandError> {
    Ok(tip == merge_commit || git.is_ancestor(merge_commit, tip)?)
}

/// What moving the target came to.
#[derive(Debug)]
pub enum Moved {
    /// The merge commit is on the target (perhaps with commits on top, or
    /// after an error git reported once the branch had moved: the notices).
    Yes(Vec<String>),
    /// Certain that the target did not move: the journal closes.
    NotMoved(LandError),
    /// The checkout was stopped part way: files may be partly written. The
    /// journal stays, marked, and says so again on resume.
    Interrupted(LandError),
    /// Uncertain (the target could not be read): the journal stays.
    Unknown(LandError),
}

/// Move the target from the journal entry, reading where it stands first
/// and again after any error. Git only, no record: a live loop runs it off
/// its task. Idempotent.
pub fn move_phase(
    git: &dyn LandGit,
    entry: &LandingEntry,
    programs: Option<&ProgramSnapshot>,
) -> Moved {
    if entry.target_moved {
        return Moved::Yes(Vec::new());
    }
    // The land runs with the snapshot it was confirmed with, or not at all.
    if programs.map(ProgramSnapshot::digest) != entry.programs_digest {
        return Moved::Unknown(LandError::Git(
            "the run's program snapshot is not the one this land was confirmed with; nothing \
             was moved"
                .into(),
        ));
    }
    let read = |git: &dyn LandGit| -> Result<(TargetInfo, bool), LandError> {
        let t = git.resolve_target(&entry.target)?;
        let on = landed_on(git, &entry.merge_commit, &t.tip)?;
        Ok((t, on))
    };
    let target = match read(git) {
        Ok((_, true)) => return Moved::Yes(Vec::new()),
        Ok((t, false)) => t,
        Err(e) => return Moved::Unknown(e),
    };
    if target.tip != entry.target_old {
        return Moved::NotMoved(LandError::TargetMoved {
            target: entry.target.clone(),
            expected: entry.target_old.clone(),
            found: target.tip,
        });
    }
    if entry.checkout_interrupted
        && let Some(wt) = target.location.worktree()
    {
        match git.worktree_clean(wt) {
            Ok(true) => {}
            Ok(false) => {
                return Moved::Interrupted(LandError::CheckoutInterrupted {
                    target: entry.target.clone(),
                    worktree: wt.clone(),
                    detail: "an earlier land was stopped while git updated this worktree, and \
                             it still has changes or a lock"
                        .into(),
                    recovery: git.checkout_recovery(wt, entry),
                });
            }
            Err(e) => return Moved::Unknown(e),
        }
    }
    if let Some(what) = git.target_busy(&entry.target).ok().flatten() {
        return Moved::NotMoved(LandError::TargetBusy {
            target: entry.target.clone(),
            what,
        });
    }
    match git.move_target(entry, &target.location, programs) {
        Ok(()) => Moved::Yes(Vec::new()),
        Err(e) => match read(git) {
            Ok((_, true)) => Moved::Yes(vec![format!(
                "{} moved to the merge commit, but git then reported: {e}",
                entry.target
            )]),
            Ok(_) if matches!(e, LandError::CheckoutInterrupted { .. }) => Moved::Interrupted(e),
            Ok((t, false)) if t.tip == entry.target_old => Moved::NotMoved(e),
            Ok((t, false)) => Moved::NotMoved(LandError::TargetMoved {
                target: entry.target.clone(),
                expected: entry.target_old.clone(),
                found: t.tip,
            }),
            Err(_) => Moved::Unknown(e),
        },
    }
}

/// After [`move_phase`]: record where the target stands, fold the merge
/// commit into the run branch, and close the journal. The journal is closed
/// without landing only when it is certain the target did not move.
pub fn finish_phase(
    git: &dyn LandGit,
    ctx: &mut dyn LandContext,
    mut entry: LandingEntry,
    mut notices: Vec<String>,
    moved: Moved,
) -> Result<LandOutcome, LandError> {
    match moved {
        Moved::Yes(more) => notices.extend(more),
        Moved::NotMoved(e) => {
            ctx.book().landing = None;
            ctx.persist().map_err(LandError::Persist)?;
            return Err(e);
        }
        Moved::Interrupted(e) => {
            entry.checkout_interrupted = true;
            ctx.book().landing = Some(entry);
            ctx.persist().map_err(LandError::Persist)?;
            return Err(e);
        }
        Moved::Unknown(e) => return Err(e),
    }
    if !entry.target_moved {
        entry.target_moved = true;
        entry.checkout_interrupted = false;
        ctx.book().landing = Some(entry.clone());
        ctx.persist().map_err(LandError::Persist)?;
    }
    let fold = git.fold(&entry.merge_commit)?;
    let described = match &fold {
        FoldResult::AlreadyContains => "already on the run branch".to_owned(),
        FoldResult::FastForwarded => "run branch fast-forwarded to the merge commit".to_owned(),
        FoldResult::Merged { commit } => {
            format!("merged into the run branch as {commit}, keeping later verified work")
        }
        FoldResult::Conflicted { paths } => {
            notices.push(format!(
                "{} landed; folding it back into {} conflicts on {}, so {} is unchanged; \
                 reopen the run to resolve it",
                entry.target,
                entry.run_branch,
                paths.join(", "),
                entry.run_branch
            ));
            format!("not folded: conflict on {}", paths.join(", "))
        }
    };
    entry.fold = Some(described);
    entry.finished_ms = Some(ctx.now_ms());
    let book = ctx.book();
    book.landing = None;
    book.landed.push(entry.clone());
    ctx.persist().map_err(LandError::Persist)?;
    Ok(LandOutcome::Landed { entry, notices })
}
