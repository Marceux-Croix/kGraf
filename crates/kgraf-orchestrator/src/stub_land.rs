//! Stubs land on the integration branch first (decision 0081).
//!
//! An interface contract's checks (stub-compile, vacuity, handshake:
//! `checks.rs`) prove the consumers compile against a trivial stub of the
//! producer side, but they commit that stub nowhere: without this step every
//! worker's worktree branches from the plain integration tip, which does not
//! yet agree with the interface. A consumer written first builds against a brief,
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
//! **Security: reuse the merge-back path, not a new git path.**
//! [`GitStubLander`] never runs a bespoke git command; it writes
//! the stub's declared files into a throwaway worktree branched from the
//! integration tip and carries them through [`kgraf_sandbox::MergeBack`]
//! exactly as a worker's own attempt is carried (`worker/workspace.rs` in
//! `kgraf-host`, and `MergeBack::commit`/`merge` themselves): attributes
//! pinned to the integration tip's own tree (never a worker-writable
//! `.gitattributes`), the host runner's overrides (`kgraf_core::host_git`:
//! hooks, every configured driver, fsmonitor and transports off), the gitlink verified before
//! any git process touches the worktree, and a change to a protected path
//! (`.gitattributes`, `.gitmodules`, `.git*` directories, kGraf's own state)
//! refused loudly (`RefusalReason::ProtectedPath`) rather than
//! merged. No filter or merge driver ever runs: merge-back's `SafeGit` never
//! reads attributes from a worktree, and the host runner neutralizes every
//! driver the config defines whatever the attributes select.
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
//! **Symbolic links.** The throwaway worktree is checked out at the integration tip, which can hold
//! symbolic links a worker committed (merge-back's prescan skips links, then
//! `add -A` records them). So before any byte is written: every component of
//! every stub path is inspected without following links and a link anywhere
//! on the way, or at the leaf, is refused naming it; the write itself
//! goes through `kgraf_sandbox::write_confined`, the descriptor-relative,
//! no-follow writer the contract checks already use, never `root.join` plus
//! `std::fs::write`. A stub may only touch its producer's declared write
//! targets, and may replace a file that already exists at the tip only when
//! the contract declares it (`InterfaceContract::replaces`) or an earlier
//! version of the same interface landed it. A tip that already holds
//! exactly the stub is recorded as landed at that tip, not refused as an
//! empty attempt. Every worktree a refused or failed land leaves is kept
//! for inspection and removed by [`GitStubLander::sweep`] at the end of the
//! run.

use crate::checks::{CheckStage, FileOverlay, InterfaceContract};
use kgraf_sandbox::{
    Attempt, FinishOutcome, MergeBack, MergeError, Workspace, WorkspaceManager, WorkspaceSpec,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What a stub may touch, decided by the orchestrator
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
    /// attempt-ref sweep.
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

    /// At the end of a run: discard every worktree and attempt ref a
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
        // The tip already holds exactly this stub (a crash after the
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
        // An existing file is replaced only when declared.
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
            // Unreachable after the check above unless the tree changed
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

/// Every stub path is one of the producer's declared write targets.
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

/// How each stub path stands under `root` (a worktree at the tip),
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
/// the symbolic-link wording ("is a symbolic link at the integration tip") the tests
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
///
/// A compile-only interface's stub is the repository's own text, not one a
/// planner wrote: a scaffold file left as it was is work not done, and a file
/// of current code left as it was is a file the producer did not need to
/// change (decision 0030, amendment of 2026-10-06). Only its scaffold files
/// count here.
pub fn stub_left_in_place<'a>(
    ic: &'a InterfaceContract,
    delivered: &BTreeMap<String, String>,
) -> Vec<&'a str> {
    ic.stub
        .iter()
        .filter(|f| {
            !ic.is_compile_only() || crate::placeholder::has_placeholder_body(&f.path, &f.content)
        })
        .filter(|f| delivered.get(&f.path).is_some_and(|d| *d == f.content))
        .map(|f| f.path.as_str())
        .collect()
}

/// [`CheckFailure`](crate::checks::CheckFailure) for a producer that left a
/// stub in place at verification, in the same shape a mechanical check
/// failure takes (0037: the artifact named, `check_failed`'s existing
/// hold/report path). The orchestrator calls it where a producer's delivery
/// becomes final, because it holds the interface's stub content there; the
/// verifier (`kgraf-host::CommandVerifier`) does not, since `OpenContract`
/// has no interface linkage.
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
/// stripped, is exactly one call to `todo!` or `unimplemented!` -- the
/// bodies a worker's own dispatched brief, or the project it was handed,
/// leaves for work not yet done. `unreachable!` is not one (a mock of
/// methods the code under test never calls is made of it), and a body of a
/// placeholder call followed by anything else is real work. Every other
/// language kGraf has a grammar for (Python, Ruby, TypeScript, JavaScript, Go,
/// Java, C, C++, C#, PHP) is read by
/// [`crate::placeholder::every_body_is_placeholder`]: every body is one
/// placeholder statement (`raise NotImplementedError`, `throw new
/// NotImplementedException()`, a raised "not implemented" message), and an
/// abstract method of Python or Ruby is not one. A file in a language with no
/// grammar is never flagged. A file with no function bodies, or with even one real one, is never
/// flagged. Legitimately unchanged content cannot take this shape: a
/// completed function does not consist solely of "not implemented yet"
/// markers, and a check-only or already-accepted contract's write target is
/// ordinary, working code, not a bare placeholder call. A cosmetic edit to a
/// stub (a comment, a renamed unused argument) still clears the file, the
/// same narrow limit [`stub_left_in_place`] itself accepts (0081's own
/// docs).
///
/// Bodies inside `impl Trait for Type` blocks count as placeholders only
/// when no function body sits outside such a block and the file has no item
/// besides `use`, `mod`, `type`, plain structs and trait impls: enums, error
/// types with non-standard derives, constants, macro calls and anything under
/// `cfg` next to a mock of unimplemented methods are a finished file. A
/// struct and its all-placeholder trait impls alone are an abandoned stub. A file over
/// [`MAX_SCANNED_BYTES`] is never a placeholder.
pub fn is_placeholder_source(path: &str, content: &str) -> bool {
    if content.len() > MAX_SCANNED_BYTES || !has_placeholder_reader(path) {
        return false;
    }
    if !path.ends_with(".rs") {
        return crate::placeholder::every_body_is_placeholder(path, content);
    }
    // Comments and literals removed by the shared lexer (`crate::placeholder`),
    // so a brace, quote, `fn ` or macro name in text is not code. A Rust
    // grammar always loads; no grammar would mean no scan, so no refusal.
    let Some(masked) = crate::placeholder::code_only(path, content) else {
        return false;
    };
    let scan = scan_source(&masked);
    let placeholders = |bodies: &[String]| bodies.iter().all(|b| is_placeholder_body(b));
    if !scan.free.is_empty() {
        return placeholders(&scan.free) && placeholders(&scan.trait_impl);
    }
    // Only trait impl bodies: a mock or an adapter written next to real items
    // (error types, data types, macro calls) is a finished file whose methods
    // are unused. Alone, in a file of `use`, `mod` and `type` items, they are
    // an abandoned stub.
    !scan.trait_impl.is_empty() && !scan.real_items && placeholders(&scan.trait_impl)
}

/// The macros a placeholder body is one call to. `unreachable!` is not one:
/// it states an invariant, and a mock or a trait impl of a method the code
/// under test never calls legitimately consists of nothing else.
const PLACEHOLDER_MACROS: [&str; 2] = ["todo", "unimplemented"];

/// True when the body (already masked by [`crate::placeholder::code_only`]) is exactly one
/// call to a [`PLACEHOLDER_MACROS`] macro, with an optional `return` before
/// it, an optional trailing `;` and nothing else: `todo!(); real()` is real
/// work after a placeholder call, not a placeholder. Any delimiter counts
/// (`todo!()`, `todo![]`, `todo!{}`), and the macro may be named by a path
/// (`std::todo!`, `::core::unimplemented!`).
fn is_placeholder_body(body: &str) -> bool {
    let t = body.trim();
    let t = match t.strip_prefix("return") {
        Some(rest) if rest.starts_with(|c: char| c.is_whitespace()) => rest.trim_start(),
        _ => t,
    };
    let t = t.strip_suffix(';').map_or(t, str::trim_end);
    let mut t = t.strip_prefix("::").map_or(t, str::trim_start);
    // A `std::` or `core::` root, with spaces allowed around `::`.
    for root in ["std", "core"] {
        if let Some(rest) = t.strip_prefix(root).map(str::trim_start)
            && let Some(rest) = rest.strip_prefix("::")
        {
            t = rest.trim_start();
            break;
        }
    }
    PLACEHOLDER_MACROS.iter().any(|name| {
        t.strip_prefix(name)
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('!'))
            .map(str::trim_start)
            .is_some_and(|rest| rest.starts_with(['(', '[', '{']) && closes_at_end(rest))
    })
}

/// True when the delimiter that starts `text` is closed by the last
/// character of `text` and by no earlier one. `text` is masked, so no quote
/// or comment can hide a delimiter.
fn closes_at_end(text: &str) -> bool {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + c.len_utf8() == text.len();
                }
            }
            _ => {}
        }
    }
    false
}

/// The end of a function signature that starts at `from`: the byte of the
/// `{` that opens its body, or of the `;` that ends a declaration. A `;` or
/// `{` inside `(..)`, `[..]` or `<..>` (an array type `[u8; 4]`, a const
/// generic) belongs to the signature. `->` and `=>` do not close an angle.
fn signature_end(content: &str, from: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut nest = 0usize;
    let mut angle = 0usize;
    for (k, &b) in bytes.iter().enumerate().skip(from) {
        match b {
            b'(' | b'[' => nest += 1,
            b')' | b']' => nest = nest.saturating_sub(1),
            b'<' if nest == 0 => angle += 1,
            b'>' if nest == 0 && k > 0 && !matches!(bytes[k - 1], b'-' | b'=') => {
                angle = angle.saturating_sub(1)
            }
            b'{' | b';' if nest == 0 && angle == 0 => return Some(k),
            _ => {}
        }
    }
    None
}

/// The largest source file [`is_placeholder_source`] scans. A stub is a few
/// lines; a file this size is real code.
const MAX_SCANNED_BYTES: usize = 4 * 1024 * 1024;

/// What [`scan_source`] found in a Rust source file.
#[derive(Default)]
struct Scan {
    /// Function bodies outside `impl Trait for Type` blocks: free functions,
    /// inherent methods, default methods of a trait definition.
    free: Vec<String>,
    /// Function bodies inside `impl Trait for Type` blocks.
    trait_impl: Vec<String>,
    /// An item other than a function, `use`, `mod`, `type`, `extern crate`,
    /// trait impl or plain struct: an enum, trait, constant, static, macro
    /// call or inherent impl, or any item under a `cfg` or a non-standard
    /// derive or attribute. Real code lives next to it. A plain struct is the
    /// scaffolding an abandoned stub declares for its trait impls.
    real_items: bool,
}

/// The bodies and the item kinds of a Rust source file (masked by
/// [`crate::placeholder::code_only`], so a brace in a literal or comment cannot unbalance the
/// scan), found by a brace-matching scan of the items rather than a parse:
/// this call site only asks whether the bodies found are placeholders, never
/// compiles or lints the file. A signature with no body (a trait method
/// declaration, ending in `;`) is skipped rather than treated as a
/// placeholder or as disqualifying one. Functions nested in another
/// function's body are part of that body.
fn scan_source(masked: &str) -> Scan {
    let mut scan = Scan::default();
    scan_items(masked, &mut scan);
    scan
}

/// Whether the text of an attribute (inside `#[..]`) marks the item under it as
/// part of a finished file: conditional compilation, a derive that is not a
/// standard trait (an error or serde derive), or an attribute that is not
/// documentation, a lint level or a layout hint.
fn attribute_is_real(attr: &str) -> bool {
    let attr = attr.trim();
    let name_end = attr
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(attr.len());
    match &attr[..name_end] {
        "doc" | "allow" | "warn" | "deny" | "expect" | "must_use" | "repr" | "non_exhaustive"
        | "inline" => false,
        "derive" => attr[name_end..]
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
            .filter(|w| !w.is_empty())
            .any(|w| {
                !matches!(
                    w.rsplit(':').next(),
                    Some(
                        "Debug"
                            | "Clone"
                            | "Copy"
                            | "Default"
                            | "PartialEq"
                            | "Eq"
                            | "Hash"
                            | "PartialOrd"
                            | "Ord"
                    )
                )
            }),
        _ => true,
    }
}

/// The keyword that names the item starting `text`, after any visibility and
/// qualifiers (`pub(crate) unsafe extern "C" fn` gives `fn`). `extern crate`
/// gives `extern_crate` and an `extern` block `extern_block`.
fn item_keyword(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let word = &rest[..end];
        let after = rest[end..].trim_start();
        match word {
            "pub" => {
                rest = match after.strip_prefix('(') {
                    Some(inner) => inner.split_once(')').map_or("", |(_, r)| r).trim_start(),
                    None => after,
                };
            }
            "unsafe" | "async" | "default" | "safe" => rest = after,
            "const" => {
                let next = after.split(|c: char| !c.is_ascii_alphanumeric()).next();
                if matches!(next, Some("fn" | "unsafe" | "async" | "extern")) {
                    rest = after;
                } else {
                    return "const";
                }
            }
            "extern" => {
                // The ABI string is masked to spaces between its quotes.
                let after = match after.strip_prefix('"') {
                    Some(s) => s.split_once('"').map_or("", |(_, r)| r).trim_start(),
                    None => after,
                };
                if after.starts_with("crate") {
                    return "extern_crate";
                }
                if after.starts_with('{') {
                    return "extern_block";
                }
                rest = after;
            }
            _ => return word,
        }
    }
}

/// The byte of the first `{` or `;` outside `(..)` and `[..]` from `from`:
/// the end of an item header that is not a function signature.
fn header_end(content: &str, from: usize) -> Option<usize> {
    let mut nest = 0usize;
    for (k, &b) in content.as_bytes().iter().enumerate().skip(from) {
        match b {
            b'(' | b'[' => nest += 1,
            b')' | b']' => nest = nest.saturating_sub(1),
            b'{' | b';' if nest == 0 => return Some(k),
            _ => {}
        }
    }
    None
}

/// The byte after the `}` that closes the `{` at `open`.
fn block_end(content: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, &b) in content.as_bytes().iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether an `impl` header (`impl<T> Trait for Type`) names a trait: a
/// `for` that is a word of its own and does not open a higher-ranked bound
/// (`for<'a>`).
fn is_trait_impl(header: &str) -> bool {
    let b = header.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    header.match_indices("for").any(|(at, _)| {
        (at == 0 || !word(b[at - 1]))
            && b.get(at + 3).is_none_or(|&c| !word(c))
            && !header[at + 3..].trim_start().starts_with('<')
    })
}

fn scan_items(content: &str, scan: &mut Scan) {
    let bytes = content.as_bytes();
    let mut i = 0;
    // A shape this scan cannot follow is real code as far as it can tell: it
    // never lets a file read as a placeholder.
    let unparsed = |scan: &mut Scan| {
        scan.real_items = true;
        scan.free.push("<unparsed>".to_owned());
    };
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Attributes: `#[..]` and `#![..]`.
        if bytes[i] == b'#' {
            let mut j = i + 1;
            if bytes.get(j) == Some(&b'!') {
                j += 1;
            }
            while bytes.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            if bytes.get(j) == Some(&b'[') {
                let mut depth = 0usize;
                let mut k = j;
                while k < bytes.len() {
                    match bytes[k] {
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                if attribute_is_real(content.get(j + 1..k).unwrap_or("")) {
                    scan.real_items = true;
                }
                i = k + 1;
                continue;
            }
        }
        let kw = item_keyword(&content[i..]);
        let end = if kw == "fn" {
            let kw_at = content[i..].find("fn").map_or(i, |n| i + n);
            signature_end(content, kw_at + 2)
        } else {
            header_end(content, i)
        };
        let Some(end) = end else {
            unparsed(scan);
            return;
        };
        let header = &content[i..end];
        if bytes[end] == b';' {
            match kw {
                "use" | "type" | "mod" | "extern_crate" | "fn" | "struct" => {}
                _ => scan.real_items = true,
            }
            i = end + 1;
            continue;
        }
        let Some(after) = block_end(content, end) else {
            unparsed(scan);
            return;
        };
        let inner = &content[end + 1..after - 1];
        match kw {
            "fn" => scan.free.push(inner.to_owned()),
            "mod" => scan_items(inner, scan),
            "impl" => {
                let mut sub = Scan::default();
                scan_items(inner, &mut sub);
                if is_trait_impl(&header[header.find("impl").map_or(0, |n| n + 4)..]) {
                    scan.trait_impl.extend(sub.free);
                } else {
                    scan.real_items = true;
                    scan.free.extend(sub.free);
                }
            }
            "trait" => {
                scan.real_items = true;
                scan_items(inner, scan);
            }
            "struct" => {}
            _ => scan.real_items = true,
        }
        i = after;
        // `const X: T = { .. };` and a macro call `m! { .. };` end in `;`.
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i) == Some(&b';') {
            i += 1;
        }
    }
}

/// Whether [`is_placeholder_source`] can read `path`: a language kGraf has a
/// grammar for, prose excluded.
fn has_placeholder_reader(path: &str) -> bool {
    kgraf_graph::Language::from_path(path).is_some_and(|l| l != kgraf_graph::Language::Markdown)
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
        // Only a file in a language with a placeholder reader can be one: the
        // others are never read.
        .filter(|path| {
            has_placeholder_reader(path)
                && read(path).is_some_and(|content| is_placeholder_source(path, &content))
        })
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
        left.iter()
            .map(|p| plain_path(p))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A path as a reason prints it: control characters, line and paragraph
/// separators, and bidirectional overrides become `?`. A worker can name a
/// file with them, and the reason reaches a terminal and the next brief.
fn plain_path(path: &str) -> String {
    path.chars()
        .map(|c| {
            let altering = c.is_control()
                || matches!(
                    c,
                    '\u{061C}'
                        | '\u{200B}'..='\u{200F}'
                        | '\u{2028}'..='\u{202E}'
                        | '\u{2060}'..='\u{2064}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{FEFF}'
                );
            if altering { '?' } else { c }
        })
        .collect()
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
            kind: crate::checks::InterfaceKind::Behavior,
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
        // book's `landed_paths` lets the revision replace it.
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

    /// Cargo's and rustup's configuration is refused as a stub path before any
    /// worktree exists: it would land on the integration tip, where the
    /// host's own cargo reads it.
    #[tokio::test]
    async fn a_stub_writing_build_configuration_is_refused_and_never_lands() {
        for path in [".cargo/config.toml", "rust-toolchain.toml"] {
            let f = fixture();
            let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
            let contract = ic(
                "cfg",
                1,
                vec![FileOverlay::new(path, "[build]\nrustc-wrapper = \"x\"\n")],
            );
            let err = lander
                .land(&contract, &scope(&contract))
                .await
                .expect_err("refused");
            assert!(err.contains(path), "{err}");
            let tip = git_out(&f.root, &["rev-parse", "kgraf/r-1"]);
            assert_eq!(tip, git_out(&f.root, &["rev-parse", "main"]), "{path}");
        }
    }

    /// The tip holds a directory link `gen -> <outside>` and a leaf link
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

    /// A stub path the producer did not declare is refused, even one
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

    /// A stub path that already exists at the tip is refused unless the
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

    /// Crash window: the merge happened but `StubLanded` was never
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
        // the clobber check must be told the stub is allowed to replace it.
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

        // A conflicted attempt is kept for inspection, like a refused one,
        // until the end-of-run sweep removes it, its worktree and its ref.
        let kept = f.manager.base().join("stub-shared-v1");
        assert!(kept.exists(), "the conflicted worktree is kept");
        assert!(
            !git_out(&f.root, &["for-each-ref", "refs/kgraf/attempts/"]).is_empty(),
            "the conflicted attempt keeps its ref"
        );
        let swept = lander.sweep().expect("sweep");
        assert!(!kept.exists(), "removed at the end of the run");
        assert!(
            swept.iter().any(|s| s.contains("stub-shared-v1")),
            "{swept:?}"
        );
        assert_eq!(
            git_out(&f.root, &["for-each-ref", "refs/kgraf/attempts/"]),
            "",
            "no attempt ref remains"
        );
    }

    /// A merge-back failure (not a conflict or a refusal) is reported with
    /// the interface it was for and the cause. A ref lock held by another
    /// process makes the branch update fail on an unmoved tip.
    #[tokio::test]
    async fn a_merge_back_failure_names_the_interface_and_the_cause() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "locked",
            1,
            vec![FileOverlay::new(
                "src/config.rs",
                "pub fn parse() -> u16 { 0 }\n",
            )],
        );
        let lock = f.root.join(".git/refs/heads/kgraf/r-1.lock");
        std::fs::write(&lock, "").expect("lock");
        let err = lander
            .land(&contract, &scope(&contract))
            .await
            .expect_err("the branch update is refused");
        let lead = format!("cannot land the stub of {}: ", contract.label());
        assert!(err.starts_with(&lead), "{err}");
        assert!(err.len() > lead.len(), "the cause follows: {err}");
        assert_eq!(tip(&f), base(&f), "nothing reached the branch");
        std::fs::remove_file(&lock).expect("unlock");
        lander.sweep().expect("sweep");
    }

    /// A stub path that cannot be inspected for a reason other than "not
    /// there" is refused as uninspectable, not treated as absent and written
    /// later. A NUL byte in a component fails the inspection with an error
    /// that is not `NotFound` (`InvalidInput`).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stub_path_that_cannot_be_inspected_is_refused_not_treated_as_absent() {
        let f = fixture();
        let lander = GitStubLander::new(f.manager.clone(), f.merge_back.clone());
        let contract = ic(
            "nulname",
            1,
            vec![FileOverlay::new("a\0b.rs", "pub fn parse() -> u16 { 0 }\n")],
        );
        let err = lander
            .land(&contract, &scope(&contract))
            .await
            .expect_err("refused");
        assert!(err.contains("cannot inspect"), "{err}");
        lander.sweep().expect("sweep");
    }

    /// Revision: a revised version whose stub bytes did not change is
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

    /// A refused land keeps its worktree for inspection; the end-of-run
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

    /// An attempt ref a stub land left (a crash between commit and
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
    fn unimplemented_is_a_placeholder_too() {
        assert!(is_placeholder_source(
            "src/lib.rs",
            "fn a() { unimplemented!() }\nfn b() { todo!(\"later\"); }\n"
        ));
    }

    #[test]
    fn a_mock_made_of_unreachable_bodies_is_not_a_placeholder() {
        assert!(!is_placeholder_source(
            "src/lib.rs",
            "fn a() { unreachable!() }\nfn b() { unreachable!(\"never\") }\n"
        ));
    }

    #[test]
    fn a_placeholder_call_followed_by_real_work_is_not_a_placeholder() {
        for body in [
            "todo!(); real()",
            "unimplemented!(); real_work()",
            "todo!() + 1",
            "todo!(\"a\"); todo!(\"b\")",
            "todo!(x)(y)",
        ] {
            assert!(
                !is_placeholder_source("src/lib.rs", &format!("fn f() {{ {body} }}\n")),
                "{body}"
            );
        }
    }

    #[test]
    fn every_delimiter_and_path_form_of_a_placeholder_call_is_one() {
        for body in [
            "todo![]",
            "todo!{}",
            "unimplemented![]",
            "unimplemented!{ }",
            "std::todo!()",
            "core::unimplemented!(\"x\")",
            "::std::todo!{}",
            "todo ! ()",
            "return todo!()",
            "return std::todo!();",
            "todo![\"a\"];",
        ] {
            assert!(
                is_placeholder_source("src/lib.rs", &format!("fn f() {{ {body} }}\n")),
                "{body}"
            );
        }
        for body in [
            "unreachable![]",
            "std::unreachable!()",
            "returns todo!()",
            "my::todo!()",
            "todo!{} + 1",
        ] {
            assert!(
                !is_placeholder_source("src/lib.rs", &format!("fn f() {{ {body} }}\n")),
                "{body}"
            );
        }
    }

    /// A brace in a character, string, raw string or comment is not code: it
    /// must not unbalance the scan, or the real body after it is lost and a
    /// file with real work is refused as a placeholder.
    #[test]
    fn braces_in_literals_and_comments_do_not_unbalance_the_scan() {
        let real = [
            "fn a() { todo!() }\nfn b() -> char { '{' }\n",
            "fn a() { todo!() }\nfn b() -> char { '\\'' }\nfn c() -> &'static str { \"}}\" }\n",
            "fn a() { todo!() }\nfn b() -> &'static str { r#\"} \" }\"# }\n",
            "fn a() { todo!() }\nfn b() -> u8 { /* } { */ 1 } // }\n",
            "fn a() { todo!() }\nfn b() -> u8 { /* /* } */ } */ 2 }\n",
            "fn a() { todo!() }\nfn b() -> &'static str { \"fn c() { todo!() }\" }\n",
            "fn a() { todo!() }\nfn b() -> char { '\\u{7d}' }\n",
            "fn a() { todo!() }\nfn b<'a>(x: &'a str) -> &'a str { x }\n",
            "fn a() { todo!() }\nfn b() -> &'static [u8] { b\"}\" }\n",
        ];
        for src in real {
            assert!(!is_placeholder_source("src/lib.rs", src), "{src}");
        }
        // Text that only looks like code does not make a body real, and a
        // commented-out or quoted `fn` is not a body.
        for src in [
            "// fn real() -> u8 { 1 }\nfn a() { todo!() }\n",
            "fn a() { /* { */ todo!() /* } */ }\n",
            "fn a() { todo!(\"}\") }\nfn b() { unimplemented!() }\n",
        ] {
            assert!(is_placeholder_source("src/lib.rs", src), "{src}");
        }
    }

    #[test]
    fn the_placeholder_reason_prints_paths_without_control_or_bidi_characters() {
        let reason =
            placeholder_left_in_place_reason(&["src/\u{1b}[31ma\u{202E}.rs", "b\u{2028}.rs"]);
        assert!(
            !reason.contains('\u{1b}') && !reason.contains('\u{202E}'),
            "{reason:?}"
        );
        assert!(!reason.contains('\u{2028}'), "{reason:?}");
        assert!(reason.contains("src/?[31ma?.rs, b?.rs"), "{reason}");
    }

    #[test]
    fn a_paren_inside_a_message_does_not_end_the_call() {
        assert!(is_placeholder_source(
            "src/lib.rs",
            "fn f() { todo!(\"a) b ( \\\" c\") }\n"
        ));
    }

    /// `;` inside an array type is part of the signature, not its end: the
    /// body is found (a real one clears the file) and a declaration is
    /// still skipped.
    #[test]
    fn an_array_type_in_a_signature_does_not_end_it() {
        // False refuse before: the scan stopped at `;` in `[u8; 4]`, took the
        // rest for the next fn, and never saw the real body.
        assert!(!is_placeholder_source(
            "src/lib.rs",
            "fn a() { todo!() }\nfn f(x: [u8; 4]) -> u8 { x[0] }\n"
        ));
        // False miss before: the stub with the array parameter was skipped as a
        // declaration, so a file of only that stub was "no bodies".
        assert!(is_placeholder_source(
            "src/lib.rs",
            "fn f(x: [u8; 4]) -> [u8; 2] { todo!() }\n"
        ));
        assert!(is_placeholder_source(
            "src/lib.rs",
            "fn f<T: Into<[u8; 4]>>(x: T) where T: Copy { todo!() }\n"
        ));
        assert_eq!(
            scan_source("trait T { fn d(x: [u8; 4]); fn e(&self) -> u8 { 1 } }")
                .free
                .len(),
            1
        );
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

    /// A finished file with a mock or an adapter whose unused trait methods
    /// are `unimplemented!()` next to real items is not an abandoned stub.
    #[test]
    fn trait_impl_placeholders_next_to_real_items_are_not_a_placeholder() {
        for src in [
            "#[derive(Debug)]\npub enum E { A }\n\
             impl std::fmt::Display for E {\n    fn fmt(&self, f: &mut F) -> R { unimplemented!() }\n}\n",
            "#[derive(thiserror::Error, Debug)]\npub struct E { n: u8 }\n\
             impl Backend for E {\n    fn get(&self) { todo!() }\n}\n",
            "#[cfg(unix)]\npub struct Mock;\n\
             impl Backend for Mock {\n    fn get(&self) { todo!() }\n}\n",
            "#![cfg_attr(not(test), no_std)]\npub struct Mock;\n\
             impl Backend for Mock {\n    fn get(&self) { todo!() }\n}\n",
            "const N: usize = 4;\nimpl Backend for Mock { fn get(&self) { unimplemented!() } }\n",
            "macro_rules! m { () => {}; }\nm!();\n\
             impl Backend for Mock { fn get(&self) { todo!() } }\n",
            "#[cfg(target_arch = \"s390x\")]\nmod arch {\n    pub struct S;\n    \
             impl Backend for S { fn get(&self) { todo!() } }\n}\n",
        ] {
            assert!(!is_placeholder_source("src/lib.rs", src), "{src}");
        }
    }

    /// The same bodies with nothing real next to them, or free functions
    /// and inherent methods that are placeholders, are still an abandoned stub.
    #[test]
    fn abandoned_stubs_are_still_placeholders() {
        for src in [
            "use std::fmt;\ntype R = u8;\nmod inner;\n\
             impl Backend for Mock { fn get(&self) { todo!() } }\n",
            "pub struct Repo;\nimpl Repo {\n    pub fn new() -> Self { todo!() }\n}\n",
            "pub struct Repo;\nfn free() { unimplemented!() }\n\
             impl Backend for Repo { fn get(&self) { todo!() } }\n",
            "mod inner {\n    pub fn a() { todo!() }\n}\n",
            "trait T { fn d(&self); fn e(&self) { todo!() } }\n",
            "pub struct Mock { n: u8 }\n\
             impl<T: Clone> Backend for Mock {\n    fn get(&self) { todo!() }\n}\n",
            "#[derive(Debug, Clone)]\n/// A stub.\npub struct Mock;\n\
             impl Backend for Mock {\n    fn get(&self) { unimplemented!() }\n}\n",
            "pub struct Mock(u8);\nimpl Backend for Mock { fn get(&self) { todo!() } }\n",
        ] {
            assert!(is_placeholder_source("src/lib.rs", src), "{src}");
        }
        // One real body anywhere, in a trait impl too, clears the file.
        assert!(!is_placeholder_source(
            "src/lib.rs",
            "fn free() { todo!() }\nimpl Backend for Repo { fn get(&self) -> u8 { 1 } }\n"
        ));
    }

    /// A file over the scan cap is real code, and is never scanned.
    #[test]
    fn a_file_over_the_scan_cap_is_never_a_placeholder() {
        let big = format!("fn f() {{ todo!() }}\n{}", " ".repeat(MAX_SCANNED_BYTES));
        assert!(!is_placeholder_source("src/lib.rs", &big));
    }

    /// Only paths in a language with a placeholder reader are read at all.
    #[test]
    fn a_path_with_no_placeholder_reader_is_not_read_for_the_placeholder_check() {
        let read = |p: &str| -> Option<String> {
            assert!(p.ends_with(".rs") || p.ends_with(".py"), "{p} was read");
            Some(if p.ends_with(".rs") {
                "fn f() { todo!() }".into()
            } else {
                "def f():\n    raise NotImplementedError\n".into()
            })
        };
        let left = placeholder_writes_left_in_place(
            ["big.bin", "a.rs", "notes.md", "b.py", "run.sh"],
            read,
        );
        assert_eq!(left, vec!["a.rs", "b.py"]);
    }

    #[test]
    fn a_prose_or_unknown_file_is_never_flagged() {
        assert!(!is_placeholder_source("README.md", "todo!()"));
        assert!(!is_placeholder_source(
            "run.sh",
            "f() { echo not implemented; exit 1; }\n"
        ));
    }

    /// The left-in-place check reads every language `placeholder` lexes: a
    /// file whose every body is one placeholder statement is an abandoned
    /// scaffold. Before, only `.rs` files were read, so a Python, TypeScript,
    /// Go, Java, C#, Ruby, PHP or C scaffold left as it was passed.
    #[test]
    fn a_scaffold_in_every_language_with_a_grammar_is_flagged() {
        for (path, src) in [
            (
                "pkg/slug.py",
                "\"\"\"Slugs.\"\"\"\n\ndef slugify(s):\n    \"\"\"Doc.\"\"\"\n    raise NotImplementedError\n\n\ndef lower(s):\n    raise NotImplementedError(\"lower\")\n",
            ),
            (
                "src/slug.ts",
                "export function slugify(s: string): string {\n  throw new Error(\"not implemented\");\n}\nexport const lower = (s: string): string => {\n  throw new Error(\"Not implemented yet\");\n};\n",
            ),
            (
                "src/slug.js",
                "function slugify(s) { throw new Error('not implemented'); }\nmodule.exports = { slugify };\n",
            ),
            (
                "slug/slug.go",
                "package slug\n\nfunc Slugify(s string) string {\n\tpanic(\"not implemented\")\n}\n",
            ),
            (
                "src/main/java/a/Slug.java",
                "package a;\npublic class Slug {\n  public static String slugify(String s) {\n    throw new UnsupportedOperationException(\"Not implemented\");\n  }\n}\n",
            ),
            (
                "Slug/Slug.cs",
                "namespace A {\n  public class Slug {\n    public string Slugify(string s) {\n      throw new NotImplementedException();\n    }\n    public string Lower(string s) => throw new System.NotImplementedException();\n  }\n}\n",
            ),
            (
                "lib/slug.rb",
                "module Slug\n  def self.slugify(s)\n    raise NotImplementedError\n  end\nend\n",
            ),
            (
                "src/Slug.php",
                "<?php\nfunction slugify($s) {\n  throw new Exception(\"not implemented\");\n}\n",
            ),
            (
                "src/slug.cpp",
                "int slugify(const char *s) {\n  throw std::logic_error(\"not implemented\");\n}\n",
            ),
        ] {
            assert!(is_placeholder_source(path, src), "{path}: {src}");
        }
    }

    /// Correct files in each language are never flagged: one real body, an
    /// abstract method, a no-op body, a file with no function, a placeholder
    /// named only in text.
    #[test]
    fn a_correct_file_in_any_language_is_never_flagged() {
        for (path, src) in [
            (
                "pkg/slug.py",
                "def slugify(s):\n    raise NotImplementedError\n\n\ndef lower(s):\n    return s.lower()\n",
            ),
            (
                "pkg/shape.py",
                "from abc import ABC, abstractmethod\n\n\nclass Shape(ABC):\n    @abstractmethod\n    def area(self):\n        raise NotImplementedError\n",
            ),
            (
                "pkg/base.py",
                "class Base:\n    def run(self):\n        raise NotImplementedError(\"subclasses override run\")\n",
            ),
            (
                "pkg/proto.py",
                "class P:\n    def run(self):\n        ...\n",
            ),
            ("pkg/noop.py", "def hook():\n    pass\n"),
            ("pkg/consts.py", "LIMIT = 3\n"),
            (
                "pkg/doc.py",
                "def brief():\n    return \"raise NotImplementedError is not implemented\"\n",
            ),
            (
                "src/slug.ts",
                "export function slugify(s: string): string {\n  if (!s) { throw new Error(\"not implemented for empty\"); }\n  return s.toLowerCase();\n}\n",
            ),
            (
                "slug/slug.go",
                "package slug\n\nfunc Slugify(s string) string {\n\treturn s\n}\n",
            ),
            (
                "Slug/Slug.cs",
                "namespace A { public class Slug { public string Slugify(string s) { return s; } } }\n",
            ),
            (
                "lib/shape.rb",
                "class Shape\n  def area\n    raise NotImplementedError\n  end\nend\nclass Circle < Shape\n  def area\n    3\n  end\nend\n",
            ),
        ] {
            assert!(!is_placeholder_source(path, src), "{path}: {src}");
        }
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
