//! Stubs land on the integration branch first (decision 0081).
//!
//! Today an interface contract's checks (stub-compile, vacuity, handshake:
//! `checks.rs`) prove the consumers compile against a trivial stub of the
//! producer side, but nothing commits that stub anywhere: every worker's
//! worktree still branches from the plain integration tip, which does not yet
//! agree with the interface. A consumer written first builds against a brief,
//! not against code, until the producer happens to land -- exactly the
//! commitment-failure class decision 0023 measured at 32 percent of
//! concurrent multi-agent failures.
//!
//! [`StubLander::land`] closes that gap: once an interface's checks pass and
//! before any party of it is dispatched (`orchestrator/contract_hooks.rs`,
//! `RunLoop::contract_gate`), the stub the checks just proved compiles is
//! committed onto `kgraf/<run>` itself, one commit per interface version. So
//! every party's worktree, created after this point, starts from a tip that
//! already compiles against the interface.
//!
//! **Security (binding, from the owner's other session on `child::snapshot`,
//! `trial_merge` and `commit_landed`): reuse the merge-back path, not a new
//! git path.** [`GitStubLander`] never runs a bespoke git command; it writes
//! the stub's declared files into a throwaway worktree branched from the
//! integration tip and carries them through [`kgraf_sandbox::MergeBack`]
//! exactly as a worker's own attempt is carried (`worker/workspace.rs` in
//! `kgraf-host`, and `MergeBack::commit`/`merge` themselves): attributes
//! pinned to the integration tip's own tree (never a worker-writable
//! `.gitattributes`), hooks and fsmonitor off, the gitlink verified before
//! any git process touches the worktree, and a change to a protected path
//! (`.gitattributes`, `.gitmodules`, `.git*` directories, kGraf's own state)
//! refused loudly (`RefusalReason::ProtectedPath`) rather than
//! merged. No filter or merge driver a worker's tree could select ever runs,
//! because merge-back's `SafeGit` never reads attributes from a worktree.
//!
//! Written new: the mechanism composes existing merge-back
//! primitives (`MergeBack::attempt`, `finish`) rather than writing tree
//! objects with raw plumbing, so every protection merge-back already has
//! (and every future one) covers a stub commit too, with nothing to keep in
//! sync by hand. Searched: nothing found lands a *stub* -- as opposed to a
//! finished attempt -- through an existing merge tool; container-use
//! (Apache-2.0, already ported into `kgraf-sandbox::merge`) commits only a
//! worker's own finished environment.
//!
//! **Security hardening (2026-09-28, review of `544c335..9f3f05c`).** The
//! throwaway worktree is checked out at the integration tip, which can hold
//! symbolic links a worker committed (merge-back's prescan skips links, then
//! `add -A` records them). So before any byte is written: every component of
//! every stub path is inspected without following links and a link anywhere
//! on the way, or at the leaf, is refused naming it (H1); the write itself
//! goes through `kgraf_sandbox::write_confined`, the descriptor-relative,
//! no-follow writer the contract checks already use, never `root.join` plus
//! `std::fs::write`. A stub may only touch its producer's declared write
//! targets, and may replace a file that already exists at the tip only when
//! the contract declares it (`InterfaceContract::replaces`) or an earlier
//! version of the same interface landed it (M1). A tip that already holds
//! exactly the stub is recorded as landed at that tip, not refused as an
//! empty attempt (M2). Every worktree a refused or failed land leaves is kept
//! for inspection and removed by [`GitStubLander::sweep`] at the end of the
//! run (L1).

use crate::checks::{CheckStage, FileOverlay, InterfaceContract};
use kgraf_sandbox::{
    Attempt, FinishOutcome, MergeBack, MergeError, Workspace, WorkspaceManager, WorkspaceSpec,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What a stub may touch (0081 hardening, M1), decided by the orchestrator
/// from the run record and checked by the lander against the tip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StubScope {
    /// The producer party's declared write targets, repository-relative.
    /// Every stub path must be one of them.
    pub producer_writes: BTreeSet<String>,
    /// Stub paths an earlier version of this interface already landed
    /// (`ContractBook::landed_paths`): a revision may replace them.
    pub landed_before: BTreeSet<String>,
}

/// Lands one interface's stub onto the integration branch. `Arc<dyn _>` (not
/// `Option`): with contract checks on, landing is not a separate switch --
/// [`crate::challenge::ContractChecks`] gives one the same way it gives a
/// handshaker and a reviewer, and checks off never calls it at all
/// (`RunLoop::contract_gate` returns early when no desk is configured).
#[async_trait::async_trait]
pub trait StubLander: Send + Sync {
    /// Commit `ic.stub` onto the integration branch and return the merge
    /// commit, or the integration tip when it already holds exactly the stub
    /// (a resumed land after a crash between merge and record, or a revision
    /// whose stub is unchanged). The stub-compile check already proved these
    /// exact files compile against the consumers and the contract's test
    /// (`checks.rs`), so this call never re-runs a command; it only carries
    /// files that already passed a check into a commit, within `scope`.
    async fn land(&self, ic: &InterfaceContract, scope: &StubScope) -> Result<String, String>;
}

/// What a refused or failed land left behind, kept for inspection until
/// [`GitStubLander::sweep`] (never a silent cleanup of evidence).
enum Leftover {
    Workspace(Workspace),
    Attempt(Box<Attempt>),
}

impl Leftover {
    fn name(&self) -> &str {
        match self {
            Leftover::Workspace(ws) => ws.name(),
            Leftover::Attempt(a) => a.name(),
        }
    }
}

/// [`StubLander`] over a real repository, through [`MergeBack`] (0018).
pub struct GitStubLander {
    manager: Arc<WorkspaceManager>,
    merge_back: Arc<MergeBack>,
    /// Every stub worktree name this lander created, for the end-of-run
    /// attempt-ref sweep (L1).
    touched: Arc<Mutex<BTreeSet<String>>>,
    leftovers: Arc<Mutex<Vec<Leftover>>>,
}

/// How one stub path stands in the fresh worktree at the tip.
enum AtTip {
    Absent,
    /// A regular file; whether its bytes equal the stub's.
    File {
        same: bool,
    },
}

impl GitStubLander {
    pub fn new(manager: Arc<WorkspaceManager>, merge_back: Arc<MergeBack>) -> Self {
        Self {
            manager,
            merge_back,
            touched: Arc::new(Mutex::new(BTreeSet::new())),
            leftovers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A workspace name valid under `WorkspaceManager::create` (ASCII
    /// letters, digits, `-`, `_`, `.`), unique per interface version so a
    /// resumed run's retry after a crash between `create` and `finish` names
    /// the same worktree again rather than leaking one per attempt.
    fn workspace_name(ic: &InterfaceContract) -> String {
        let id: String = ic
            .id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("stub-{id}-v{}", ic.version)
    }

    /// At the end of a run (L1): discard every worktree and attempt ref a
    /// refused, conflicted or failed land kept for inspection, then sweep the
    /// attempt refs of every stub worktree name this lander created (a crash
    /// between commit and merge leaves one). Returns what was removed. Every
    /// failure is reported, none stops the others.
    pub fn sweep(&self) -> Result<Vec<String>, String> {
        let mut removed = Vec::new();
        let mut errors = Vec::new();
        let leftovers = std::mem::take(&mut *lock(&self.leftovers));
        for left in leftovers {
            match left {
                Leftover::Workspace(ws) => {
                    let root = ws.root().display().to_string();
                    match self.manager.remove(ws) {
                        Ok(()) => removed.push(root),
                        Err(e) => errors.push(format!("stub worktree {root}: {e}")),
                    }
                }
                Leftover::Attempt(mut attempt) => {
                    let root = attempt.workspace().root().display().to_string();
                    match self.merge_back.discard(&mut attempt) {
                        Ok(()) => removed.push(root),
                        Err(e) => errors.push(format!("stub attempt {root}: {e}")),
                    }
                }
            }
        }
        let names: Vec<String> = lock(&self.touched).iter().cloned().collect();
        match self.merge_back.sweep_attempt_refs(&names) {
            Ok(refs) => removed.extend(refs),
            Err(e) => errors.push(format!("stub attempt refs: {e}")),
        }
        if errors.is_empty() {
            Ok(removed)
        } else {
            Err(format!(
                "stub leftovers were not all removed: {}",
                errors.join("; ")
            ))
        }
    }

    /// Write `ic.stub` into a fresh worktree at the integration tip, then
    /// commit and merge it through [`MergeBack`]. Blocking (git and file
    /// I/O); the caller runs it off the async runtime.
    fn land_blocking(&self, ic: &InterfaceContract, scope: &StubScope) -> Result<String, String> {
        // Defense in depth: the artifact check ran `validate` before any
        // check; a revision reaching here unvalidated must not land.
        ic.validate()
            .map_err(|e| format!("the stub of {} is not a valid artifact: {e}", ic.label()))?;
        check_scope(ic, scope)?;
        let tip = self
            .merge_back
            .integration_tip()
            .map_err(|e| format!("cannot read the integration tip: {e}"))?;
        self.land_at(&tip, ic, scope)
    }

    /// As [`GitStubLander::land_blocking`], against a tip already read
    /// (`ic.validate` and `check_scope` already ran there). Split out so a
    /// test can place the branch-moved-between-create-and-merge race
    /// deterministically: give a tip that is already stale by the time
    /// [`MergeBack::finish`] reads the branch again, which yields a real
    /// [`kgraf_sandbox::FinishOutcome::Conflicted`] instead of one a test
    /// cannot schedule through the public, always-freshest-tip [`Self::land`].
    fn land_at(
        &self,
        tip: &str,
        ic: &InterfaceContract,
        scope: &StubScope,
    ) -> Result<String, String> {
        let tip = tip.to_owned();
        let name = Self::workspace_name(ic);
        lock(&self.touched).insert(name.clone());
        // A retry of the same version in this process: the worktree an
        // earlier refused land kept is this lander's own evidence of that
        // attempt, superseded by this one. Removed first, or `create` finds
        // it held by a live owner (this process) and refuses.
        self.discard_leftover(&name)?;
        let workspace = self
            .manager
            .create(&WorkspaceSpec {
                name: &name,
                base: &tip,
                branch: None,
            })
            .map_err(|e| format!("cannot create the stub worktree `{name}`: {e}"))?;
        let root = workspace.root().to_path_buf();
        let at_tip = match inspect_tip(&root, &ic.stub) {
            Ok(v) => v,
            Err(e) => return Err(self.keep(Leftover::Workspace(workspace), &name, &root, e)),
        };
        // M2: the tip already holds exactly this stub (a crash after the
        // merge but before `StubLanded` was recorded, or a revision whose stub
        // did not change). Landing it again would be an empty attempt; the
        // stub is on the branch, so the tip is the landed commit.
        if at_tip
            .iter()
            .all(|t| matches!(t, AtTip::File { same: true }))
        {
            self.manager.remove(workspace).map_err(|e| {
                format!("the stub is already at the tip {tip}, but `{name}` was not removed: {e}")
            })?;
            return Ok(tip);
        }
        // M1: an existing file is replaced only when declared.
        let clobbered: Vec<&str> = ic
            .stub
            .iter()
            .zip(&at_tip)
            .filter(|(f, t)| {
                matches!(t, AtTip::File { same: false })
                    && !ic.replaces.contains(&f.path)
                    && !scope.landed_before.contains(&f.path)
            })
            .map(|(f, _)| f.path.as_str())
            .collect();
        if !clobbered.is_empty() {
            let e = format!(
                "stub path(s) {} already exist at the integration tip {tip} and the contract \
                 does not declare them in `replaces`; a stub never overwrites a file it did not \
                 declare (0081)",
                clobbered.join(", ")
            );
            return Err(self.keep(Leftover::Workspace(workspace), &name, &root, e));
        }
        for f in &ic.stub {
            if let Err(e) =
                kgraf_sandbox::write_confined(&root, Path::new(&f.path), f.content.as_bytes())
            {
                let e = format!("cannot write `{}`: {e}", f.path);
                return Err(self.keep(Leftover::Workspace(workspace), &name, &root, e));
            }
        }
        let mut attempt = self
            .merge_back
            .attempt(workspace)
            .map_err(|e| format!("`{name}` cannot be a merge-back attempt: {e}"))?;
        let message = format!("kgraf: stub {}", ic.label());
        // The stub's own paths are the declared writes (decision 0083): the
        // scope check above already proved every one of them is inside the
        // producer's declared write targets, so this can only narrow what
        // `finish` accepts, never widen it.
        let writes: Vec<&str> = ic.stub.iter().map(|f| f.path.as_str()).collect();
        let outcome = self.merge_back.finish(&mut attempt, &message, &writes);
        let empty = matches!(outcome, Ok(FinishOutcome::Empty));
        let err = match outcome {
            Ok(FinishOutcome::Merged { merge_commit, .. }) => return Ok(merge_commit),
            // Unreachable after the M2 check above unless the tree changed
            // between the inspection and the commit: said, never guessed.
            Ok(FinishOutcome::Empty) => format!(
                "the stub of {} produced no change against the integration tip {tip}, though \
                 the tip did not already hold it; nothing was landed",
                ic.label()
            ),
            Ok(FinishOutcome::Refused(refusal)) => format!(
                "merge-back refused the stub of {}: {:?}",
                ic.label(),
                refusal.reasons
            ),
            Ok(FinishOutcome::Conflicted(conflict)) => format!(
                "the stub of {} conflicts with the integration tip {}: {:?}",
                ic.label(),
                conflict.integration_tip,
                conflict.paths
            ),
            Err(e) => land_error(&ic.label(), e),
        };
        // An empty attempt's workspace is already gone (`MergeBack::commit`);
        // every other outcome keeps it for inspection until the sweep.
        if !empty {
            lock(&self.leftovers).push(Leftover::Attempt(Box::new(attempt)));
        }
        Err(err)
    }

    /// Remove the kept leftover named `name`, if any.
    fn discard_leftover(&self, name: &str) -> Result<(), String> {
        let mut kept = lock(&self.leftovers);
        let Some(i) = kept.iter().position(|l| l.name() == name) else {
            return Ok(());
        };
        match kept.remove(i) {
            Leftover::Workspace(ws) => self.manager.remove(ws).map_err(|e| e.to_string()),
            Leftover::Attempt(mut a) => self.merge_back.discard(&mut a).map_err(|e| e.to_string()),
        }
        .map_err(|e| format!("the earlier stub worktree `{name}` was not removed: {e}"))
    }

    /// Keep `left` for inspection until [`GitStubLander::sweep`] and name
    /// where it is in the error.
    fn keep(&self, left: Leftover, name: &str, root: &Path, e: String) -> String {
        lock(&self.leftovers).push(left);
        format!(
            "the stub was not landed from `{name}` at {}: {e}",
            root.display()
        )
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn land_error(label: &str, e: MergeError) -> String {
    format!("cannot land the stub of {label}: {e}")
}

/// M1: every stub path is one of the producer's declared write targets.
/// Exact comparison, as `consumer_tests::producer_of` compares them.
fn check_scope(ic: &InterfaceContract, scope: &StubScope) -> Result<(), String> {
    let outside: Vec<&str> = ic
        .stub
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| !scope.producer_writes.contains(*p))
        .collect();
    if outside.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "stub path(s) {} of {} are not among the producer's declared write targets [{}]; \
             a stub lands only where its producer will write (0081)",
            outside.join(", "),
            ic.label(),
            scope
                .producer_writes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// H1: how each stub path stands under `root` (a worktree at the tip),
/// inspected without following any link. A symbolic link at any component,
/// the leaf included, is refused naming it: the tip may carry links a worker
/// committed, and a write through `gen -> /elsewhere` would land outside the
/// worktree.
///
/// **Not superseded by decision 0083.** `write_confined` (`kgraf_sandbox`)
/// now refuses a link at any component itself, leaf included, so this walk
/// duplicates that protection rather than being the only line of defense.
/// It is kept anyway for two reasons a single `write_confined` call per file
/// does not give: it inspects every stub path *before* any file is written
/// into the throwaway worktree, so a link on stub path 3 of 5 never leaves
/// 1 and 2 written into a workspace that is about to be discarded and kept
/// for inspection (a cleaner leftover to read); and it names the link with
/// the H1 wording ("is a symbolic link at the integration tip") the tests
/// pin, which is this call's own contract independent of whatever message
/// `write_confined` chooses. The write that follows still goes through
/// `write_confined`, so a link planted after this inspection (a second
/// process touching the same throwaway worktree, which nothing here spawns)
/// is refused there too. No process runs in this worktree meanwhile: it was
/// created by this lander a moment ago.
fn inspect_tip(root: &Path, files: &[FileOverlay]) -> Result<Vec<AtTip>, String> {
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let rel = Path::new(&f.path);
        let mut walked = PathBuf::new();
        let mut state = AtTip::Absent;
        let count = rel.components().count();
        for (i, component) in rel.components().enumerate() {
            walked.push(component);
            let leaf = i + 1 == count;
            let meta = match std::fs::symlink_metadata(root.join(&walked)) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(format!("cannot inspect `{}`: {e}", walked.display())),
            };
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "`{}` is a symbolic link at the integration tip, on the way to stub path \
                     `{}`; a stub is never written through or over a link (0081)",
                    walked.display(),
                    f.path
                ));
            }
            if leaf {
                if !meta.is_file() {
                    return Err(format!(
                        "stub path `{}` is not a regular file at the integration tip",
                        f.path
                    ));
                }
                let bytes = std::fs::read(root.join(&walked))
                    .map_err(|e| format!("cannot read `{}`: {e}", f.path))?;
                state = AtTip::File {
                    same: bytes == f.content.as_bytes(),
                };
            } else if !meta.is_dir() {
                return Err(format!(
                    "`{}` on the way to stub path `{}` is not a directory",
                    walked.display(),
                    f.path
                ));
            }
        }
        out.push(state);
    }
    Ok(out)
}

#[async_trait::async_trait]
impl StubLander for GitStubLander {
    async fn land(&self, ic: &InterfaceContract, scope: &StubScope) -> Result<String, String> {
        let this = GitStubLander {
            manager: self.manager.clone(),
            merge_back: self.merge_back.clone(),
            touched: self.touched.clone(),
            leftovers: self.leftovers.clone(),
        };
        let ic = ic.clone();
        let scope = scope.clone();
        tokio::task::spawn_blocking(move || this.land_blocking(&ic, &scope))
            .await
            .map_err(|e| format!("the stub-land task panicked: {e}"))?
    }
}

/// The paths of `ic.stub` whose delivered content (`delivered`, keyed by the
/// same repository-relative path) is still byte-identical to the stub's own
/// content: the producer's verification names these (requirement 3, 0081).
///
/// **Detection chosen, and why not a marker comment.** A marker comment would
/// have to be valid syntax in whatever language each stub file is (the
/// contract does not declare one), and would have to be added *after* the
/// stub-compile check already proved the file compiles -- so the committed
/// stub and the checked stub would differ, which is exactly the drift 0081
/// exists to close. Byte-identity against the stub the checks already proved
/// needs no injected syntax and never touches what was checked: a producer
/// that delivers anything else, even a one-character change, clears every
/// path it touched.
pub fn stub_left_in_place<'a>(
    ic: &'a InterfaceContract,
    delivered: &BTreeMap<String, String>,
) -> Vec<&'a str> {
    ic.stub
        .iter()
        .filter(|f| delivered.get(&f.path).is_some_and(|d| *d == f.content))
        .map(|f| f.path.as_str())
        .collect()
}

/// [`CheckFailure`](crate::checks::CheckFailure) for a producer that left a
/// stub in place at verification, in the same shape a mechanical check
/// failure takes (0037: the artifact named, `check_failed`'s existing
/// hold/report path). Not wired to a live call site in this worktree: the
/// call needs the interface's stub content at the point a producer's
/// delivery becomes final (`orchestrator.rs::finalize_ordinary`), which is
/// orchestrator run-state the verifier (`kgraf-host::CommandVerifier`) does
/// not carry today (`OpenContract` has no interface linkage) -- the same gap
/// 0076 recorded for its own step 3 and left as the next build's work rather
/// than widen this one's blast radius into `finalize`/`OpenContract`'s
/// serialized shape. [`stub_left_in_place`] is complete and tested standalone;
/// wiring this in is that next step.
pub fn stub_left_in_place_failure(
    ic: &InterfaceContract,
    left: &[&str],
) -> crate::checks::CheckFailure {
    crate::checks::CheckFailure {
        stage: CheckStage::StubLand,
        artifact: ic.artifact(CheckStage::StubLand),
        detail: format!(
            "the producer's delivery still carries the stub's own content at {}: {}, unchanged \
             since it was committed as {}'s stub. A stub left in place is a commitment failure \
             (0081), never merged as if it were the producer's work",
            left.len(),
            left.join(", "),
            ic.label()
        ),
    }
}

/// The general-path counterpart to [`stub_left_in_place`] (decision 0086):
/// applied to any contract, not only an interface's producer, and to
/// whatever a declared write target held at dispatch, not only a stub
/// `stub_land` itself committed.
///
/// [`stub_left_in_place`]'s byte-identity check needs the stub's own
/// recorded content to compare against; an ordinary contract has none (only
/// an interface's producer does). A no-op attempt's declared write target is
/// always byte-identical to what it held at dispatch by construction (an
/// empty attempt changed nothing at all), so byte-identity alone proves
/// nothing here -- it would flag a legitimately unchanged file (a
/// re-verify, a check-only contract) exactly as readily as an abandoned
/// stub. What tells the two apart is *what* the unchanged content is: a
/// placeholder body a real implementation never takes the shape of.
///
/// **Placeholder, defined.** A file is a placeholder only when every
/// function body in it, once line comments and surrounding whitespace are
/// stripped, is nothing but one call to `todo!`, `unimplemented!` or
/// `unreachable!` -- the bodies a worker's own dispatched brief, or the
/// project it was handed, leaves for work not yet done. Only `.rs` files are
/// recognized today, kGraf's own implementation language (decision 0054); a
/// file with no function bodies, or with even one real one, is never
/// flagged. Legitimately unchanged content cannot take this shape: a
/// completed function does not consist solely of "not implemented yet"
/// markers, and a check-only or already-accepted contract's write target is
/// ordinary, working code, not a bare placeholder call. A cosmetic edit to a
/// stub (a comment, a renamed unused argument) still clears the file, the
/// same narrow limit [`stub_left_in_place`] itself accepts (0081's own
/// docs).
pub fn is_placeholder_source(path: &str, content: &str) -> bool {
    if !path.ends_with(".rs") {
        return false;
    }
    let bodies = rust_fn_bodies(content);
    !bodies.is_empty() && bodies.iter().all(|b| is_placeholder_body(b))
}

/// A line-comment strip, not a lexer: good enough for a placeholder body,
/// which never legitimately contains a string literal holding `//`.
fn strip_line_comments(s: &str) -> String {
    s.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_placeholder_body(body: &str) -> bool {
    let stripped = strip_line_comments(body);
    let t = stripped.trim();
    let t = t.strip_suffix(';').map_or(t, str::trim_end);
    for name in ["todo!", "unimplemented!", "unreachable!"] {
        if let Some(rest) = t.strip_prefix(name) {
            let rest = rest.trim();
            if rest.starts_with('(') && rest.ends_with(')') {
                return true;
            }
        }
    }
    false
}

/// Every top-level `fn ... { ... }` body in a Rust source file, found by a
/// brace-matching scan rather than a parse: this call site only asks whether
/// every body found is a placeholder, never compiles or lints the file, and
/// a stub file is short and plain enough that a scan finds its bodies
/// reliably. A signature with no body (a trait method declaration, ending in
/// `;`) is skipped rather than treated as a placeholder or as disqualifying
/// one: it is not a body a worker could have implemented at this file.
fn rust_fn_bodies(content: &str) -> Vec<String> {
    let bytes = content.as_bytes();
    let mut bodies = Vec::new();
    let mut i = 0;
    while let Some(rel) = content[i..].find("fn ") {
        let start = i + rel;
        if start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                i = start + 3;
                continue;
            }
        }
        let Some(brace_rel) = content[start..].find(['{', ';']) else {
            break;
        };
        let marker = start + brace_rel;
        if bytes[marker] == b';' {
            i = marker + 1;
            continue;
        }
        let body_start = marker + 1;
        let mut depth = 1usize;
        let mut j = body_start;
        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        if depth != 0 {
            break;
        }
        bodies.push(content[body_start..j - 1].to_string());
        i = j;
    }
    bodies
}

/// Every write target in `writes` (repository-relative paths) whose content,
/// read through `read`, [`is_placeholder_source`]. `read` returns `None` for
/// a path with nothing to read (absent, or not a regular file), which is
/// never a placeholder by this definition.
pub fn placeholder_writes_left_in_place<'a>(
    writes: impl IntoIterator<Item = &'a str>,
    read: impl Fn(&str) -> Option<String>,
) -> Vec<&'a str> {
    writes
        .into_iter()
        .filter(|path| read(path).is_some_and(|content| is_placeholder_source(path, &content)))
        .collect()
}

/// A refusal reason for [`placeholder_writes_left_in_place`], in the same
/// voice as [`stub_left_in_place_failure`] but shaped as a plain `String`
/// (this path's caller, `kgraf-host`'s `CommandVerifier`, returns
/// `Verdict::Fail { reason }`, not a `CheckFailure`; there is no interface or
/// artifact to name here, only the contract already in the reason's own
/// context).
pub fn placeholder_left_in_place_reason(left: &[&str]) -> String {
    format!(
        "the declared write target{} still only {} a placeholder body from dispatch, unchanged: \
         {}. An attempt that leaves a stub in place is a commitment failure (0081, generalized by \
         0086), never verified as if it were the work",
        if left.len() == 1 { "" } else { "s" },
        if left.len() == 1 { "holds" } else { "hold" },
        left.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_sandbox::{Identity, WorkspaceManager};
    use std::path::PathBuf;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_out(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        manager: Arc<WorkspaceManager>,
        merge_back: Arc<MergeBack>,
    }

    /// A real repository with a `kgraf/r-1` integration branch: an adapter is
    /// tested against the thing it speaks to (a real git repo), not a
    /// restatement of one.
    fn fixture() -> Fixture {
        fixture_with(|_, _| {})
    }

    /// As [`fixture`], with `setup(repo, tempdir)` run before the first
    /// commit, so what it creates is at the integration tip.
    fn fixture_with(setup: impl FnOnce(&Path, &Path)) -> Fixture {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("repo");
        let base = dir.path().join("ws");
        std::fs::create_dir_all(&root).expect("mkdir");
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "hello\n").expect("write");
        setup(&root, dir.path());
        git(&root, &["add", "."]);
        git(
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
        git(&root, &["branch", "kgraf/r-1", "HEAD"]);
        let manager = Arc::new(WorkspaceManager::open(&root, &base).expect("manager"));
        let merge_back = Arc::new(
            MergeBack::open(
                manager.clone(),
                "kgraf/r-1",
                Identity {
                    name: "Test Owner".into(),
                    email: "owner@example.invalid".into(),
                },
            )
            .expect("merge-back"),
        );
        Fixture {
            _dir: dir,
            root,
            manager,
            merge_back,
        }
    }

    /// The scope a producer declaring every stub path gives.
    fn scope(ic: &InterfaceContract) -> StubScope {
        StubScope {
            producer_writes: ic.stub.iter().map(|f| f.path.clone()).collect(),
            landed_before: BTreeSet::new(),
        }
    }

    fn tip(f: &Fixture) -> String {
        git_out(&f.root, &["rev-parse", "kgraf/r-1"])
    }

    fn base(f: &Fixture) -> String {
        git_out(&f.root, &["rev-parse", "main"])
    }

    fn ic(id: &str, version: u32, stub: Vec<FileOverlay>) -> InterfaceContract {
        InterfaceContract {
            id: id.into(),
            version,
            parties: vec!["prod".into(), "cons".into()],
            symbols: vec!["parse".into()],
            stub: stub.clone(),
            wrong: vec![],
            reference: stub
                .iter()
                .map(|f| FileOverlay::new(f.path.clone(), "pub fn parse() -> u16 { 8080 }"))
                .collect(),
            consumer: vec![],
            tests: vec![FileOverlay::new("tests/t.rs", "")],
            compile: vec!["true".into()],
            test: vec!["true".into()],
            consulted: vec![],
            replaces: vec![],
        }
    }

    #[tokio::test]
    async fn a_landed_stub_is_on_the_integration_branch_before_any_party_exists() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "config",
            1,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse() -> u16 { 0 }\n",
            )],
        );
        let commit = lander
            .land(&contract, &scope(&contract))
            .await
            .expect("land");
        // The consumer's worktree, branched *after* landing, already has the
        // stub file -- requirement 6's first test, phrased against the real
        // branch tip rather than a second worktree, since both read the same
        // ref.
        let tip = git_out(&f.root, &["rev-parse", "kgraf/r-1"]);
        assert_eq!(
            tip, commit,
            "the integration branch moved to the landed commit"
        );
        let content = git_out(&f.root, &["show", &format!("{tip}:src/config.rs")]);
        assert_eq!(content, "pub fn parse() -> u16 { 0 }");
        // The merge commit's own subject names the landed attempt; its second
        // parent is the stub commit itself, whose message names the interface.
        let stub_message = git_out(&f.root, &["log", "-1", "--format=%s", &format!("{tip}^2")]);
        assert!(stub_message.contains("config@v1"), "{stub_message}");
    }

    #[tokio::test]
    async fn a_superseded_version_lands_its_own_stub_without_rewriting_the_first() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let v1 = ic(
            "config",
            1,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse() -> u16 { 0 }\n",
            )],
        );
        let commit1 = lander.land(&v1, &scope(&v1)).await.expect("land v1");
        let v2 = ic(
            "config",
            2,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse(s: &str) -> u16 { 0 }\n",
            )],
        );
        // The path exists at the tip now, put there by v1's own stub: the
        // book's `landed_paths` lets the revision replace it (M1).
        let v2_scope = StubScope {
            landed_before: ["src/config.rs".to_owned()].into(),
            ..scope(&v2)
        };
        let commit2 = lander.land(&v2, &v2_scope).await.expect("land v2");
        assert_ne!(commit1, commit2);
        // History is append-only: v1's commit is still reachable, unrewritten.
        let parents = git_out(&f.root, &["rev-list", "--all", "--count"]);
        assert!(parents.parse::<u32>().unwrap() >= 3, "{parents}");
        let v1_content = git_out(&f.root, &["show", &format!("{commit1}:src/config.rs")]);
        assert_eq!(v1_content, "pub fn parse() -> u16 { 0 }");
        let tip = git_out(&f.root, &["rev-parse", "kgraf/r-1"]);
        let tip_content = git_out(&f.root, &["show", &format!("{tip}:src/config.rs")]);
        assert_eq!(tip_content, "pub fn parse(s: &str) -> u16 { 0 }");
    }

    #[tokio::test]
    async fn a_stub_touching_a_protected_path_is_refused_and_the_refusal_names_it() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "evil",
            1,
            vec![
                FileOverlay::new("src/config.rs", "pub fn parse() -> u16 { 0 }\n"),
                FileOverlay::new(".gitattributes", "* filter=evil\n"),
            ],
        );
        let err = lander
            .land(&contract, &scope(&contract))
            .await
            .expect_err("refused");
        assert!(err.contains(".gitattributes"), "{err}");
        // Refused: never merged, so the branch did not move.
        let tip = git_out(&f.root, &["rev-parse", "kgraf/r-1"]);
        let base = git_out(&f.root, &["rev-parse", "main"]);
        assert_eq!(tip, base, "a refused stub must never reach the branch");
    }

    /// H1: the tip holds a directory link `gen -> <outside>` and a leaf link
    /// `src/leaf.rs -> <outside>/leaf.rs`, as a worker's merged commit can.
    /// A stub through either is refused naming the link, and nothing is
    /// written outside the worktree.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stub_through_a_directory_or_leaf_symlink_at_the_tip_is_refused() {
        let f = fixture_with(|repo, dir| {
            let outside = dir.join("outside");
            std::fs::create_dir_all(&outside).expect("mkdir");
            std::fs::write(outside.join("leaf.rs"), "original\n").expect("write");
            std::os::unix::fs::symlink(&outside, repo.join("gen")).expect("dir link");
            std::fs::create_dir_all(repo.join("src")).expect("mkdir");
            std::os::unix::fs::symlink(outside.join("leaf.rs"), repo.join("src/leaf.rs"))
                .expect("leaf link");
        });
        let outside = f._dir.path().join("outside");
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());

        let through_dir = ic("gen", 1, vec![FileOverlay::new("gen/pwn.rs", "pwned\n")]);
        let err = lander
            .land(&through_dir, &scope(&through_dir))
            .await
            .expect_err("a directory link is refused");
        assert!(err.contains("`gen` is a symbolic link"), "{err}");
        assert!(!outside.join("pwn.rs").exists(), "nothing written outside");

        let over_leaf = ic("leaf", 1, vec![FileOverlay::new("src/leaf.rs", "pwned\n")]);
        let err = lander
            .land(&over_leaf, &scope(&over_leaf))
            .await
            .expect_err("a leaf link is refused");
        assert!(err.contains("`src/leaf.rs` is a symbolic link"), "{err}");
        assert_eq!(
            std::fs::read_to_string(outside.join("leaf.rs")).expect("read"),
            "original\n",
            "the link's target is untouched"
        );
        assert_eq!(tip(&f), base(&f), "a refused stub never reaches the branch");
    }

    /// M1: a stub path the producer did not declare is refused, even one
    /// that is new at the tip.
    #[tokio::test]
    async fn a_stub_path_outside_the_producers_writes_is_refused() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "config",
            1,
            vec![
                FileOverlay::new("src/config.rs", "pub fn parse() -> u16 { 0 }\n"),
                FileOverlay::new("build.rs", "fn main() {}\n"),
            ],
        );
        let narrow = StubScope {
            producer_writes: ["src/config.rs".to_owned()].into(),
            landed_before: BTreeSet::new(),
        };
        let err = lander.land(&contract, &narrow).await.expect_err("refused");
        assert!(
            err.contains("build.rs") && err.contains("declared write targets"),
            "{err}"
        );
        assert_eq!(tip(&f), base(&f));
    }

    /// M1: a stub path that already exists at the tip is refused unless the
    /// contract declares it in `replaces`; declared, it lands.
    #[tokio::test]
    async fn a_stub_replacing_an_existing_file_is_refused_unless_declared() {
        let f = fixture_with(|repo, _| {
            std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"real\"\n").expect("write");
        });
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let mut contract = ic(
            "manifest",
            1,
            vec![FileOverlay::new(
                "Cargo.toml",
                "[package]\nname = \"stub\"\n",
            )],
        );
        let err = lander
            .land(&contract, &scope(&contract))
            .await
            .expect_err("an undeclared replacement is refused");
        assert!(
            err.contains("Cargo.toml") && err.contains("replaces"),
            "{err}"
        );
        assert_eq!(tip(&f), base(&f));
        contract.replaces = vec!["Cargo.toml".into()];
        let commit = lander
            .land(&contract, &scope(&contract))
            .await
            .expect("a declared replacement lands");
        assert_eq!(
            git_out(&f.root, &["show", &format!("{commit}:Cargo.toml")]),
            "[package]\nname = \"stub\""
        );
    }

    /// M2, crash window: the merge happened but `StubLanded` was never
    /// recorded, so a resume lands the same version again. The tip already
    /// holds exactly the stub: the tip is returned, never an empty-attempt
    /// failure, and nothing new is committed.
    #[tokio::test]
    async fn a_re_land_after_a_crash_past_the_merge_returns_the_tip() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "config",
            1,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse() -> u16 { 0 }\n",
            )],
        );
        let first = lander
            .land(&contract, &scope(&contract))
            .await
            .expect("land");
        let again = lander
            .land(&contract, &scope(&contract))
            .await
            .expect("the re-land is recorded, not refused");
        assert_eq!(again, first, "the tip is the landed commit");
        assert_eq!(tip(&f), first, "nothing new was committed");
    }

    /// A real content conflict, not a scope refusal: the branch moves with a
    /// genuinely competing edit to the same line the stub also touches,
    /// between the tip [`GitStubLander::land_at`] is given and the merge
    /// itself, exactly the race decision 0081's own note in `inspect_tip`
    /// says a test through the public, always-freshest-tip `land` cannot
    /// place deterministically. [`MergeBack::finish`] must return
    /// `FinishOutcome::Conflicted`, which `land_at` reports as a typed
    /// failure string naming the conflict, never as a silent pick of either
    /// side; the racer's own commit must be the untouched tip.
    #[tokio::test]
    async fn a_real_merge_conflict_on_the_run_branch_is_reported_and_nothing_is_written() {
        let f = fixture_with(|repo, _| {
            std::fs::create_dir_all(repo.join("src")).expect("mkdir");
            std::fs::write(repo.join("src/shared.rs"), "pub fn shared() -> u16 { 1 }\n")
                .expect("write");
        });
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let stale_tip = tip(&f);

        // Advance `kgraf/r-1` with an independent edit to the very line the
        // stub below also changes, through a plain worktree this lander
        // never touches: the two histories genuinely conflict rather than
        // one being a fast-forward of the other.
        let race_dir = f._dir.path().join("race");
        git(
            &f.root,
            &[
                "worktree",
                "add",
                "-q",
                race_dir.to_str().expect("utf8"),
                "kgraf/r-1",
            ],
        );
        std::fs::write(
            race_dir.join("src/shared.rs"),
            "pub fn shared() -> u16 { 2 }\n",
        )
        .expect("write");
        git(&race_dir, &["add", "."]);
        git(
            &race_dir,
            &[
                "-c",
                "user.name=racer",
                "-c",
                "user.email=racer@example.invalid",
                "commit",
                "-q",
                "-m",
                "a concurrent, non-lander edit of the same line",
            ],
        );
        let racer_commit = git_out(&race_dir, &["rev-parse", "HEAD"]);
        git(
            &f.root,
            &[
                "worktree",
                "remove",
                "--force",
                race_dir.to_str().expect("utf8"),
            ],
        );
        assert_eq!(
            tip(&f),
            racer_commit,
            "the racer's commit is the branch tip the stub's merge will meet"
        );

        let mut contract = ic(
            "shared",
            1,
            vec![FileOverlay::new(
                "src/shared.rs",
                "pub fn shared() -> u16 { 3 }\n",
            )],
        );
        // The file exists at the stale tip this stub is written against, so
        // the M1 clobber check must be told the stub is allowed to replace it.
        contract.replaces = vec!["src/shared.rs".into()];
        let scope = scope(&contract);

        let err = lander
            .land_at(&stale_tip, &contract, &scope)
            .expect_err("a genuine content conflict is refused, never silently resolved");
        assert!(
            err.contains("conflicts with the integration tip"),
            "the failure names the conflict: {err}"
        );

        let final_tip = tip(&f);
        assert_eq!(
            final_tip, racer_commit,
            "the branch never moved: a conflict merges nothing"
        );
        assert_eq!(
            git_out(&f.root, &["show", &format!("{final_tip}:src/shared.rs")]),
            "pub fn shared() -> u16 { 2 }",
            "the racer's content stands; none of the stub's bytes were written to the branch"
        );
    }

    /// M2, revision: a revised version whose stub bytes did not change is
    /// landed at the tip that already holds them.
    #[tokio::test]
    async fn a_revision_with_identical_stub_content_is_landed_at_the_tip() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let stub = vec![FileOverlay::new(
            "src/config.rs",
            "pub fn parse() -> u16 { 0 }\n",
        )];
        let v1 = ic("config", 1, stub.clone());
        let first = lander.land(&v1, &scope(&v1)).await.expect("land v1");
        let v2 = ic("config", 2, stub);
        let second = lander.land(&v2, &scope(&v2)).await.expect("land v2");
        assert_eq!(second, first);
        assert_eq!(tip(&f), first);
    }

    /// L1: a refused land keeps its worktree for inspection; the end-of-run
    /// sweep removes it and names it.
    #[tokio::test]
    async fn a_refused_land_keeps_its_worktree_until_the_end_of_run_sweep() {
        let f = fixture_with(|repo, _| {
            std::fs::write(repo.join("Cargo.toml"), "[package]\n").expect("write");
        });
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "manifest",
            1,
            vec![FileOverlay::new(
                "Cargo.toml",
                "[package]\nname = \"stub\"\n",
            )],
        );
        lander
            .land(&contract, &scope(&contract))
            .await
            .expect_err("refused");
        let kept = f.manager.base().join("stub-manifest-v1");
        assert!(kept.exists(), "kept for inspection");
        let swept = lander.sweep().expect("sweep");
        assert!(!kept.exists(), "removed at the end of the run");
        assert!(
            swept.iter().any(|s| s.contains("stub-manifest-v1")),
            "{swept:?}"
        );
    }

    /// L1: an attempt ref a stub land left (a crash between commit and
    /// merge) is removed by the end-of-run sweep. A real conflict needs the
    /// branch to move between `create` and `merge`, which a test cannot
    /// place deterministically; the refused test above covers a kept
    /// worktree, and a conflicted one is kept the same way (`Leftover`).
    #[tokio::test]
    async fn a_stub_attempt_ref_left_by_a_crash_is_removed_by_the_sweep() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "config",
            1,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse() -> u16 { 0 }\n",
            )],
        );
        lander
            .land(&contract, &scope(&contract))
            .await
            .expect("land");
        let left = format!("{}stub-config-v1", kgraf_sandbox::ATTEMPT_REF_PREFIX);
        git(&f.root, &["update-ref", &left, "HEAD"]);
        let swept = lander.sweep().expect("sweep");
        assert!(swept.contains(&left), "{swept:?}");
        let listed = git_out(&f.root, &["for-each-ref", "--format=%(refname)", &left]);
        assert!(listed.is_empty(), "the stub attempt ref is gone: {listed}");
    }

    #[test]
    fn a_producer_that_never_touched_a_stub_path_is_named_left_in_place() {
        let contract = ic(
            "config",
            1,
            vec![
                FileOverlay::new("src/config.rs", "pub fn parse() -> u16 { 0 }\n"),
                FileOverlay::new("src/other.rs", "pub fn other() {}\n"),
            ],
        );
        let mut delivered = BTreeMap::new();
        delivered.insert(
            "src/config.rs".to_owned(),
            "pub fn parse() -> u16 { 0 }\n".to_owned(),
        );
        delivered.insert(
            "src/other.rs".to_owned(),
            "pub fn other() { real() }\n".to_owned(),
        );
        let left = stub_left_in_place(&contract, &delivered);
        assert_eq!(left, vec!["src/config.rs"]);
    }

    #[test]
    fn a_producer_that_replaced_every_stub_path_is_never_named() {
        let contract = ic(
            "config",
            1,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse() -> u16 { 0 }\n",
            )],
        );
        let mut delivered = BTreeMap::new();
        delivered.insert(
            "src/config.rs".to_owned(),
            "pub fn parse(s: &str) -> u16 { s.parse().unwrap_or(0) }\n".to_owned(),
        );
        assert!(stub_left_in_place(&contract, &delivered).is_empty());
    }

    #[test]
    fn the_failure_names_the_interface_and_the_paths() {
        let contract = ic(
            "config",
            3,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse() -> u16 { 0 }\n",
            )],
        );
        let failure = stub_left_in_place_failure(&contract, &["src/config.rs"]);
        assert_eq!(failure.stage, CheckStage::StubLand);
        assert!(failure.detail.contains("config@v3"), "{}", failure.detail);
        assert!(
            failure.detail.contains("src/config.rs"),
            "{}",
            failure.detail
        );
    }

    // -- decision 0086: the general, non-interface placeholder check --

    #[test]
    fn a_bare_todo_body_is_a_placeholder() {
        assert!(is_placeholder_source(
            "src/lib.rs",
            "pub fn parse(s: &str) -> u16 {\n    todo!()\n}\n"
        ));
    }

    #[test]
    fn unimplemented_and_unreachable_are_placeholders_too() {
        assert!(is_placeholder_source(
            "src/lib.rs",
            "fn a() { unimplemented!() }\nfn b() { unreachable!(\"never\") }\n"
        ));
    }

    #[test]
    fn a_todo_with_a_message_argument_is_still_a_placeholder() {
        assert!(is_placeholder_source(
            "src/lib.rs",
            "pub fn parse() -> u16 { todo!(\"fill in once the format is settled\") }\n"
        ));
    }

    #[test]
    fn a_placeholder_with_a_trailing_comment_is_still_a_placeholder() {
        assert!(is_placeholder_source(
            "src/lib.rs",
            "pub fn parse() -> u16 {\n    todo!() // fill me in\n}\n"
        ));
    }

    #[test]
    fn one_real_body_among_placeholders_clears_the_whole_file() {
        assert!(!is_placeholder_source(
            "src/lib.rs",
            "fn a() { todo!() }\nfn b() -> u16 { 42 }\n"
        ));
    }

    #[test]
    fn a_real_implementation_is_never_a_placeholder() {
        assert!(!is_placeholder_source(
            "src/lib.rs",
            "pub fn parse(s: &str) -> u16 { s.parse().unwrap_or(0) }\n"
        ));
    }

    #[test]
    fn a_file_with_no_function_bodies_is_never_a_placeholder() {
        // A pure data or trait-declaration file: no body to have left
        // unimplemented, so nothing here is a stub the worker skipped.
        assert!(!is_placeholder_source(
            "src/lib.rs",
            "pub struct Config {\n    pub port: u16,\n}\n\ntrait Parse {\n    fn parse(s: &str) -> Self;\n}\n"
        ));
    }

    #[test]
    fn a_non_rust_file_is_never_flagged() {
        assert!(!is_placeholder_source("README.md", "todo!()"));
    }

    #[test]
    fn placeholder_writes_left_in_place_names_only_the_stub_paths() {
        let mut files = BTreeMap::new();
        files.insert("src/stub.rs".to_owned(), "fn f() { todo!() }".to_owned());
        files.insert("src/done.rs".to_owned(), "fn f() -> u16 { 7 }".to_owned());
        let left = placeholder_writes_left_in_place(
            ["src/stub.rs", "src/done.rs", "src/absent.rs"],
            |p| files.get(p).cloned(),
        );
        assert_eq!(left, vec!["src/stub.rs"]);
    }

    #[test]
    fn the_general_reason_names_the_paths_and_cites_both_decisions() {
        let reason = placeholder_left_in_place_reason(&["src/stub.rs"]);
        assert!(reason.contains("src/stub.rs"), "{reason}");
        assert!(reason.contains("0081"), "{reason}");
        assert!(reason.contains("0086"), "{reason}");
    }
}
