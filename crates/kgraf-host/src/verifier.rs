//! The acceptance check of one attempt. Moved from `kgraf-cli`'s
//! `orchestrate_cmd.rs`. The order of its steps matters (see
//! [`CommandVerifier`]).

use crate::gates::run_confined;
use crate::workspaces::GitWorkspaces;
use kgraf_orchestrator::{
    OpenContract, Verdict, VerifiedCommit, Verifier, placeholder_left_in_place_reason,
    placeholder_writes_left_in_place,
};
use kgraf_runtime::{WorkerResult, forbidden_path, is_build_config_path, same_path};
use kgraf_sandbox::{CommitOutcome, Excluded, GeneratedKind, Leftovers, lockfile_manifests};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Verifies an attempt in these steps, none of which runs host git in the
/// worker's tree or runs worker code outside a sandbox:
///
/// 1. no process the worker started may still run in the worktree (the
///    sandbox hold and session check, `WorkspaceManager::quiesce`): a tree
///    something still writes is not final;
/// 2. the worktree is committed through merge-back (`SafeGit`: hooks and
///    fsmonitor off, attributes from a trusted tree, the gitlink verified,
///    protected paths refused, a moved HEAD refused). The commit is the
///    snapshot of what the worker changed, and the only thing ever merged. A
///    lockfile or Python bytecode the worker's own run generated outside its
///    scope is left out of it and reported ("made by the worker");
/// 3. every path the commit changes against the dispatch base must be one the
///    contract declared, compared as a case- and normalization-insensitive
///    volume resolves names, and none may be repository metadata or cargo's
///    or rustup's configuration ([`build_config_changes`]);
/// 4. the acceptance command runs **inside the sandbox** of that worktree
///    (writes confined to the worktree and its scratch directory, network off,
///    kGraf's state unreadable, the same policy as the worker's shell),
///    bounded by the gate timeout; when its shell exits, its process group is
///    killed;
/// 5. the worktree must then be quiet again (a process that left the group is
///    found by the hold check and fails the verification);
/// 6. what the command left is removed, descriptor-relative and never through
///    a symlink (`MergeBack::discard_leftovers`). New files are reported as
///    notices and never merged. A committed file the command changed fails the
///    verification: the merged tree would differ from the tree it checked.
///
/// The commit comes before the check. With the check first, the commit swept
/// up whatever the check wrote, so the scope check refused the worker for a
/// file its check generated ("changes outside the declared write scope:
/// [\"Cargo.lock\"]"). Running the check on a throwaway worktree of the commit
/// was the other option, rejected: it loses the worker's warm build directory
/// and the ignored environment directories (`node_modules`, `.venv`) a check
/// may need, so a check that passes in the worker's tree could fail there for
/// reasons that are not the work; the quality gate already gives the
/// clean-checkout judgment on the merged tree.
///
/// **Known limit: worker-controlled caches are check inputs.** The check sees
/// the worker's scratch directory (`CARGO_TARGET_DIR` at `<scratch>/target`,
/// Python bytecode at `<scratch>/pycache`) and ignored files in the tree, all of
/// which the worker's run could have written. A worker can therefore make its
/// own check pass with build output that does not match the committed
/// sources. This verdict selects among attempts; only the quality gate, which
/// runs the project's commands on a clean checkout of the merged commit,
/// judges the merged tree.
pub(crate) struct CommandVerifier {
    pub(crate) command: String,
    pub(crate) repo: PathBuf,
    pub(crate) workspaces: Arc<GitWorkspaces>,
    pub(crate) merge_back: Arc<kgraf_sandbox::MergeBack>,
    pub(crate) read_denies: Vec<PathBuf>,
    /// The check is killed after this long (the configured gate timeout).
    pub(crate) timeout: Duration,
}

/// Tries of the hold check, and the pause between them: a process that has
/// been killed can still hold the hold for a few milliseconds.
const QUIESCE_TRIES: u32 = 40;
const QUIESCE_PAUSE: Duration = Duration::from_millis(25);

/// `WorkspaceManager::quiesce`, retried for up to about one second while the
/// worktree reads busy. Any other error, or busy after the last try, is
/// returned.
fn quiesce_retrying(
    manager: &kgraf_sandbox::WorkspaceManager,
    ws: &kgraf_sandbox::Workspace,
) -> Result<kgraf_sandbox::QuietGuard, kgraf_sandbox::WorkspaceError> {
    let mut tries = 0;
    loop {
        match manager.quiesce(ws) {
            Err(kgraf_sandbox::WorkspaceError::Busy { .. }) if tries + 1 < QUIESCE_TRIES => {
                tries += 1;
                std::thread::sleep(QUIESCE_PAUSE);
            }
            other => return other,
        }
    }
}

/// Output a failed check's reason keeps, from the end.
const CHECK_OUTPUT_CAP: usize = 1_000;

/// An accepted no-op attempt's own notice (decision 0086): every attempt
/// verified with `Integration::Empty` is reported, not only traced, so a run
/// that keeps "passing" without changing anything stays visible without
/// reading the trace.
const EMPTY_INTEGRATION_NOTICE: &str = "this attempt integrated no change: its check passed against the worktree exactly as it was \
     dispatched";

/// Reads one declared write target from the still-live worktree, for the
/// placeholder check below. This only ever runs on an attempt merge-back
/// reported `CommitOutcome::Empty`, so the worktree's content at this path is
/// exactly what it held at dispatch, changed or not.
fn read_write_target(ws_root: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(ws_root.join(path)).ok()
}

fn declared(contract: &OpenContract, p: &Path) -> bool {
    contract
        .writes
        .iter()
        .any(|w| same_path(Path::new(&w.path), p))
}

pub(crate) fn scope_violations(changed: &[String], contract: &OpenContract) -> Vec<String> {
    changed
        .iter()
        .filter(|p| {
            let p = Path::new(p.as_str());
            forbidden_path(p) || !declared(contract, p)
        })
        .cloned()
        .collect()
}

/// The changed paths that are cargo's or rustup's configuration
/// ([`is_build_config_path`]), declared or not. Such a change is never
/// merged: once in the checkout, the host's own cargo reads it (the
/// dependency fetch of decision 0087 runs with the checkout's
/// configuration), and so does every later worker's build. Decision 0084's
/// tripwire already refuses builds in the worktree that changed them; this
/// refuses the attempt itself, so the change never reaches the checkout.
pub(crate) fn build_config_changes(changed: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|p| is_build_config_path(Path::new(p.as_str())))
        .cloned()
        .collect()
}

/// The verdict's words for [`build_config_changes`], for the model and the
/// user alike.
pub(crate) fn build_config_reason(paths: &[String]) -> String {
    format!(
        "the attempt changes cargo or rustup configuration ({paths:?}), which kGraf never \
         merges: a `.cargo` folder or a `rust-toolchain` file changes what every later build, \
         and the host's own cargo, runs. Leave these files as they are and solve the task \
         without them; if the project needs such a change, say so in your result so the user \
         can make it by hand"
    )
}

/// Paths named in a notice, at most this many, then a count.
const NOTICE_PATHS: usize = 5;

fn some_paths(paths: &[&Path]) -> String {
    let mut text = paths
        .iter()
        .take(NOTICE_PATHS)
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > NOTICE_PATHS {
        text.push_str(&format!(", and {} more", paths.len() - NOTICE_PATHS));
    }
    text
}

/// The owner's notices for what the worker's own run generated and the commit
/// left out: one per lockfile, one for all bytecode.
pub(crate) fn worker_notices(excluded: &[Excluded]) -> Vec<String> {
    let mut notices = Vec::new();
    let mut bytecode = Vec::new();
    for e in excluded {
        match e.kind {
            GeneratedKind::Lockfile => notices.push(format!(
                "the worker's run generated {} (made by the worker), which the project does not \
                 commit; it was left out of the attempt. Commit it in the project to make builds \
                 reproducible",
                e.path.display()
            )),
            GeneratedKind::PythonBytecode => bytecode.push(e.path.as_path()),
        }
    }
    if !bytecode.is_empty() {
        notices.push(format!(
            "the worker's run left {} Python bytecode file(s) (made by the worker): {}; they were \
             left out of the attempt and removed before the check. Add __pycache__/ to .gitignore",
            bytecode.len(),
            some_paths(&bytecode)
        ));
    }
    notices
}

/// The owner's notices for the files the check created: one per lockfile (the
/// owner acts on each), one for the rest. Paths the commit already left out as
/// the worker's (an excluded lockfile stays for the check) are reported once,
/// by [`worker_notices`].
pub(crate) fn check_notices(command: &str, left: &Leftovers, excluded: &[Excluded]) -> Vec<String> {
    let mut notices = Vec::new();
    let created: Vec<&Path> = left
        .created
        .iter()
        .map(PathBuf::as_path)
        .filter(|p| !excluded.iter().any(|e| e.path == *p))
        .collect();
    let (locks, other): (Vec<&Path>, Vec<&Path>) = created
        .into_iter()
        .partition(|p| lockfile_manifests(p).is_some());
    for lock in locks {
        notices.push(format!(
            "the check `{command}` generated {}, which the project does not commit; it was not \
             merged. Commit it in the project to make builds reproducible",
            lock.display()
        ));
    }
    if !other.is_empty() {
        notices.push(format!(
            "the check `{command}` created {} file(s) that git would stage: {}; they were not \
             merged. Add them to .gitignore, or commit them in the project",
            other.len(),
            some_paths(&other)
        ));
    }
    notices
}

/// A check that changed committed files checked a tree that is not the one
/// that would be merged.
fn changed_reason(command: &str, changed: &[PathBuf]) -> String {
    let paths: Vec<&Path> = changed.iter().map(PathBuf::as_path).collect();
    format!(
        "the check `{command}` changed or deleted {} committed file(s): {}; the merged tree would \
         differ from the tree it checked. The files were restored; make the check leave committed \
         files alone, or commit what it writes",
        paths.len(),
        some_paths(&paths)
    )
}

#[async_trait::async_trait]
impl Verifier for CommandVerifier {
    async fn verify(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        _result: &WorkerResult,
    ) -> Result<Verdict, String> {
        let Some(ws) = self.workspaces.get(workspace) else {
            return Err(format!(
                "{} is not a live worktree of this run",
                workspace.display()
            ));
        };
        let command = self.command.clone();
        let denies = self.read_denies.clone();
        let merge_back = self.merge_back.clone();
        let contract = contract.clone();
        let workspaces = self.workspaces.clone();
        let attempt_root = workspace.to_path_buf();
        let timeout = self.timeout;
        tokio::task::spawn_blocking(move || {
            let manager = workspaces.manager.clone();
            let root = attempt_root.clone();
            // 1. Nothing the worker started still runs; held through the commit.
            // A process that dropped its descriptors and left its session is
            // not seen off Linux PID namespaces (decision 0084, known gap);
            // what it writes after the commit below is not in the attempt.
            let quiet = match quiesce_retrying(&manager, &ws) {
                Ok(q) => q,
                Err(e) => {
                    return Ok(Verdict::Fail {
                        reason: format!(
                            "a process the worker started still runs in its worktree, so the \
                             tree is not final: {e}"
                        ),
                    });
                }
            };
            // 2. The snapshot: commit through merge-back, before the check can
            // write anything. An empty attempt keeps its worktree for the check.
            // Numbered per verification from the record's persisted counter: a
            // worktree verified again never collides with an earlier attempt ref.
            let mut attempt =
                match merge_back.attempt_numbered(ws.clone(), contract.verifications.max(1)) {
                    Ok(a) => a,
                    Err(e) => {
                        return Ok(Verdict::Fail {
                            reason: format!("merge-back: {e}"),
                        });
                    }
                };
            let message = format!("kgraf: contract {}", contract.contract);
            let in_scope = |p: &Path| declared(&contract, p);
            let commit = match merge_back.commit_retaining(&mut attempt, &message, &in_scope) {
                Ok(CommitOutcome::Committed { commit }) => Some(commit),
                Ok(CommitOutcome::Empty) => None,
                Ok(CommitOutcome::Refused(r)) => {
                    return Ok(Verdict::Fail {
                        reason: format!("merge-back refused the attempt: {:?}", r.reasons),
                    });
                }
                Err(e) => {
                    return Ok(Verdict::Fail {
                        reason: format!("merge-back could not commit the attempt: {e}"),
                    });
                }
            };
            drop(quiet);
            // A committed attempt that fails from here on is kept for discard.
            let fail = |attempt, reason: String| {
                if commit.is_some() {
                    workspaces.committed().insert(root.clone(), attempt);
                }
                Ok(Verdict::Fail { reason })
            };
            // 3. Scope, base to the snapshot: only what the worker changed.
            if commit.is_some() {
                let changed = match merge_back.changed_paths(&attempt) {
                    Ok(c) => c,
                    Err(e) => {
                        workspaces.committed().insert(root.clone(), attempt);
                        return Err(format!("cannot read the attempt's changes: {e}"));
                    }
                };
                let outside = scope_violations(&changed, &contract);
                if !outside.is_empty() {
                    // The check does not run on a refused attempt.
                    return fail(
                        attempt,
                        format!("changes outside the declared write scope: {outside:?}"),
                    );
                }
                let config = build_config_changes(&changed);
                if !config.is_empty() {
                    return fail(attempt, build_config_reason(&config));
                }
            }
            if let Err(e) = merge_back.remove_excluded_bytecode(&attempt) {
                return fail(
                    attempt,
                    format!("cannot remove the bytecode the worker's run left: {e}"),
                );
            }
            // 4. The acceptance command, confined, in the worker's tree; its
            // process group is killed when its shell exits.
            let (passed, output) = run_confined(
                &ws,
                manager.repo(),
                &denies,
                &command,
                timeout,
                CHECK_OUTPUT_CAP,
            )?;
            // 5. Quiet again, held through the cleanup. A process that escaped
            // the group fails the verification and nothing is cleaned: the
            // worktree is left as it is, never merged, never continued.
            let quiet = match quiesce_retrying(&manager, &ws) {
                Ok(q) => q,
                Err(e) => {
                    return Ok(Verdict::Fail {
                        reason: format!(
                            "the check `{command}` left a process running in the worktree after \
                             its process group was killed: {e}"
                        ),
                    });
                }
            };
            // 6. What the check wrote: removed whether it passed or failed (a
            // worktree kept for a continued worker must not carry it).
            let cleared = merge_back.discard_leftovers(&attempt);
            drop(quiet);
            if !passed {
                let mut reason = format!("`{command}` failed: {output}");
                if let Err(e) = &cleared {
                    reason.push_str(&format!("; also, what it left could not be removed: {e}"));
                }
                // The worker's files stay in the worktree for inspection (its
                // attempt ref is swept at the end of the run); a failed
                // attempt is never continued in it.
                return Ok(Verdict::Fail { reason });
            }
            let left = match cleared {
                Ok(left) => left,
                Err(e) => {
                    return fail(
                        attempt,
                        format!("cannot remove what `{command}` left in the worktree: {e}"),
                    );
                }
            };
            if !left.changed.is_empty() {
                return fail(attempt, changed_reason(&command, &left.changed));
            }
            let mut notices = worker_notices(attempt.excluded());
            notices.extend(check_notices(&command, &left, attempt.excluded()));
            let Some(commit) = commit else {
                // The general-path stub refusal (decision 0086, generalizing
                // 0081's interface-only `stub_left_in_place`): an empty
                // attempt's declared write targets are byte-identical to
                // dispatch by construction (nothing changed at all), so this
                // asks what that unchanged content *is*, not whether it
                // changed. A target still holding nothing but a placeholder
                // body is refused rather than passed, before the worktree is
                // removed.
                let write_paths: Vec<&str> =
                    contract.writes.iter().map(|w| w.path.as_str()).collect();
                let left_as_stub = placeholder_writes_left_in_place(write_paths, |p| {
                    read_write_target(ws.root(), p)
                });
                if !left_as_stub.is_empty() {
                    let reason = placeholder_left_in_place_reason(&left_as_stub);
                    manager.remove(ws).map_err(|e| {
                        format!("cannot remove the refused empty attempt's worktree: {e}")
                    })?;
                    return Ok(Verdict::Fail { reason });
                }
                // Nothing to merge: the worktree goes, as merge-back's plain
                // commit would have removed it.
                manager
                    .remove(ws)
                    .map_err(|e| format!("cannot remove the empty attempt's worktree: {e}"))?;
                workspaces
                    .empty
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(root);
                notices.push(EMPTY_INTEGRATION_NOTICE.to_owned());
                return Ok(Verdict::Pass {
                    by: command,
                    commit: None,
                    notices,
                });
            };
            // Kept for merge (the selected one) or discard (the rest).
            workspaces.committed().insert(root, attempt);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(Verdict::Pass {
                by: command.clone(),
                commit: Some(VerifiedCommit {
                    sha: commit,
                    verified_by: command,
                    verified_ms: now,
                }),
                notices,
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// The claim is checked against the dispatching repository (never a
    /// worker's tree): the commit must exist. The command is not re-run on
    /// resume.
    async fn recheck(&self, commit: &VerifiedCommit) -> Result<bool, String> {
        let repo = self.repo.clone();
        let spec = format!("{}^{{commit}}", commit.sha);
        tokio::task::spawn_blocking(move || {
            let git = kgraf_sandbox::git::Git::discover().map_err(|e| e.to_string())?;
            Ok(git.run(&repo, ["cat-file", "-e", spec.as_str()]).is_ok())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}
